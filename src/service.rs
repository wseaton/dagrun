//! Service lifecycle management

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::RwLock;
use tokio::time::{sleep, timeout};
use tracing::{error, info, warn};

use crate::config::{LogOutput, ReadinessCheck, ServiceConfig, ServiceKind, Task};
use crate::env::service_env_vars;
use crate::ssh::{self, SessionCache};

/// Service state machine
#[derive(Debug, Clone, PartialEq)]
pub enum ServiceState {
    Stopped,
    Starting,
    Ready,
    Failed(String),
    Stopping,
}

/// Info about an active SSH port forward
#[derive(Debug, Clone)]
struct PortForwardInfo {
    local_port: u16,
    remote_host: String,
    remote_port: u16,
}

/// Running service instance
struct ServiceInstance {
    task: Task,
    state: ServiceState,
    child: Option<Child>,
    /// remote PID for SSH-based services
    remote_pid: Option<u32>,
    /// active SSH port forward for tunneled services
    port_forward: Option<PortForwardInfo>,
    ref_count: usize,
}

/// Manages service lifecycles
pub struct ServiceManager {
    services: Arc<RwLock<HashMap<String, ServiceInstance>>>,
    ssh_sessions: SessionCache,
    insecure_tls: bool,
}

impl ServiceManager {
    pub fn new() -> Self {
        Self::with_ssh_cache(ssh::new_session_cache())
    }

    pub fn with_ssh_cache(ssh_sessions: SessionCache) -> Self {
        let insecure_tls = std::env::var("DAGRUN_INSECURE_TLS")
            .map(|v| v == "1" || v.to_lowercase() == "true")
            .unwrap_or(false);

        if insecure_tls {
            warn!("TLS verification disabled via DAGRUN_INSECURE_TLS");
        }

        ServiceManager {
            services: Arc::new(RwLock::new(HashMap::new())),
            ssh_sessions,
            insecure_tls,
        }
    }

    /// Register a service (doesn't start it yet)
    pub async fn register(&self, task: &Task) {
        if task.service.is_none() {
            return;
        }

        let mut services = self.services.write().await;
        if !services.contains_key(&task.name) {
            services.insert(
                task.name.clone(),
                ServiceInstance {
                    task: task.clone(),
                    state: ServiceState::Stopped,
                    child: None,
                    remote_pid: None,
                    port_forward: None,
                    ref_count: 0,
                },
            );
        }
    }

    /// Acquire a service (starts if needed, waits for ready)
    pub async fn acquire(&self, name: &str) -> Result<HashMap<String, String>, String> {
        // bump ref count
        {
            let mut services = self.services.write().await;
            if let Some(svc) = services.get_mut(name) {
                svc.ref_count += 1;
            } else {
                return Err(format!("service '{}' not registered", name));
            }
        }

        // check current state
        let (state, config, task) = {
            let services = self.services.read().await;
            let svc = services.get(name).unwrap();
            (svc.state.clone(), svc.task.service.clone().unwrap(), svc.task.clone())
        };

        match state {
            ServiceState::Ready => {
                let forwarded_port = {
                    let services = self.services.read().await;
                    services.get(name).and_then(|s| s.port_forward.as_ref().map(|pf| pf.local_port))
                };
                return Ok(service_env_vars(name, &config.kind, config.ready.as_ref(), forwarded_port));
            }
            ServiceState::Failed(msg) => {
                return Err(format!("service '{}' failed: {}", name, msg));
            }
            ServiceState::Stopped => {
                self.start_service(name, &task, &config).await?;
            }
            ServiceState::Starting => {
                // another task is starting it, just wait for ready
                self.wait_for_ready(name, &config, None).await?;
            }
            ServiceState::Stopping => {
                return Err(format!("service '{}' is stopping", name));
            }
        }

        let (config, forwarded_port) = {
            let services = self.services.read().await;
            let svc = services.get(name).unwrap();
            (
                svc.task.service.clone().unwrap(),
                svc.port_forward.as_ref().map(|pf| pf.local_port),
            )
        };
        Ok(service_env_vars(name, &config.kind, config.ready.as_ref(), forwarded_port))
    }

    /// Release a service (stops if ref_count hits 0)
    pub async fn release(&self, name: &str) {
        let should_stop = {
            let mut services = self.services.write().await;
            if let Some(svc) = services.get_mut(name) {
                svc.ref_count = svc.ref_count.saturating_sub(1);
                svc.ref_count == 0 && svc.task.service.as_ref().map(|s| s.kind == ServiceKind::Managed).unwrap_or(false)
            } else {
                false
            }
        };

        if should_stop {
            self.stop_service(name).await;
        }
    }

    /// Start a managed service
    async fn start_service(&self, name: &str, task: &Task, config: &ServiceConfig) -> Result<(), String> {
        // mark as starting
        {
            let mut services = self.services.write().await;
            if let Some(svc) = services.get_mut(name) {
                svc.state = ServiceState::Starting;
            }
        }

        // external services just wait for readiness
        if config.kind == ServiceKind::External {
            return self.wait_for_ready(name, config, None).await;
        }

        // start the process
        let cmd = task.run.as_ref().ok_or_else(|| format!("service '{}' has no run command", name))?;

        info!(service = %name, "starting service");

        // check if this is a remote service (has ssh config)
        if let Some(ref ssh_config) = task.ssh {
            return self.start_remote_service(name, cmd, ssh_config, config).await;
        }

        // run preflight check if configured
        if let Some(ref preflight) = config.preflight {
            info!(service = %name, "running preflight check");
            let output = Command::new("sh")
                .arg("-c")
                .arg(preflight)
                .output()
                .await
                .map_err(|e| format!("preflight failed: {}", e))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(format!("preflight check failed: {}", stderr.trim()));
            }
            info!(service = %name, "preflight check passed");
        }

        let mut child = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to start service '{}': {}", name, e))?;

        // stream output if configured
        if config.log != LogOutput::Quiet {
            let stdout = child.stdout.take();
            let stderr = child.stderr.take();
            let svc_name = name.to_string();

            if let Some(stdout) = stdout {
                let name = svc_name.clone();
                tokio::spawn(async move {
                    let reader = BufReader::new(stdout);
                    let mut lines = reader.lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        println!("[service:{}] {}", name, line);
                    }
                });
            }

            if let Some(stderr) = stderr {
                let name = svc_name;
                tokio::spawn(async move {
                    let reader = BufReader::new(stderr);
                    let mut lines = reader.lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        eprintln!("[service:{}] {}", name, line);
                    }
                });
            }
        }

        // store the child process
        {
            let mut services = self.services.write().await;
            if let Some(svc) = services.get_mut(name) {
                svc.child = Some(child);
            }
        }

        // wait for readiness
        self.wait_for_ready(name, config, None).await
    }

    /// Start a service on a remote host via SSH
    async fn start_remote_service(
        &self,
        name: &str,
        cmd: &str,
        ssh_config: &crate::config::SshConfig,
        config: &ServiceConfig,
    ) -> Result<(), String> {
        info!(service = %name, host = %ssh_config.host, "starting remote service");

        let session = ssh::get_session(ssh_config, &self.ssh_sessions)
            .await
            .map_err(|e| format!("SSH connection failed: {}", e))?;

        // run preflight check on remote host if configured
        if let Some(ref preflight) = config.preflight {
            info!(service = %name, host = %ssh_config.host, "running remote preflight check");
            let preflight_cmd = match &ssh_config.workdir {
                Some(dir) => format!("cd {} && {}", dir, preflight),
                None => preflight.clone(),
            };
            let result = ssh::execute_remote(&session, name, &preflight_cmd, None, None)
                .await
                .map_err(|e| format!("preflight failed: {}", e))?;

            if !result.success {
                return Err(format!("preflight check failed: {}", result.stderr.trim()));
            }
            info!(service = %name, "preflight check passed");
        }

        // build command with workdir if specified
        // use subshell to fully detach from SSH session
        let full_cmd = match &ssh_config.workdir {
            Some(dir) => format!("cd {} && ( nohup {} </dev/null >/tmp/dagrun-{}.log 2>&1 & echo $! )", dir, cmd, name),
            None => format!("( nohup {} </dev/null >/tmp/dagrun-{}.log 2>&1 & echo $! )", cmd, name),
        };

        let output = ssh::execute_remote(&session, name, &full_cmd, None, None)
            .await
            .map_err(|e| format!("failed to start remote service '{}': {}", name, e))?;

        if !output.success {
            return Err(format!("remote service '{}' failed to start", name));
        }

        // parse PID from output
        let pid: u32 = output.stdout.trim().parse()
            .map_err(|_| format!("failed to parse remote PID for '{}': {}", name, output.stdout))?;

        info!(service = %name, pid = pid, "captured remote PID");

        // start log streaming if configured
        if config.log == LogOutput::Stream {
            let tail_cmd = format!("tail -f /tmp/dagrun-{}.log 2>/dev/null", name);
            info!(service = %name, "starting remote log stream");
            ssh::spawn_streaming_command(
                session.clone(),
                name.to_string(),
                tail_cmd,
            );
        }

        // check if we need to set up port forwarding for health checks
        // for SSH services, auto-tunnel any HTTP/TCP readiness check (not just localhost)
        let should_forward = config.forward
            || config.ready.as_ref().and_then(|r| r.port()).is_some();

        let (tunneled_check, port_forward_info) = if should_forward {
            if let Some(ref ready) = config.ready {
                if let Some(remote_port) = ready.port() {
                    let local_port = ssh::find_available_port()
                        .map_err(|e| format!("failed to find available port: {}", e))?;

                    info!(
                        service = %name,
                        local_port = local_port,
                        remote_port = remote_port,
                        "setting up SSH tunnel localhost:{} -> localhost:{}",
                        local_port,
                        remote_port
                    );

                    ssh::setup_port_forward(&session, local_port, "localhost", remote_port)
                        .await
                        .map_err(|e| format!("failed to set up port forward: {}", e))?;

                    let tunneled = ready.with_tunnel(local_port);
                    let pf_info = PortForwardInfo {
                        local_port,
                        remote_host: "localhost".to_string(),
                        remote_port,
                    };

                    (Some(tunneled), Some(pf_info))
                } else {
                    (None, None)
                }
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };

        // store the remote PID and port forward info
        {
            let mut services = self.services.write().await;
            if let Some(svc) = services.get_mut(name) {
                svc.remote_pid = Some(pid);
                svc.port_forward = port_forward_info;
            }
        }

        self.wait_for_ready(name, config, tunneled_check).await
    }

    /// Wait for service to become ready
    /// If tunneled_check is provided, use it instead of config.ready (for SSH tunneled services)
    async fn wait_for_ready(
        &self,
        name: &str,
        config: &ServiceConfig,
        tunneled_check: Option<ReadinessCheck>,
    ) -> Result<(), String> {
        let check_to_use = tunneled_check.as_ref().or(config.ready.as_ref());

        let Some(ready) = check_to_use else {
            // no readiness check, assume ready immediately
            let mut services = self.services.write().await;
            if let Some(svc) = services.get_mut(name) {
                svc.state = ServiceState::Ready;
            }
            return Ok(());
        };

        let check_target = match ready {
            ReadinessCheck::Http { url } => url.clone(),
            ReadinessCheck::Tcp { host, port } => format!("tcp:{}:{}", host, port),
            ReadinessCheck::Command { cmd } => format!("cmd:{}", cmd),
        };

        info!(service = %name, check = %check_target, "waiting for readiness");

        let start = tokio::time::Instant::now();
        let deadline = start + config.startup_timeout;
        let max_attempts = (config.startup_timeout.as_secs_f64() / config.interval.as_secs_f64()).ceil() as u32;
        let mut attempt = 0u32;

        while tokio::time::Instant::now() < deadline {
            attempt += 1;

            // check if process crashed
            {
                let mut services = self.services.write().await;
                if let Some(svc) = services.get_mut(name) {
                    if let Some(ref mut child) = svc.child {
                        if let Ok(Some(status)) = child.try_wait() {
                            let msg = format!("service exited with status: {}", status);
                            svc.state = ServiceState::Failed(msg.clone());
                            return Err(msg);
                        }
                    }
                }
            }

            info!(
                service = %name,
                attempt = attempt,
                max_attempts = max_attempts,
                "polling readiness"
            );

            if self.check_readiness(ready).await {
                let elapsed = start.elapsed();
                info!(
                    service = %name,
                    elapsed_secs = format!("{:.1}", elapsed.as_secs_f64()),
                    "service ready"
                );
                let mut services = self.services.write().await;
                if let Some(svc) = services.get_mut(name) {
                    svc.state = ServiceState::Ready;
                }
                return Ok(());
            }

            sleep(config.interval).await;
        }

        let msg = format!("service '{}' failed to become ready within {:?}", name, config.startup_timeout);
        {
            let mut services = self.services.write().await;
            if let Some(svc) = services.get_mut(name) {
                svc.state = ServiceState::Failed(msg.clone());
            }
        }
        Err(msg)
    }

    /// Check if service is ready
    async fn check_readiness(&self, ready: &ReadinessCheck) -> bool {
        match ready {
            ReadinessCheck::Http { url } => self.check_http(url).await,
            ReadinessCheck::Tcp { host, port } => self.check_tcp(host, *port).await,
            ReadinessCheck::Command { cmd } => self.check_command(cmd).await,
        }
    }

    async fn check_http(&self, url: &str) -> bool {
        let client = match reqwest::Client::builder()
            .danger_accept_invalid_certs(self.insecure_tls)
            .timeout(Duration::from_secs(5))
            .build()
        {
            Ok(c) => c,
            Err(_) => return false,
        };

        match timeout(Duration::from_secs(5), client.get(url).send()).await {
            Ok(Ok(resp)) => resp.status().is_success(),
            _ => false,
        }
    }

    async fn check_tcp(&self, host: &str, port: u16) -> bool {
        let addr = format!("{}:{}", host, port);
        timeout(Duration::from_secs(5), TcpStream::connect(&addr))
            .await
            .map(|r| r.is_ok())
            .unwrap_or(false)
    }

    async fn check_command(&self, cmd: &str) -> bool {
        match Command::new("sh").arg("-c").arg(cmd).output().await {
            Ok(output) => output.status.success(),
            Err(_) => false,
        }
    }

    /// Stop a managed service
    async fn stop_service(&self, name: &str) {
        let (config, child, remote_pid, ssh_config, port_forward) = {
            let mut services = self.services.write().await;
            if let Some(svc) = services.get_mut(name) {
                svc.state = ServiceState::Stopping;
                (
                    svc.task.service.clone(),
                    svc.child.take(),
                    svc.remote_pid.take(),
                    svc.task.ssh.clone(),
                    svc.port_forward.take(),
                )
            } else {
                return;
            }
        };

        let config = config.unwrap_or_default();

        // handle remote service shutdown
        if let (Some(pid), Some(ssh_config)) = (remote_pid, ssh_config) {
            self.stop_remote_service(name, pid, &ssh_config, port_forward).await;
            return;
        }

        let Some(mut child) = child else {
            // mark as stopped anyway
            let mut services = self.services.write().await;
            if let Some(svc) = services.get_mut(name) {
                svc.state = ServiceState::Stopped;
            }
            return;
        };

        info!(service = %name, "stopping service");

        // send SIGTERM
        #[cfg(unix)]
        {
            use nix::sys::signal::{kill, Signal};
            use nix::unistd::Pid;

            if let Some(pid) = child.id() {
                let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
            }
        }

        // wait for graceful shutdown
        match timeout(config.shutdown_grace, child.wait()).await {
            Ok(Ok(_)) => {
                info!(service = %name, "service stopped gracefully");
            }
            _ => {
                warn!(service = %name, "service did not stop gracefully, killing");
                let _ = child.kill().await;

                // wait for kill to take effect
                match timeout(config.shutdown_kill, child.wait()).await {
                    Ok(Ok(_)) => {}
                    _ => error!(service = %name, "service did not die after SIGKILL"),
                }
            }
        }

        // mark as stopped
        {
            let mut services = self.services.write().await;
            if let Some(svc) = services.get_mut(name) {
                svc.state = ServiceState::Stopped;
            }
        }
    }

    /// Stop a remote service via SSH
    async fn stop_remote_service(
        &self,
        name: &str,
        pid: u32,
        ssh_config: &crate::config::SshConfig,
        port_forward: Option<PortForwardInfo>,
    ) {
        info!(service = %name, pid = pid, host = %ssh_config.host, "stopping remote service");

        match ssh::get_session(ssh_config, &self.ssh_sessions).await {
            Ok(session) => {
                // close port forward if active
                if let Some(pf) = port_forward {
                    if let Err(e) = ssh::close_port_forward(
                        &session,
                        pf.local_port,
                        &pf.remote_host,
                        pf.remote_port,
                    ).await {
                        warn!(
                            service = %name,
                            error = %e,
                            "failed to close port forward"
                        );
                    } else {
                        info!(
                            service = %name,
                            local_port = pf.local_port,
                            "SSH tunnel closed"
                        );
                    }
                }

                // try graceful SIGTERM first, then SIGKILL
                let kill_cmd = format!("kill {} 2>/dev/null || kill -9 {} 2>/dev/null || true", pid, pid);
                let _ = ssh::execute_remote(&session, name, &kill_cmd, None, None).await;
                info!(service = %name, "remote service stopped");
            }
            Err(e) => {
                warn!(service = %name, error = %e, "failed to connect to stop remote service");
            }
        }

        // mark as stopped
        let mut services = self.services.write().await;
        if let Some(svc) = services.get_mut(name) {
            svc.state = ServiceState::Stopped;
        }
    }

    /// Stop all managed services
    pub async fn shutdown(&self) {
        let names: Vec<String> = {
            let services = self.services.read().await;
            services
                .iter()
                .filter(|(_, svc)| {
                    svc.task.service.as_ref().map(|s| s.kind == ServiceKind::Managed).unwrap_or(false)
                        && svc.state != ServiceState::Stopped
                })
                .map(|(name, _)| name.clone())
                .collect()
        };

        for name in names {
            self.stop_service(&name).await;
        }
    }

    /// Get current state of a service
    #[allow(dead_code)]
    pub async fn state(&self, name: &str) -> Option<ServiceState> {
        let services = self.services.read().await;
        services.get(name).map(|s| s.state.clone())
    }
}

impl Default for ServiceManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn make_service_task(name: &str, cmd: &str, ready: ReadinessCheck) -> Task {
        Task {
            name: name.to_string(),
            run: Some(cmd.to_string()),
            depends_on: vec![],
            service_deps: vec![],
            pipe_from: vec![],
            timeout: None,
            retry: 0,
            join: false,
            ssh: None,
            k8s: None,
            shebang: None,
            service: Some(ServiceConfig {
                kind: ServiceKind::Managed,
                ready: Some(ready),
                startup_timeout: Duration::from_secs(10),
                shutdown_grace: Duration::from_secs(2),
                shutdown_kill: Duration::from_secs(5),
                interval: Duration::from_millis(100),
                log: LogOutput::Quiet,
                forward: false,
                preflight: None,
            }),
        }
    }

    #[tokio::test]
    async fn test_tcp_readiness_check() {
        let mgr = ServiceManager::new();

        // start a simple TCP listener using nc
        let task = make_service_task(
            "tcp-test",
            "nc -l 19876",
            ReadinessCheck::Tcp {
                host: "127.0.0.1".to_string(),
                port: 19876,
            },
        );

        mgr.register(&task).await;

        // acquire should start the service and wait for readiness
        let result = mgr.acquire("tcp-test").await;
        assert!(result.is_ok(), "service should become ready: {:?}", result);

        let env = result.unwrap();
        assert_eq!(env.get("DAGRUN_SVC_TCP_TEST_HOST"), Some(&"127.0.0.1".to_string()));
        assert_eq!(env.get("DAGRUN_SVC_TCP_TEST_PORT"), Some(&"19876".to_string()));

        // verify state is ready
        assert_eq!(mgr.state("tcp-test").await, Some(ServiceState::Ready));

        // release and shutdown
        mgr.release("tcp-test").await;
        mgr.shutdown().await;

        // give it a moment to stop
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(mgr.state("tcp-test").await, Some(ServiceState::Stopped));
    }

    #[tokio::test]
    async fn test_http_readiness_check() {
        let mgr = ServiceManager::new();

        // start python http server on a random-ish port
        let task = make_service_task(
            "http-test",
            "python3 -m http.server 19877",
            ReadinessCheck::Http {
                url: "http://127.0.0.1:19877/".to_string(),
            },
        );

        mgr.register(&task).await;

        let result = mgr.acquire("http-test").await;
        assert!(result.is_ok(), "http service should become ready: {:?}", result);

        let env = result.unwrap();
        assert_eq!(env.get("DAGRUN_SVC_HTTP_TEST_URL"), Some(&"http://127.0.0.1:19877".to_string()));

        mgr.release("http-test").await;
        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn test_command_readiness_check() {
        let mgr = ServiceManager::new();

        // service that becomes "ready" when a file exists
        let task = make_service_task(
            "cmd-test",
            "touch /tmp/dagrun-test-ready && sleep 30",
            ReadinessCheck::Command {
                cmd: "test -f /tmp/dagrun-test-ready".to_string(),
            },
        );

        // clean up from previous runs
        let _ = std::fs::remove_file("/tmp/dagrun-test-ready");

        mgr.register(&task).await;

        let result = mgr.acquire("cmd-test").await;
        assert!(result.is_ok(), "cmd service should become ready: {:?}", result);

        mgr.release("cmd-test").await;
        mgr.shutdown().await;

        // clean up
        let _ = std::fs::remove_file("/tmp/dagrun-test-ready");
    }

    #[tokio::test]
    async fn test_service_timeout() {
        let mgr = ServiceManager::new();

        // service that will never be ready (wrong port)
        let task = Task {
            name: "timeout-test".to_string(),
            run: Some("sleep 30".to_string()),
            depends_on: vec![],
            service_deps: vec![],
            pipe_from: vec![],
            timeout: None,
            retry: 0,
            join: false,
            ssh: None,
            k8s: None,
            shebang: None,
            service: Some(ServiceConfig {
                kind: ServiceKind::Managed,
                ready: Some(ReadinessCheck::Tcp {
                    host: "127.0.0.1".to_string(),
                    port: 19999, // nothing listening here
                }),
                startup_timeout: Duration::from_secs(1), // short timeout
                shutdown_grace: Duration::from_secs(1),
                shutdown_kill: Duration::from_secs(1),
                interval: Duration::from_millis(100),
                log: LogOutput::Quiet,
                forward: false,
                preflight: None,
            }),
        };

        mgr.register(&task).await;

        let result = mgr.acquire("timeout-test").await;
        assert!(result.is_err(), "service should timeout");
        assert!(result.unwrap_err().contains("failed to become ready"));

        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn test_external_service() {
        // start a listener in background first
        let listener = std::process::Command::new("nc")
            .args(["-l", "19878"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("failed to start nc");

        // give it a moment to bind
        tokio::time::sleep(Duration::from_millis(100)).await;

        let mgr = ServiceManager::new();

        // external service - we don't start it, just wait for ready
        let task = Task {
            name: "external-test".to_string(),
            run: None, // no command for external
            depends_on: vec![],
            service_deps: vec![],
            pipe_from: vec![],
            timeout: None,
            retry: 0,
            join: false,
            ssh: None,
            k8s: None,
            shebang: None,
            service: Some(ServiceConfig {
                kind: ServiceKind::External,
                ready: Some(ReadinessCheck::Tcp {
                    host: "127.0.0.1".to_string(),
                    port: 19878,
                }),
                startup_timeout: Duration::from_secs(5),
                shutdown_grace: Duration::from_secs(1),
                shutdown_kill: Duration::from_secs(1),
                interval: Duration::from_millis(100),
                log: LogOutput::Quiet,
                forward: false,
                preflight: None,
            }),
        };

        mgr.register(&task).await;

        let result = mgr.acquire("external-test").await;
        assert!(result.is_ok(), "external service should be detected as ready");

        // release shouldn't kill the external service
        mgr.release("external-test").await;

        // the nc process should still be running (we started it, not justflow)
        // clean it up ourselves
        drop(listener);
    }

    #[tokio::test]
    async fn test_ref_counting() {
        let mgr = ServiceManager::new();

        let task = make_service_task(
            "refcount-test",
            "nc -l 19879",
            ReadinessCheck::Tcp {
                host: "127.0.0.1".to_string(),
                port: 19879,
            },
        );

        mgr.register(&task).await;

        // acquire twice
        let _ = mgr.acquire("refcount-test").await.unwrap();
        let _ = mgr.acquire("refcount-test").await.unwrap();

        // release once - should still be running
        mgr.release("refcount-test").await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(mgr.state("refcount-test").await, Some(ServiceState::Ready));

        // release again - now it should stop
        mgr.release("refcount-test").await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(mgr.state("refcount-test").await, Some(ServiceState::Stopped));
    }

    #[tokio::test]
    async fn test_ssh_service_registration() {
        // test that a service with SSH config can be registered
        // (actual SSH tests require connectivity, but we can test the plumbing)
        use crate::config::SshConfig;

        let mgr = ServiceManager::new();

        let task = Task {
            name: "ssh-service-test".to_string(),
            run: Some("python3 -m http.server 8888".to_string()),
            depends_on: vec![],
            service_deps: vec![],
            pipe_from: vec![],
            timeout: None,
            retry: 0,
            join: false,
            ssh: Some(SshConfig {
                host: "test-host".to_string(),
                user: Some("testuser".to_string()),
                port: Some(22),
                identity: None,
                workdir: Some("/app".to_string()),
                upload: vec![],
                download: vec![],
            }),
            k8s: None,
            shebang: None,
            service: Some(ServiceConfig {
                kind: ServiceKind::Managed,
                ready: Some(ReadinessCheck::Http {
                    url: "http://test-host:8888/".to_string(),
                }),
                startup_timeout: Duration::from_secs(30),
                shutdown_grace: Duration::from_secs(5),
                shutdown_kill: Duration::from_secs(10),
                interval: Duration::from_secs(1),
                log: LogOutput::Quiet,
                forward: false,
                preflight: None,
            }),
        };

        mgr.register(&task).await;

        // verify registration
        assert_eq!(mgr.state("ssh-service-test").await, Some(ServiceState::Stopped));

        // acquiring would fail without actual SSH, but that's expected
        // this test just verifies the registration path works
    }
}
