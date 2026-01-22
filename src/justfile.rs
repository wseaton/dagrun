//! Justfile-like syntax parser with Lua block support
//!
//! Supports a subset of justfile syntax plus extensions:
//!
//! ```dagrun
//! # variables
//! env := "production"
//!
//! # simple recipe
//! build:
//!     cargo build --release
//!
//! # recipe with dependencies
//! test: build
//!     cargo test
//!
//! # annotations for dagrun features
//! # @timeout 5m
//! # @retry 2
//! deploy: test
//!     ./deploy.sh
//!
//! # pipe from other tasks
//! # @pipe_from generate-a, generate-b
//! transform: generate-a generate-b
//!     tr 'a-z' 'A-Z'
//!
//! # join node (no command)
//! # @join
//! # @pipe_from worker-1, worker-2
//! collect: worker-1 worker-2
//!
//! # lua block for dynamic generation
//! @lua
//! for i = 1, 3 do
//!     task("worker-" .. i, {
//!         run = "echo worker " .. i,
//!         depends_on = {"build"}
//!     })
//! end
//! @end
//! ```

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;
use thiserror::Error;

use crate::config::{Config, ConfigMount, DotenvSettings, FileTransfer, K8sConfig, K8sMode, LogOutput, PortForward, ReadinessCheck, ServiceConfig, ServiceKind, Shebang, SshConfig, Task};
use crate::lua::parse_lua_config;

#[derive(Error, Debug)]
pub enum ParseError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse error at line {0}: {1}")]
    Syntax(usize, String),
    #[error("lua error: {0}")]
    Lua(String),
}

pub fn load_justflow<P: AsRef<Path>>(path: P) -> Result<Config, ParseError> {
    let content = std::fs::read_to_string(path)?;
    parse_justflow(&content)
}

pub fn parse_justflow(content: &str) -> Result<Config, ParseError> {
    let mut tasks: HashMap<String, Task> = HashMap::new();
    let mut variables: HashMap<String, String> = HashMap::new();
    let mut dotenv = DotenvSettings::default();
    let mut lines = content.lines().enumerate().peekable();

    // pending annotations for next recipe
    let mut pending_timeout: Option<Duration> = None;
    let mut pending_retry: Option<u32> = None;
    let mut pending_pipe_from: Vec<String> = Vec::new();
    let mut pending_join = false;
    let mut pending_ssh: Option<SshConfig> = None;
    let mut pending_upload: Vec<FileTransfer> = Vec::new();
    let mut pending_download: Vec<FileTransfer> = Vec::new();
    let mut pending_service: Option<ServiceConfig> = None;
    let mut pending_k8s: Option<K8sConfig> = None;
    let mut pending_k8s_configmaps: Vec<ConfigMount> = Vec::new();
    let mut pending_k8s_secrets: Vec<ConfigMount> = Vec::new();
    let mut pending_k8s_upload: Vec<FileTransfer> = Vec::new();
    let mut pending_k8s_download: Vec<FileTransfer> = Vec::new();
    let mut pending_k8s_forwards: Vec<PortForward> = Vec::new();

    while let Some((line_num, line)) = lines.next() {
        let trimmed = line.trim();

        // skip empty lines
        if trimmed.is_empty() {
            continue;
        }

        // handle lua blocks
        if trimmed == "@lua" {
            let lua_block = collect_lua_block(&mut lines)?;
            let lua_tasks =
                parse_lua_config(&lua_block).map_err(|e| ParseError::Lua(e.to_string()))?;
            tasks.extend(lua_tasks.tasks);
            continue;
        }

        // handle comments and annotations
        if trimmed.starts_with('#') {
            let comment = trimmed.trim_start_matches('#').trim();

            if let Some(rest) = comment.strip_prefix("@timeout") {
                let timeout_str = rest.trim();
                if let Ok(d) = timeout_str.parse::<humantime::Duration>() {
                    pending_timeout = Some(d.into());
                }
            } else if let Some(rest) = comment.strip_prefix("@retry") {
                if let Ok(n) = rest.trim().parse::<u32>() {
                    pending_retry = Some(n);
                }
            } else if let Some(rest) = comment.strip_prefix("@pipe_from") {
                pending_pipe_from = rest
                    .trim()
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            } else if comment.starts_with("@join") {
                pending_join = true;
            } else if let Some(rest) = comment.strip_prefix("@ssh") {
                // @ssh host user=foo port=22 workdir=/app identity=~/.ssh/id_rsa
                pending_ssh = Some(parse_ssh_annotation(rest.trim(), &variables));
            } else if let Some(rest) = comment.strip_prefix("@upload") {
                // @upload local/path:remote/path
                if let Some(transfer) = parse_file_transfer(rest.trim(), &variables) {
                    pending_upload.push(transfer);
                }
            } else if let Some(rest) = comment.strip_prefix("@download") {
                // @download remote/path:local/path (note: swapped from upload)
                if let Some((remote, local)) = rest.trim().split_once(':') {
                    pending_download.push(FileTransfer {
                        local: substitute_variables(local.trim(), &variables),
                        remote: substitute_variables(remote.trim(), &variables),
                    });
                }
            } else if let Some(rest) = comment.strip_prefix("@extern") {
                // @extern ready=tcp:localhost:5432 startup_timeout=30s interval=1s
                let mut svc = pending_service.take().unwrap_or_default();
                svc.kind = ServiceKind::External;
                parse_service_options(rest.trim(), &mut svc, &variables);
                pending_service = Some(svc);
            } else if let Some(rest) = comment.strip_prefix("@service") {
                // @service ready=http://localhost:8080/health startup_timeout=60s
                let mut svc = pending_service.take().unwrap_or_default();
                svc.kind = ServiceKind::Managed;
                parse_service_options(rest.trim(), &mut svc, &variables);
                pending_service = Some(svc);
            } else if let Some(rest) = comment.strip_prefix("@k8s-forward") {
                // @k8s-forward local_port:resource:remote_port or local_port:type/resource:remote_port
                if let Some(forward) = parse_port_forward(rest.trim(), &variables) {
                    pending_k8s_forwards.push(forward);
                }
            } else if let Some(rest) = comment.strip_prefix("@k8s-upload") {
                // @k8s-upload local/path:remote/path
                if let Some(transfer) = parse_file_transfer(rest.trim(), &variables) {
                    pending_k8s_upload.push(transfer);
                }
            } else if let Some(rest) = comment.strip_prefix("@k8s-download") {
                // @k8s-download remote/path:local/path
                if let Some((remote, local)) = rest.trim().split_once(':') {
                    pending_k8s_download.push(FileTransfer {
                        local: substitute_variables(local.trim(), &variables),
                        remote: substitute_variables(remote.trim(), &variables),
                    });
                }
            } else if let Some(rest) = comment.strip_prefix("@k8s-configmap") {
                // @k8s-configmap name:/mount/path
                if let Some(mount) = parse_config_mount(rest.trim(), &variables) {
                    pending_k8s_configmaps.push(mount);
                }
            } else if let Some(rest) = comment.strip_prefix("@k8s-secret") {
                // @k8s-secret name:/mount/path
                if let Some(mount) = parse_config_mount(rest.trim(), &variables) {
                    pending_k8s_secrets.push(mount);
                }
            } else if let Some(rest) = comment.strip_prefix("@k8s") {
                // @k8s job image=python:3.11 namespace=default ...
                let mut cfg = pending_k8s.take().unwrap_or_default();
                parse_k8s_annotation(rest.trim(), &mut cfg, &variables);
                pending_k8s = Some(cfg);
            }
            continue;
        }

        // handle set directives
        if let Some(rest) = trimmed.strip_prefix("set ") {
            parse_set_directive(rest, &mut dotenv);
            continue;
        }

        // handle variable assignment: name := value or name := `shell command`
        if let Some(idx) = trimmed.find(":=") {
            let name = trimmed[..idx].trim().to_string();
            let raw_value = trimmed[idx + 2..].trim();
            let value = if raw_value.starts_with('`') && raw_value.ends_with('`') && raw_value.len() > 1 {
                // shell expansion: evaluate command between backticks
                let cmd = &raw_value[1..raw_value.len() - 1];
                evaluate_shell_command(cmd).unwrap_or_else(|e| {
                    eprintln!("warning: shell expansion failed for '{}': {}", name, e);
                    String::new()
                })
            } else {
                raw_value.to_string()
            };
            variables.insert(name, value);
            continue;
        }

        // handle recipe definition: name deps*:
        if let Some(idx) = trimmed.find(':') {
            // make sure it's not := (variable assignment)
            if trimmed.chars().nth(idx + 1) != Some('=') {
                let header = &trimmed[..idx];
                let deps_str = &trimmed[idx + 1..];

                // parse recipe name (might have parameters, ignore for now)
                let name = header
                    .split_whitespace()
                    .next()
                    .unwrap_or(header)
                    .to_string();

                // parse dependencies, separating service: prefixed ones
                let mut depends_on: Vec<String> = Vec::new();
                let mut service_deps: Vec<String> = Vec::new();
                for dep in deps_str.split_whitespace() {
                    if let Some(svc) = dep.strip_prefix("service:") {
                        service_deps.push(svc.to_string());
                    } else {
                        depends_on.push(dep.to_string());
                    }
                }

                // collect recipe body (indented lines)
                let mut body_lines: Vec<String> = Vec::new();
                while let Some((_, next_line)) = lines.peek() {
                    if next_line.starts_with('\t') || next_line.starts_with("    ") {
                        let (_, body_line) = lines.next().unwrap();
                        // remove leading indent
                        let clean = body_line
                            .strip_prefix('\t')
                            .or_else(|| body_line.strip_prefix("    "))
                            .unwrap_or(body_line);
                        body_lines.push(substitute_variables(clean, &variables));
                    } else if next_line.trim().is_empty() {
                        // consume empty lines within recipe
                        lines.next();
                    } else {
                        break;
                    }
                }

                // parse shebang from first line if present
                let (run, shebang) = if body_lines.is_empty() || pending_join {
                    (None, None)
                } else {
                    let first_line = &body_lines[0];
                    if let Some(shebang) = Shebang::parse(first_line) {
                        // keep the full script including shebang for temp file execution
                        (Some(body_lines.join("\n")), Some(shebang))
                    } else {
                        (Some(body_lines.join("\n")), None)
                    }
                };

                // attach file transfers to ssh config if present
                let ssh = pending_ssh.take().map(|mut cfg| {
                    cfg.upload = std::mem::take(&mut pending_upload);
                    cfg.download = std::mem::take(&mut pending_download);
                    cfg
                });

                // clear file transfers even if no ssh config
                pending_upload.clear();
                pending_download.clear();

                // attach configmaps/secrets/uploads/downloads/forwards to k8s config if present
                let k8s = pending_k8s.take().map(|mut cfg| {
                    cfg.configmaps = std::mem::take(&mut pending_k8s_configmaps);
                    cfg.secrets = std::mem::take(&mut pending_k8s_secrets);
                    cfg.upload = std::mem::take(&mut pending_k8s_upload);
                    cfg.download = std::mem::take(&mut pending_k8s_download);
                    cfg.forwards = std::mem::take(&mut pending_k8s_forwards);
                    cfg
                });

                // clear k8s fields even if no k8s config
                pending_k8s_configmaps.clear();
                pending_k8s_secrets.clear();
                pending_k8s_upload.clear();
                pending_k8s_download.clear();
                pending_k8s_forwards.clear();

                let task = Task {
                    name: name.clone(),
                    run,
                    depends_on,
                    service_deps,
                    pipe_from: std::mem::take(&mut pending_pipe_from),
                    timeout: pending_timeout.take(),
                    retry: pending_retry.take().unwrap_or(0),
                    join: pending_join,
                    ssh,
                    k8s,
                    service: pending_service.take(),
                    shebang,
                };

                pending_join = false;
                tasks.insert(name, task);
                continue;
            }
        }

        // unknown line format
        return Err(ParseError::Syntax(
            line_num + 1,
            format!("unexpected: {}", trimmed),
        ));
    }

    Ok(Config { tasks, dotenv })
}

fn collect_lua_block<'a, I>(lines: &mut std::iter::Peekable<I>) -> Result<String, ParseError>
where
    I: Iterator<Item = (usize, &'a str)>,
{
    let mut lua_lines: Vec<&str> = Vec::new();

    for (_line_num, line) in lines.by_ref() {
        let trimmed = line.trim();
        if trimmed == "@end" {
            return Ok(lua_lines.join("\n"));
        }
        lua_lines.push(line);
    }

    Err(ParseError::Syntax(
        0,
        "unclosed @lua block, missing @end".to_string(),
    ))
}

/// Parse @ssh annotation: @ssh hostname user=foo port=22 workdir=/app
fn parse_ssh_annotation(s: &str, vars: &HashMap<String, String>) -> SshConfig {
    let mut config = SshConfig::default();
    let s = substitute_variables(s, vars);
    let parts: Vec<&str> = s.split_whitespace().collect();

    for (i, part) in parts.iter().enumerate() {
        if let Some((key, value)) = part.split_once('=') {
            match key {
                "user" => config.user = Some(value.to_string()),
                "port" => config.port = value.parse().ok(),
                "workdir" => config.workdir = Some(value.to_string()),
                "identity" => config.identity = Some(value.to_string()),
                "host" => config.host = value.to_string(),
                _ => {}
            }
        } else if i == 0 && config.host.is_empty() {
            // first non-key=value part is the host
            config.host = part.to_string();
        }
    }

    config
}

/// Parse file transfer annotation: local/path:remote/path
fn parse_file_transfer(s: &str, vars: &HashMap<String, String>) -> Option<FileTransfer> {
    let (local, remote) = s.split_once(':')?;
    Some(FileTransfer {
        local: substitute_variables(local.trim(), vars),
        remote: substitute_variables(remote.trim(), vars),
    })
}

/// Parse @k8s annotation: @k8s job image=python:3.11 namespace=default ...
fn parse_k8s_annotation(s: &str, config: &mut K8sConfig, vars: &HashMap<String, String>) {
    let s = substitute_variables(s, vars);

    for part in s.split_whitespace() {
        if let Some((key, value)) = part.split_once('=') {
            match key {
                "context" => config.context = Some(value.to_string()),
                "namespace" | "ns" => config.namespace = value.to_string(),
                "image" => config.image = Some(value.to_string()),
                "pod" => config.pod = Some(value.to_string()),
                "selector" => config.selector = Some(value.to_string()),
                "container" => config.container = Some(value.to_string()),
                "workdir" => config.workdir = Some(value.to_string()),
                "cpu" => config.cpu = Some(value.to_string()),
                "memory" => config.memory = Some(value.to_string()),
                "service_account" | "sa" => config.service_account = Some(value.to_string()),
                "ttl" => config.ttl_seconds = value.parse().ok(),
                "path" => config.path = Some(value.to_string()),
                "wait" => {
                    // wait=deployment/foo,statefulset/bar
                    config.wait_for = value.split(',').map(|s| s.trim().to_string()).collect();
                }
                "timeout" => {
                    if let Ok(d) = value.parse::<humantime::Duration>() {
                        config.wait_timeout = Some(d.into());
                    }
                }
                "node_selector" => {
                    // node_selector=gpu:true
                    if let Some((k, v)) = value.split_once(':') {
                        config
                            .node_selector
                            .get_or_insert_with(std::collections::HashMap::new)
                            .insert(k.to_string(), v.to_string());
                    }
                }
                "toleration" => config.tolerations.push(value.to_string()),
                _ => {}
            }
        } else {
            // mode keywords
            match part {
                "job" => config.mode = K8sMode::Job,
                "exec" => config.mode = K8sMode::Exec,
                "apply" => config.mode = K8sMode::Apply,
                _ => {}
            }
        }
    }
}

/// Parse configmap/secret mount: name:/path
fn parse_config_mount(s: &str, vars: &HashMap<String, String>) -> Option<ConfigMount> {
    let s = substitute_variables(s, vars);
    let (name, path) = s.split_once(':')?;
    Some(ConfigMount {
        name: name.trim().to_string(),
        mount_path: path.trim().to_string(),
    })
}

/// Parse port forward: local_port:resource:remote_port or local_port:type/resource:remote_port
fn parse_port_forward(s: &str, vars: &HashMap<String, String>) -> Option<PortForward> {
    let s = substitute_variables(s, vars);
    let parts: Vec<&str> = s.splitn(3, ':').collect();

    if parts.len() < 3 {
        return None;
    }

    let local_port = parts[0].trim().parse::<u16>().ok()?;
    let resource_part = parts[1].trim();
    let remote_port = parts[2].trim().parse::<u16>().ok()?;

    // check if resource has a type prefix (e.g., svc/my-service, deployment/my-deploy)
    let (resource_type, resource) = if resource_part.contains('/') {
        let (rt, r) = resource_part.split_once('/')?;
        (Some(rt.to_string()), r.to_string())
    } else {
        (None, resource_part.to_string())
    };

    Some(PortForward {
        local_port,
        remote_port,
        resource_type,
        resource,
    })
}

fn substitute_variables(line: &str, vars: &HashMap<String, String>) -> String {
    let mut result = line.to_string();
    for (name, value) in vars {
        // handle {{name}} syntax
        let pattern = format!("{{{{{}}}}}", name);
        result = result.replace(&pattern, value);
    }
    result
}

/// evaluate a shell command and return its stdout (trimmed)
fn evaluate_shell_command(cmd: &str) -> Result<String, String> {
    use std::process::Command;
    let output = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .map_err(|e| e.to_string())?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("command failed: {}", stderr.trim()));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Parse service options from annotation: ready=http://... startup_timeout=60s ...
fn parse_service_options(s: &str, svc: &mut ServiceConfig, vars: &HashMap<String, String>) {
    let s = substitute_variables(s, vars);
    for part in s.split_whitespace() {
        if let Some((key, value)) = part.split_once('=') {
            match key {
                "ready" => svc.ready = ReadinessCheck::parse(value),
                "startup_timeout" => {
                    if let Ok(d) = value.parse::<humantime::Duration>() {
                        svc.startup_timeout = d.into();
                    }
                }
                "shutdown_grace" => {
                    if let Ok(d) = value.parse::<humantime::Duration>() {
                        svc.shutdown_grace = d.into();
                    }
                }
                "shutdown_kill" => {
                    if let Ok(d) = value.parse::<humantime::Duration>() {
                        svc.shutdown_kill = d.into();
                    }
                }
                "interval" => {
                    if let Ok(d) = value.parse::<humantime::Duration>() {
                        svc.interval = d.into();
                    }
                }
                "log" => {
                    svc.log = match value {
                        "quiet" => LogOutput::Quiet,
                        _ => LogOutput::Stream,
                    };
                }
                "forward" => {
                    svc.forward = value == "true";
                }
                "preflight" => {
                    svc.preflight = Some(value.trim_matches('"').to_string());
                }
                _ => {}
            }
        }
    }
}

/// Parse set directive: set dotenv-load := true
fn parse_set_directive(s: &str, dotenv: &mut DotenvSettings) {
    let s = s.trim();
    if let Some((key, value)) = s.split_once(":=") {
        let key = key.trim();
        let value = value.trim();
        match key {
            "dotenv-load" => dotenv.load = value == "true",
            "dotenv-path" => {
                dotenv.load = true;
                dotenv.paths = value
                    .trim_matches('"')
                    .split(',')
                    .map(|p| p.trim().trim_matches('"').to_string())
                    .filter(|p| !p.is_empty())
                    .collect();
            }
            "dotenv-required" => dotenv.required = value == "true",
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_recipe() {
        let input = r#"
build:
    cargo build
"#;
        let config = parse_justflow(input).unwrap();
        assert_eq!(config.tasks.len(), 1);
        assert_eq!(config.tasks["build"].run, Some("cargo build".to_string()));
    }

    #[test]
    fn test_recipe_with_deps() {
        let input = r#"
build:
    cargo build

test: build
    cargo test
"#;
        let config = parse_justflow(input).unwrap();
        assert_eq!(config.tasks["test"].depends_on, vec!["build"]);
    }

    #[test]
    fn test_annotations() {
        let input = r#"
# @timeout 5m
# @retry 2
deploy:
    ./deploy.sh
"#;
        let config = parse_justflow(input).unwrap();
        assert_eq!(
            config.tasks["deploy"].timeout,
            Some(Duration::from_secs(300))
        );
        assert_eq!(config.tasks["deploy"].retry, 2);
    }

    #[test]
    fn test_pipe_from() {
        let input = r#"
gen-a:
    echo a

gen-b:
    echo b

# @pipe_from gen-a, gen-b
transform: gen-a gen-b
    tr 'a-z' 'A-Z'
"#;
        let config = parse_justflow(input).unwrap();
        assert_eq!(config.tasks["transform"].pipe_from, vec!["gen-a", "gen-b"]);
    }

    #[test]
    fn test_join_node() {
        let input = r#"
a:
    echo a

b:
    echo b

# @join
# @pipe_from a, b
collect: a b
"#;
        let config = parse_justflow(input).unwrap();
        assert!(config.tasks["collect"].is_join());
        assert_eq!(config.tasks["collect"].pipe_from, vec!["a", "b"]);
    }

    #[test]
    fn test_lua_block() {
        let input = r#"
build:
    cargo build

@lua
for i = 1, 3 do
    task("worker-" .. i, {
        run = "echo " .. i,
        depends_on = {"build"}
    })
end
@end
"#;
        let config = parse_justflow(input).unwrap();
        assert_eq!(config.tasks.len(), 4); // build + 3 workers
        assert!(config.tasks.contains_key("worker-1"));
        assert!(config.tasks.contains_key("worker-2"));
        assert!(config.tasks.contains_key("worker-3"));
    }

    #[test]
    fn test_variables() {
        let input = r#"
env := production

deploy:
    ./deploy.sh {{env}}
"#;
        let config = parse_justflow(input).unwrap();
        assert_eq!(
            config.tasks["deploy"].run,
            Some("./deploy.sh production".to_string())
        );
    }

    #[test]
    fn test_shell_expansion() {
        let input = r#"
greeting := `echo hello`
count := `expr 1 + 2`

run:
    echo "{{greeting}} world, count={{count}}"
"#;
        let config = parse_justflow(input).unwrap();
        assert_eq!(
            config.tasks["run"].run,
            Some("echo \"hello world, count=3\"".to_string())
        );
    }

    #[test]
    fn test_file_transfers() {
        let input = r#"
# @ssh remote-host
# @upload ./local.txt:/remote/local.txt
# @download /remote/result.txt:./result.txt
build-remote:
    make build
"#;
        let config = parse_justflow(input).unwrap();
        let ssh = config.tasks["build-remote"].ssh.as_ref().unwrap();
        assert_eq!(ssh.host, "remote-host");
        assert_eq!(ssh.upload.len(), 1);
        assert_eq!(ssh.upload[0].local, "./local.txt");
        assert_eq!(ssh.upload[0].remote, "/remote/local.txt");
        assert_eq!(ssh.download.len(), 1);
        assert_eq!(ssh.download[0].remote, "/remote/result.txt");
        assert_eq!(ssh.download[0].local, "./result.txt");
    }

    #[test]
    fn test_ssh_variables() {
        let input = r#"
host := user@10.0.0.1
workdir := /home/user/project

# @ssh {{host}} workdir={{workdir}}
# @upload ./{{workdir}}/file.txt:/remote/file.txt
remote-task:
    echo "hello"
"#;
        let config = parse_justflow(input).unwrap();
        let ssh = config.tasks["remote-task"].ssh.as_ref().unwrap();
        assert_eq!(ssh.host, "user@10.0.0.1");
        assert_eq!(ssh.workdir, Some("/home/user/project".to_string()));
        assert_eq!(ssh.upload[0].local, ".//home/user/project/file.txt");
        assert_eq!(ssh.upload[0].remote, "/remote/file.txt");
    }

    #[test]
    fn test_service_managed() {
        let input = r#"
# @service ready=http://localhost:8080/health startup_timeout=30s
api-server:
    ./run-server
"#;
        let config = parse_justflow(input).unwrap();
        let svc = config.tasks["api-server"].service.as_ref().unwrap();
        assert_eq!(svc.kind, crate::config::ServiceKind::Managed);
        assert!(matches!(svc.ready, Some(crate::config::ReadinessCheck::Http { .. })));
        assert_eq!(svc.startup_timeout, std::time::Duration::from_secs(30));
    }

    #[test]
    fn test_service_external() {
        let input = r#"
# @extern ready=tcp:localhost:5432 startup_timeout=10s interval=500ms
postgres:
"#;
        let config = parse_justflow(input).unwrap();
        let svc = config.tasks["postgres"].service.as_ref().unwrap();
        assert_eq!(svc.kind, crate::config::ServiceKind::External);
        assert!(matches!(svc.ready, Some(crate::config::ReadinessCheck::Tcp { .. })));
        assert_eq!(svc.startup_timeout, std::time::Duration::from_secs(10));
        assert_eq!(svc.interval, std::time::Duration::from_millis(500));
    }

    #[test]
    fn test_service_dependency() {
        let input = r#"
# @extern ready=tcp:localhost:5432
postgres:

test-db: service:postgres
    psql -c "SELECT 1"
"#;
        let config = parse_justflow(input).unwrap();
        assert_eq!(config.tasks["test-db"].service_deps, vec!["postgres"]);
        assert!(config.tasks["test-db"].depends_on.is_empty());
    }

    #[test]
    fn test_dotenv_settings() {
        let input = r#"
set dotenv-load := true
set dotenv-path := ".env, .env.local"
set dotenv-required := true

build:
    cargo build
"#;
        let config = parse_justflow(input).unwrap();
        assert!(config.dotenv.load);
        assert_eq!(config.dotenv.paths, vec![".env", ".env.local"]);
        assert!(config.dotenv.required);
    }

    #[test]
    fn test_ssh_and_service_combined() {
        let input = r#"
# @ssh remote-host user=deploy workdir=/app
# @service ready=http://localhost:8080/health startup_timeout=30s
remote-api:
    ./start-server.sh
"#;
        let config = parse_justflow(input).unwrap();
        let task = &config.tasks["remote-api"];

        // verify ssh config
        let ssh = task.ssh.as_ref().unwrap();
        assert_eq!(ssh.host, "remote-host");
        assert_eq!(ssh.user, Some("deploy".to_string()));
        assert_eq!(ssh.workdir, Some("/app".to_string()));

        // verify service config
        let svc = task.service.as_ref().unwrap();
        assert_eq!(svc.kind, crate::config::ServiceKind::Managed);
        assert!(matches!(svc.ready, Some(crate::config::ReadinessCheck::Http { .. })));
        assert_eq!(svc.startup_timeout, std::time::Duration::from_secs(30));
    }

    #[test]
    fn test_shebang_python() {
        let input = r#"
run-python:
    #!/usr/bin/env python3
    print("hello from python")
    x = 1 + 2
    print(f"result: {x}")
"#;
        let config = parse_justflow(input).unwrap();
        let task = &config.tasks["run-python"];

        // verify shebang was parsed
        let shebang = task.shebang.as_ref().unwrap();
        assert_eq!(shebang.interpreter, "/usr/bin/env");
        assert_eq!(shebang.args, vec!["python3"]);

        // verify full script is preserved
        assert!(task.run.as_ref().unwrap().starts_with("#!/usr/bin/env python3"));
        assert!(task.run.as_ref().unwrap().contains("print(\"hello from python\")"));
    }

    #[test]
    fn test_shebang_bash() {
        let input = r#"
run-bash:
    #!/bin/bash
    set -e
    echo "hello"
"#;
        let config = parse_justflow(input).unwrap();
        let task = &config.tasks["run-bash"];

        let shebang = task.shebang.as_ref().unwrap();
        assert_eq!(shebang.interpreter, "/bin/bash");
        assert!(shebang.args.is_empty());
    }

    #[test]
    fn test_no_shebang() {
        let input = r#"
simple:
    echo "just a shell command"
"#;
        let config = parse_justflow(input).unwrap();
        let task = &config.tasks["simple"];

        // no shebang
        assert!(task.shebang.is_none());
        assert_eq!(task.run, Some("echo \"just a shell command\"".to_string()));
    }

    #[test]
    fn test_shebang_uv_script() {
        let input = r#"
uv-script:
    #!/usr/bin/env -S uv run --script
    # /// script
    # dependencies = ["requests"]
    # ///
    import requests
    print(requests.get("https://httpbin.org/ip").json())
"#;
        let config = parse_justflow(input).unwrap();
        let task = &config.tasks["uv-script"];

        let shebang = task.shebang.as_ref().unwrap();
        assert_eq!(shebang.interpreter, "/usr/bin/env");
        assert_eq!(shebang.args, vec!["-S", "uv", "run", "--script"]);
    }
}
