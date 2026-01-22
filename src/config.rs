use serde::Serialize;
use std::collections::HashMap;
use std::time::Duration;

/// Parsed shebang from the first line of a script
#[derive(Debug, Clone, Serialize)]
pub struct Shebang {
    /// the interpreter path (e.g., "/usr/bin/env", "/bin/bash")
    pub interpreter: String,
    /// arguments to the interpreter (e.g., ["python3"] for "#!/usr/bin/env python3")
    pub args: Vec<String>,
}

impl Shebang {
    /// parse a shebang line like "#!/usr/bin/env python3" or "#!/bin/bash"
    pub fn parse(line: &str) -> Option<Self> {
        let line = line.trim();
        if !line.starts_with("#!") {
            return None;
        }

        let content = line[2..].trim();
        let mut parts = content.split_whitespace();
        let interpreter = parts.next()?.to_string();
        let args: Vec<String> = parts.map(|s| s.to_string()).collect();

        Some(Shebang { interpreter, args })
    }
}

#[cfg(test)]
mod shebang_tests {
    use super::*;

    #[test]
    fn test_parse_env_python() {
        let shebang = Shebang::parse("#!/usr/bin/env python3").unwrap();
        assert_eq!(shebang.interpreter, "/usr/bin/env");
        assert_eq!(shebang.args, vec!["python3"]);
    }

    #[test]
    fn test_parse_direct_bash() {
        let shebang = Shebang::parse("#!/bin/bash").unwrap();
        assert_eq!(shebang.interpreter, "/bin/bash");
        assert!(shebang.args.is_empty());
    }

    #[test]
    fn test_parse_with_flags() {
        let shebang = Shebang::parse("#!/usr/bin/env -S uv run --script").unwrap();
        assert_eq!(shebang.interpreter, "/usr/bin/env");
        assert_eq!(shebang.args, vec!["-S", "uv", "run", "--script"]);
    }

    #[test]
    fn test_parse_not_shebang() {
        assert!(Shebang::parse("echo hello").is_none());
        assert!(Shebang::parse("# just a comment").is_none());
    }

    #[test]
    fn test_parse_trimmed() {
        let shebang = Shebang::parse("  #!/bin/sh  ").unwrap();
        assert_eq!(shebang.interpreter, "/bin/sh");
    }
}

/// File transfer specification (local:remote path pair)
#[derive(Debug, Clone, Serialize)]
pub struct FileTransfer {
    pub local: String,
    pub remote: String,
}

/// Service kind - managed (we start/stop) or external (just wait for readiness)
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum ServiceKind {
    Managed,
    External,
}

/// Readiness check type for services
#[derive(Debug, Clone, Serialize)]
pub enum ReadinessCheck {
    Http { url: String },
    Tcp { host: String, port: u16 },
    Command { cmd: String },
}

impl ReadinessCheck {
    pub fn parse(s: &str) -> Option<Self> {
        if s.starts_with("http://") || s.starts_with("https://") {
            Some(ReadinessCheck::Http { url: s.to_string() })
        } else if let Some(rest) = s.strip_prefix("tcp:") {
            let (host, port) = rest.rsplit_once(':')?;
            Some(ReadinessCheck::Tcp {
                host: host.to_string(),
                port: port.parse().ok()?,
            })
        } else if let Some(rest) = s.strip_prefix("cmd:") {
            let cmd = rest.trim_matches('"').to_string();
            Some(ReadinessCheck::Command { cmd })
        } else {
            None
        }
    }

    /// extract host/port for env var injection
    pub fn host_port(&self) -> Option<(String, u16)> {
        match self {
            ReadinessCheck::Http { url } => {
                let url = url::Url::parse(url).ok()?;
                let host = url.host_str()?.to_string();
                let port = url.port().unwrap_or(if url.scheme() == "https" { 443 } else { 80 });
                Some((host, port))
            }
            ReadinessCheck::Tcp { host, port } => Some((host.clone(), *port)),
            ReadinessCheck::Command { .. } => None,
        }
    }

    /// get base URL for HTTP checks (for env injection)
    pub fn base_url(&self) -> Option<String> {
        match self {
            ReadinessCheck::Http { url } => {
                let parsed = url::Url::parse(url).ok()?;
                let port_str = parsed.port().map(|p| format!(":{}", p)).unwrap_or_default();
                Some(format!("{}://{}{}", parsed.scheme(), parsed.host_str()?, port_str))
            }
            _ => None,
        }
    }

    /// get port for any HTTP/TCP check (for auto-tunneling)
    pub fn port(&self) -> Option<u16> {
        self.host_port().map(|(_, port)| port)
    }

    /// create a modified check that targets a different host/port (for tunneling)
    pub fn with_tunnel(&self, local_port: u16) -> Self {
        match self {
            ReadinessCheck::Http { url } => {
                if let Ok(mut parsed) = url::Url::parse(url) {
                    let _ = parsed.set_host(Some("127.0.0.1"));
                    let _ = parsed.set_port(Some(local_port));
                    ReadinessCheck::Http { url: parsed.to_string() }
                } else {
                    self.clone()
                }
            }
            ReadinessCheck::Tcp { .. } => ReadinessCheck::Tcp {
                host: "127.0.0.1".to_string(),
                port: local_port,
            },
            ReadinessCheck::Command { cmd } => ReadinessCheck::Command { cmd: cmd.clone() },
        }
    }
}

/// Log output mode for services
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub enum LogOutput {
    #[default]
    Stream,
    Quiet,
}

/// Service configuration
#[derive(Debug, Clone, Serialize)]
pub struct ServiceConfig {
    pub kind: ServiceKind,
    pub ready: Option<ReadinessCheck>,
    pub startup_timeout: Duration,
    pub shutdown_grace: Duration,
    pub shutdown_kill: Duration,
    pub interval: Duration,
    pub log: LogOutput,
    /// tunnel readiness checks through SSH for remote services
    pub forward: bool,
    /// command to run before starting the service (validation/preflight check)
    pub preflight: Option<String>,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            kind: ServiceKind::Managed,
            ready: None,
            startup_timeout: Duration::from_secs(60),
            shutdown_grace: Duration::from_secs(5),
            shutdown_kill: Duration::from_secs(10),
            interval: Duration::from_secs(1),
            log: LogOutput::Stream,
            forward: false,
            preflight: None,
        }
    }
}

/// Dotenv settings for the config file
#[derive(Debug, Clone, Default)]
pub struct DotenvSettings {
    pub load: bool,
    pub paths: Vec<String>,
    pub required: bool,
}

/// SSH connection configuration for remote execution
#[derive(Debug, Clone, Default, Serialize)]
pub struct SshConfig {
    /// hostname or SSH config alias
    pub host: String,
    /// optional username (defaults to current user)
    pub user: Option<String>,
    /// optional port (defaults to 22)
    pub port: Option<u16>,
    /// optional identity file path
    pub identity: Option<String>,
    /// working directory on remote host
    pub workdir: Option<String>,
    /// files to upload before task runs
    pub upload: Vec<FileTransfer>,
    /// files to download after task completes
    pub download: Vec<FileTransfer>,
}

impl SshConfig {
    pub fn destination(&self) -> String {
        match &self.user {
            Some(user) => format!("{}@{}", user, self.host),
            None => self.host.clone(),
        }
    }
}

/// Kubernetes execution mode
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum K8sMode {
    /// exec into an existing pod (kubectl exec)
    Exec,
    /// create an ephemeral Job that runs to completion
    Job,
    /// apply manifests from a path (kubectl apply -f)
    Apply,
}

impl Default for K8sMode {
    fn default() -> Self {
        K8sMode::Job
    }
}

/// ConfigMap or Secret mount specification
#[derive(Debug, Clone, Serialize)]
pub struct ConfigMount {
    pub name: String,
    pub mount_path: String,
}

/// Port forward specification for kubectl port-forward
#[derive(Debug, Clone, Serialize)]
pub struct PortForward {
    /// local port to listen on
    pub local_port: u16,
    /// remote port on the pod/service
    pub remote_port: u16,
    /// optional resource type (pod, svc, deployment) - defaults to pod
    pub resource_type: Option<String>,
    /// resource name (pod name, service name, etc.) or selector
    pub resource: String,
}

/// Kubernetes configuration for remote execution
#[derive(Debug, Clone, Serialize)]
pub struct K8sConfig {
    /// execution mode
    pub mode: K8sMode,
    /// kubernetes context (from kubeconfig), defaults to current
    pub context: Option<String>,
    /// namespace (defaults to "default")
    pub namespace: String,

    // -- Exec mode fields --
    /// pod selector (label selector like "app=myapp")
    pub selector: Option<String>,
    /// specific pod name (alternative to selector)
    pub pod: Option<String>,
    /// container name within pod (for multi-container pods)
    pub container: Option<String>,

    // -- Job mode fields --
    /// container image for ephemeral jobs
    pub image: Option<String>,
    /// resource requests/limits
    pub cpu: Option<String>,
    pub memory: Option<String>,
    /// node selector (key:value pairs)
    pub node_selector: Option<HashMap<String, String>>,
    /// tolerations (just the key names)
    pub tolerations: Vec<String>,
    /// service account name
    pub service_account: Option<String>,
    /// TTL seconds after job finishes (for cleanup)
    pub ttl_seconds: Option<u32>,

    // -- Apply mode fields --
    /// path to manifests folder or file
    pub path: Option<String>,
    /// resources to wait for (deployment/foo, statefulset/bar)
    pub wait_for: Vec<String>,
    /// timeout for waiting on resources
    #[serde(serialize_with = "serialize_duration_opt", skip_serializing_if = "Option::is_none")]
    pub wait_timeout: Option<Duration>,

    // -- File transfer --
    /// files to upload before task (local:remote)
    pub upload: Vec<FileTransfer>,
    /// files to download after task (remote:local)
    pub download: Vec<FileTransfer>,
    /// configmaps to mount (name:/path)
    pub configmaps: Vec<ConfigMount>,
    /// secrets to mount (name:/path)
    pub secrets: Vec<ConfigMount>,

    // -- Port forwarding --
    /// port forwards to set up before task execution
    pub forwards: Vec<PortForward>,

    /// working directory inside container
    pub workdir: Option<String>,
}

impl Default for K8sConfig {
    fn default() -> Self {
        Self {
            mode: K8sMode::Job,
            context: None,
            namespace: "default".to_string(),
            selector: None,
            pod: None,
            container: None,
            image: None,
            cpu: None,
            memory: None,
            node_selector: None,
            tolerations: vec![],
            service_account: None,
            ttl_seconds: Some(300),
            path: None,
            wait_for: vec![],
            wait_timeout: None,
            upload: vec![],
            download: vec![],
            configmaps: vec![],
            secrets: vec![],
            forwards: vec![],
            workdir: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Task {
    pub name: String,
    pub run: Option<String>,
    pub depends_on: Vec<String>,
    /// dependencies that are services (prefixed with service: in the file)
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub service_deps: Vec<String>,
    pub pipe_from: Vec<String>,
    #[serde(serialize_with = "serialize_duration_opt", skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Duration>,
    pub retry: u32,
    pub join: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub k8s: Option<K8sConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<ServiceConfig>,
    /// parsed shebang if first line of script starts with #!
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shebang: Option<Shebang>,
}

fn serialize_duration_opt<S>(dur: &Option<Duration>, s: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match dur {
        Some(d) => s.serialize_str(&humantime::format_duration(*d).to_string()),
        None => s.serialize_none(),
    }
}

impl Task {
    pub fn is_join(&self) -> bool {
        self.join || self.run.is_none()
    }

    #[allow(dead_code)]
    pub fn is_remote(&self) -> bool {
        self.ssh.is_some()
    }
}

#[derive(Debug)]
pub struct Config {
    pub tasks: HashMap<String, Task>,
    pub dotenv: DotenvSettings,
}

impl Config {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Config {
            tasks: HashMap::new(),
            dotenv: DotenvSettings::default(),
        }
    }

    /// get all services (tasks with service config)
    #[allow(dead_code)]
    pub fn services(&self) -> impl Iterator<Item = &Task> {
        self.tasks.values().filter(|t| t.service.is_some())
    }
}
