//! SSH remote execution support
//!
//! Execute tasks on remote hosts via SSH using the openssh crate.

use bytes::BytesMut;
use colored::Colorize;
use openssh::{ForwardType, KnownHosts, Session, SessionBuilder, Socket, Stdio};
use openssh_sftp_client::Sftp;
use std::collections::HashMap;
use std::io::IsTerminal;
use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::sync::RwLock;
use tracing::info;

use crate::progress::task_color;

use dagrun_ast::SshConfig;

/// Cache of SSH sessions for connection reuse
pub type SessionCache = Arc<RwLock<HashMap<String, Arc<Session>>>>;

pub fn new_session_cache() -> SessionCache {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Get or create an SSH session for the given config
pub async fn get_session(
    config: &SshConfig,
    cache: &SessionCache,
) -> Result<Arc<Session>, openssh::Error> {
    let key = config.destination();

    // check cache first
    {
        let cache_read = cache.read().await;
        if let Some(session) = cache_read.get(&key) {
            return Ok(session.clone());
        }
    }

    // create new session
    info!(host = %config.host, user = ?config.user, "establishing SSH connection");

    let mut builder = SessionBuilder::default();
    builder.known_hosts_check(KnownHosts::Accept);

    if let Some(port) = config.port {
        builder.port(port);
    }

    if let Some(ref identity) = config.identity {
        // expand ~ to home directory
        let path = if identity.starts_with('~') {
            if let Some(home) = dirs::home_dir() {
                identity.replacen('~', &home.to_string_lossy(), 1)
            } else {
                identity.clone()
            }
        } else {
            identity.clone()
        };
        builder.keyfile(&path);
    }

    let session = builder.connect(&config.destination()).await?;
    let session = Arc::new(session);

    // cache the session
    {
        let mut cache_write = cache.write().await;
        cache_write.insert(key, session.clone());
    }

    Ok(session)
}

/// Execute a command on a remote host with streaming output
pub async fn execute_remote(
    session: &Session,
    task_name: &str,
    command: &str,
    workdir: Option<&str>,
    stdin_data: Option<&str>,
) -> Result<RemoteOutput, openssh::Error> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let full_command = match workdir {
        Some(dir) => format!("cd {} && {}", dir, command),
        None => command.to_string(),
    };

    info!(command = %full_command, "executing remote command");

    let mut cmd = session.command("sh");
    cmd.arg("-c").arg(&full_command);

    if stdin_data.is_some() {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().await?;

    if let Some(data) = stdin_data
        && let Some(mut stdin) = child.stdin().take()
    {
        let _ = stdin.write_all(data.as_bytes()).await;
        drop(stdin);
    }

    // stream stdout and stderr in real-time
    let stdout_handle = child.stdout().take();
    let stderr_handle = child.stderr().take();

    let stdout_is_tty = std::io::stdout().is_terminal();
    let stderr_is_tty = std::io::stderr().is_terminal();

    let task_name_stdout = task_name.to_string();
    let color = task_color(task_name);
    let stdout_task = tokio::spawn(async move {
        let mut lines_collected = Vec::new();
        if let Some(stdout) = stdout_handle {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if stdout_is_tty {
                    println!(
                        "  {} {}",
                        format!("[{}]", task_name_stdout).color(color),
                        line
                    );
                } else {
                    println!("{}", line);
                }
                lines_collected.push(line);
            }
        }
        lines_collected.join("\n")
    });

    let task_name_stderr = task_name.to_string();
    let color = task_color(task_name);
    let stderr_task = tokio::spawn(async move {
        let mut lines_collected = Vec::new();
        if let Some(stderr) = stderr_handle {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if stderr_is_tty {
                    eprintln!(
                        "  {} {}",
                        format!("[{}]", task_name_stderr).color(color),
                        line
                    );
                } else {
                    eprintln!("{}", line);
                }
                lines_collected.push(line);
            }
        }
        lines_collected.join("\n")
    });

    let status = child.wait().await?;
    let stdout = stdout_task.await.unwrap_or_default();
    let stderr = stderr_task.await.unwrap_or_default();

    Ok(RemoteOutput {
        stdout,
        stderr,
        success: status.success(),
    })
}

#[allow(dead_code)]
pub struct RemoteOutput {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
}

/// Spawn a streaming command that runs indefinitely, printing output as it comes
/// The entire command lifecycle happens in the spawned task to avoid lifetime issues
pub fn spawn_streaming_command(
    session: Arc<Session>,
    task_name: String,
    command: String,
) -> tokio::task::JoinHandle<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let is_tty = std::io::stdout().is_terminal();
    let color = task_color(&task_name);

    tokio::spawn(async move {
        let mut cmd = session.command("sh");
        cmd.arg("-c").arg(&command);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::null());

        let Ok(mut child) = cmd.spawn().await else {
            return;
        };
        let stdout = child.stdout().take();

        if let Some(stdout) = stdout {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if is_tty {
                    println!("  {} {}", format!("[{}]", task_name).color(color), line);
                } else {
                    println!("{}", line);
                }
            }
        }
        let _ = child.wait().await;
    })
}

/// Close all cached sessions (sessions will be dropped, triggering cleanup)
pub async fn close_sessions(cache: &SessionCache) {
    let mut cache_write = cache.write().await;
    for (host, _session) in cache_write.drain() {
        info!(host = %host, "closing SSH connection");
        // session is dropped here, which closes the connection
    }
}

#[derive(Error, Debug)]
pub enum TransferError {
    #[error("ssh error: {0}")]
    Ssh(#[from] openssh::Error),
    #[error("sftp error: {0}")]
    Sftp(#[from] openssh_sftp_client::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Upload a local file to the remote host via SFTP
pub async fn upload_file(
    session: Arc<Session>,
    local_path: &str,
    remote_path: &str,
) -> Result<(), TransferError> {
    info!(local = %local_path, remote = %remote_path, "uploading file");

    let local = Path::new(local_path);
    let contents = tokio::fs::read(local).await?;

    let sftp = Sftp::from_clonable_session(session, Default::default()).await?;
    let mut remote_file = sftp.create(remote_path).await?;
    remote_file.write_all(&contents).await?;
    remote_file.close().await?;
    sftp.close().await?;

    info!(remote = %remote_path, bytes = contents.len(), "upload complete");
    Ok(())
}

/// Download a file from the remote host via SFTP
pub async fn download_file(
    session: Arc<Session>,
    remote_path: &str,
    local_path: &str,
) -> Result<(), TransferError> {
    info!(remote = %remote_path, local = %local_path, "downloading file");

    let sftp = Sftp::from_clonable_session(session, Default::default()).await?;
    let mut remote_file = sftp.open(remote_path).await?;

    let mut contents = Vec::new();
    loop {
        let buf = BytesMut::with_capacity(8192);
        match remote_file.read(8192, buf).await? {
            Some(data) => contents.extend_from_slice(&data),
            None => break,
        }
    }
    remote_file.close().await?;
    sftp.close().await?;

    let local = Path::new(local_path);
    if let Some(parent) = local.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(local, &contents).await?;

    info!(local = %local_path, bytes = contents.len(), "download complete");
    Ok(())
}

/// Find an available local port for forwarding
pub fn find_available_port() -> std::io::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    Ok(port)
}

/// Set up local port forwarding through SSH
/// Traffic to local_port will be forwarded to remote_host:remote_port on the remote side
pub async fn setup_port_forward(
    session: &Session,
    local_port: u16,
    remote_host: &str,
    remote_port: u16,
) -> Result<(), openssh::Error> {
    info!(
        local_port = local_port,
        remote_host = %remote_host,
        remote_port = remote_port,
        "setting up SSH port forward"
    );

    let local_socket = Socket::new("127.0.0.1", local_port);
    let remote_socket = Socket::new(remote_host, remote_port);

    session
        .request_port_forward(ForwardType::Local, local_socket, remote_socket)
        .await
}

/// Close an existing port forward
pub async fn close_port_forward(
    session: &Session,
    local_port: u16,
    remote_host: &str,
    remote_port: u16,
) -> Result<(), openssh::Error> {
    info!(
        local_port = local_port,
        remote_host = %remote_host,
        remote_port = remote_port,
        "closing SSH port forward"
    );

    let local_socket = Socket::new("127.0.0.1", local_port);
    let remote_socket = Socket::new(remote_host, remote_port);

    session
        .close_port_forward(ForwardType::Local, local_socket, remote_socket)
        .await
}
