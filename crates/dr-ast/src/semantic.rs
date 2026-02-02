//! Semantic types for dagrun - these are the runtime types used by the executor.
//! They carry optional spans for LSP support.

use crate::Span;
use serde::Serialize;
use std::collections::HashMap;
use std::time::Duration;

/// A parsed dagrun configuration file
#[derive(Debug, Clone)]
pub struct Config {
    pub tasks: HashMap<String, Task>,
    pub dotenv: DotenvSettings,
}

impl Config {
    pub fn new() -> Self {
        Config {
            tasks: HashMap::new(),
            dotenv: DotenvSettings::default(),
        }
    }

    pub fn services(&self) -> impl Iterator<Item = &Task> {
        self.tasks.values().filter(|t| t.service.is_some())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}

/// Dotenv settings
#[derive(Debug, Clone, Default)]
pub struct DotenvSettings {
    pub load: bool,
    pub paths: Vec<String>,
    pub required: bool,
}

/// A task/recipe definition
#[derive(Debug, Clone, Serialize)]
pub struct Task {
    pub name: String,
    /// Task parameters (positional arguments)
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<TaskParameter>,
    pub run: Option<String>,
    pub depends_on: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub service_deps: Vec<String>,
    pub pipe_from: Vec<String>,
    #[serde(
        serialize_with = "serialize_duration_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub timeout: Option<Duration>,
    pub retry: u32,
    pub join: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub k8s: Option<K8sConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<ServiceConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shebang: Option<Shebang>,
    /// span of the task definition (for LSP)
    #[serde(skip)]
    pub span: Option<Span>,
}

impl Task {
    pub fn is_join(&self) -> bool {
        self.join || self.run.is_none()
    }

    pub fn is_remote(&self) -> bool {
        self.ssh.is_some()
    }
}

/// Task parameter definition
#[derive(Debug, Clone, Serialize)]
pub struct TaskParameter {
    pub name: String,
    /// None = required, Some = optional with default
    pub default: Option<String>,
    #[serde(skip)]
    pub span: Option<Span>,
}

/// Parsed shebang
#[derive(Debug, Clone, Serialize)]
pub struct Shebang {
    pub interpreter: String,
    pub args: Vec<String>,
}

impl Shebang {
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

/// File transfer specification
#[derive(Debug, Clone, Serialize)]
pub struct FileTransfer {
    pub local: String,
    pub remote: String,
}

/// SSH configuration
#[derive(Debug, Clone, Default, Serialize)]
pub struct SshConfig {
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity: Option<String>,
    pub workdir: Option<String>,
    pub upload: Vec<FileTransfer>,
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

/// Service kind
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum ServiceKind {
    Managed,
    External,
}

/// Readiness check for services
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

    pub fn host_port(&self) -> Option<(String, u16)> {
        match self {
            ReadinessCheck::Http { url } => {
                let url = url::Url::parse(url).ok()?;
                let host = url.host_str()?.to_string();
                let port = url
                    .port()
                    .unwrap_or(if url.scheme() == "https" { 443 } else { 80 });
                Some((host, port))
            }
            ReadinessCheck::Tcp { host, port } => Some((host.clone(), *port)),
            ReadinessCheck::Command { .. } => None,
        }
    }

    pub fn base_url(&self) -> Option<String> {
        match self {
            ReadinessCheck::Http { url } => {
                let parsed = url::Url::parse(url).ok()?;
                let port_str = parsed.port().map(|p| format!(":{}", p)).unwrap_or_default();
                Some(format!(
                    "{}://{}{}",
                    parsed.scheme(),
                    parsed.host_str()?,
                    port_str
                ))
            }
            _ => None,
        }
    }

    pub fn port(&self) -> Option<u16> {
        self.host_port().map(|(_, port)| port)
    }

    pub fn with_tunnel(&self, local_port: u16) -> Self {
        match self {
            ReadinessCheck::Http { url } => {
                if let Ok(mut parsed) = url::Url::parse(url) {
                    let _ = parsed.set_host(Some("127.0.0.1"));
                    let _ = parsed.set_port(Some(local_port));
                    ReadinessCheck::Http {
                        url: parsed.to_string(),
                    }
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

/// Log output mode
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
    pub forward: bool,
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

/// Kubernetes execution mode
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub enum K8sMode {
    Exec,
    #[default]
    Job,
    Apply,
}

/// ConfigMap/Secret mount
#[derive(Debug, Clone, Serialize)]
pub struct ConfigMount {
    pub name: String,
    pub mount_path: String,
}

/// Port forward specification
#[derive(Debug, Clone, Serialize)]
pub struct PortForward {
    pub local_port: u16,
    pub remote_port: u16,
    pub resource_type: Option<String>,
    pub resource: String,
}

/// Kubernetes configuration
#[derive(Debug, Clone, Serialize)]
pub struct K8sConfig {
    pub mode: K8sMode,
    pub context: Option<String>,
    pub namespace: String,
    pub selector: Option<String>,
    pub pod: Option<String>,
    pub container: Option<String>,
    pub image: Option<String>,
    pub cpu: Option<String>,
    pub memory: Option<String>,
    pub node_selector: Option<HashMap<String, String>>,
    pub tolerations: Vec<String>,
    pub service_account: Option<String>,
    pub ttl_seconds: Option<u32>,
    pub path: Option<String>,
    pub wait_for: Vec<String>,
    #[serde(
        serialize_with = "serialize_duration_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub wait_timeout: Option<Duration>,
    pub upload: Vec<FileTransfer>,
    pub download: Vec<FileTransfer>,
    pub configmaps: Vec<ConfigMount>,
    pub secrets: Vec<ConfigMount>,
    pub forwards: Vec<PortForward>,
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

fn serialize_duration_opt<S>(dur: &Option<Duration>, s: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match dur {
        Some(d) => s.serialize_str(&humantime::format_duration(*d).to_string()),
        None => s.serialize_none(),
    }
}

/// Parse error with location
#[derive(Debug, Clone)]
pub struct ParseError {
    pub span: Span,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "error at {}-{}: {}",
            self.span.start, self.span.end, self.message
        )
    }
}

impl std::error::Error for ParseError {}
