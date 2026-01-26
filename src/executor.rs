#![allow(dead_code)]

use colored::Colorize;
use std::collections::HashMap;
use std::io::IsTerminal;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::RwLock;
use tokio::time::timeout;
use tracing::{error, info, warn};

use crate::progress::task_color;

use crate::dag::TaskGraph;
use crate::k8s::{self, ResourceTracker};
use crate::service::ServiceManager;
use crate::ssh::{self, SessionCache};
use dagrun_ast::{FileTransfer, Shebang, SshConfig, Task};
use glob::glob;
use shell_escape::escape;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;

#[derive(Error, Debug)]
pub enum ExecutorError {
    #[error("task '{0}' failed after {1} attempts")]
    TaskFailed(String, u32),
    #[error("task '{0}' timed out after {1:?}")]
    Timeout(String, Duration),
    #[error("task '{0}' not found")]
    TaskNotFound(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("dag error: {0}")]
    Dag(#[from] crate::dag::DagError),
    #[error("ssh error: {0}")]
    Ssh(String),
    #[error("k8s error: {0}")]
    K8s(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TaskStatus {
    Pending,
    Running,
    Success,
    Failed,
    Skipped,
}

#[derive(Clone)]
pub struct TaskResult {
    pub task_name: String,
    pub status: TaskStatus,
    pub attempts: u32,
    pub output: String,
}

/// shared state for tracking task outputs during execution
type OutputStore = Arc<RwLock<HashMap<String, String>>>;

pub struct Executor {
    pub graph: TaskGraph,
    outputs: OutputStore,
    ssh_sessions: SessionCache,
    services: Arc<ServiceManager>,
    k8s_tracker: ResourceTracker,
}

impl Executor {
    pub fn new(graph: TaskGraph) -> Self {
        let ssh_sessions = ssh::new_session_cache();
        Executor {
            graph,
            outputs: Arc::new(RwLock::new(HashMap::new())),
            ssh_sessions: ssh_sessions.clone(),
            services: Arc::new(ServiceManager::with_ssh_cache(ssh_sessions)),
            k8s_tracker: k8s::new_tracker(),
        }
    }

    /// Register all services from the graph
    pub async fn register_services(&self) {
        for name in self.graph.task_names() {
            if let Some(task) = self.graph.task(name)
                && task.service.is_some()
            {
                self.services.register(task).await;
            }
        }
    }

    /// Close all SSH connections, stop services, and cleanup K8s resources
    pub async fn close(&self) {
        self.services.shutdown().await;
        self.k8s_tracker.read().await.cleanup_all().await;
        ssh::close_sessions(&self.ssh_sessions).await;
    }

    pub async fn run_task(&self, target: &str) -> Result<Vec<TaskResult>, ExecutorError> {
        let tasks = self.graph.execution_order_for(target)?;
        self.execute_sequential(tasks).await
    }

    /// Run task with positional arguments bound to parameters
    pub async fn run_task_with_args(
        &self,
        target: &str,
        args: &[String],
    ) -> Result<Vec<TaskResult>, ExecutorError> {
        let tasks = self.graph.execution_order_for(target)?;

        // if no args, just run normally
        if args.is_empty() {
            return self.execute_sequential(tasks).await;
        }

        // build name->value mapping from target task's parameters
        let target_task = self
            .graph
            .task(target)
            .ok_or_else(|| ExecutorError::TaskNotFound(target.to_string()))?;
        let bindings = build_param_bindings(target_task, args)
            .map_err(|_| ExecutorError::TaskFailed(target.to_string(), 0))?;

        // apply bindings to all tasks in the chain
        let mut results = Vec::new();
        for task in tasks {
            let task_to_run = apply_bindings(task, &bindings)
                .map_err(|_| ExecutorError::TaskFailed(task.name.clone(), 0))?;

            let result = self.execute_single(&task_to_run).await;
            let failed = result.status == TaskStatus::Failed;
            results.push(result);
            if failed {
                break;
            }
        }

        Ok(results)
    }

    pub async fn run_all(&self) -> Result<Vec<TaskResult>, ExecutorError> {
        let groups = self.graph.parallel_groups()?;
        let mut all_results = Vec::new();

        for group in groups {
            let results = self.execute_parallel(group).await?;

            let any_failed = results.iter().any(|r| r.status == TaskStatus::Failed);
            all_results.extend(results);

            if any_failed {
                break;
            }
        }

        Ok(all_results)
    }

    async fn execute_sequential(
        &self,
        tasks: Vec<&Task>,
    ) -> Result<Vec<TaskResult>, ExecutorError> {
        let mut results = Vec::new();

        for task in tasks {
            // acquire service dependencies
            let mut service_env = HashMap::new();
            let mut service_failed = None;
            for svc_name in &task.service_deps {
                match self.services.acquire(svc_name).await {
                    Ok(env) => service_env.extend(env),
                    Err(e) => {
                        service_failed = Some(e);
                        break;
                    }
                }
            }

            let result = if let Some(err) = service_failed {
                TaskResult {
                    task_name: task.name.clone(),
                    status: TaskStatus::Failed,
                    attempts: 0,
                    output: err,
                }
            } else {
                let stdin_data = self.collect_pipe_inputs(task).await;
                execute_with_retry(
                    task,
                    stdin_data.as_deref(),
                    &self.ssh_sessions,
                    &service_env,
                    &self.k8s_tracker,
                )
                .await
            };

            // release service dependencies
            for svc_name in &task.service_deps {
                self.services.release(svc_name).await;
            }

            // store output for downstream tasks
            self.outputs
                .write()
                .await
                .insert(task.name.clone(), result.output.clone());

            let failed = result.status == TaskStatus::Failed;
            results.push(result);

            if failed {
                break;
            }
        }

        Ok(results)
    }

    async fn execute_parallel(&self, tasks: Vec<&Task>) -> Result<Vec<TaskResult>, ExecutorError> {
        let handles: Vec<_> = tasks
            .into_iter()
            .map(|task| {
                let task = task.clone();
                let outputs = self.outputs.clone();
                let ssh_sessions = self.ssh_sessions.clone();
                let services = self.services.clone();
                let k8s_tracker = self.k8s_tracker.clone();

                tokio::spawn(async move {
                    // acquire service dependencies
                    let mut service_env = HashMap::new();
                    let mut service_failed = None;
                    for svc_name in &task.service_deps {
                        match services.acquire(svc_name).await {
                            Ok(env) => service_env.extend(env),
                            Err(e) => {
                                service_failed = Some(e);
                                break;
                            }
                        }
                    }

                    let result = if let Some(err) = service_failed {
                        TaskResult {
                            task_name: task.name.clone(),
                            status: TaskStatus::Failed,
                            attempts: 0,
                            output: err,
                        }
                    } else {
                        let stdin_data = collect_pipe_inputs_from_store(&task, &outputs).await;
                        execute_with_retry(
                            &task,
                            stdin_data.as_deref(),
                            &ssh_sessions,
                            &service_env,
                            &k8s_tracker,
                        )
                        .await
                    };

                    // release service dependencies
                    for svc_name in &task.service_deps {
                        services.release(svc_name).await;
                    }

                    outputs
                        .write()
                        .await
                        .insert(task.name.clone(), result.output.clone());

                    result
                })
            })
            .collect();

        let mut results = Vec::new();
        for handle in handles {
            results.push(handle.await.unwrap());
        }

        Ok(results)
    }

    async fn collect_pipe_inputs(&self, task: &Task) -> Option<String> {
        collect_pipe_inputs_from_store(task, &self.outputs).await
    }

    pub async fn execute_single(&self, task: &Task) -> TaskResult {
        let stdin_data = self.collect_pipe_inputs(task).await;
        execute_with_retry(
            task,
            stdin_data.as_deref(),
            &self.ssh_sessions,
            &HashMap::new(),
            &self.k8s_tracker,
        )
        .await
    }
}

async fn collect_pipe_inputs_from_store(task: &Task, outputs: &OutputStore) -> Option<String> {
    if task.pipe_from.is_empty() {
        return None;
    }

    let store = outputs.read().await;
    let mut combined = String::new();

    for source in &task.pipe_from {
        if let Some(output) = store.get(source) {
            combined.push_str(output);
        }
    }

    if combined.is_empty() {
        None
    } else {
        Some(combined)
    }
}

async fn execute_with_retry(
    task: &Task,
    stdin_data: Option<&str>,
    ssh_sessions: &SessionCache,
    service_env: &HashMap<String, String>,
    k8s_tracker: &ResourceTracker,
) -> TaskResult {
    let max_attempts = task.retry + 1;
    let mut output = String::new();

    for attempt in 1..=max_attempts {
        info!(
            task = %task.name,
            progress = "start",
            attempt,
            max_attempts,
            "running task"
        );

        let start = Instant::now();
        match execute_once(task, stdin_data, ssh_sessions, service_env, k8s_tracker).await {
            Ok(task_output) => {
                let duration_ms = start.elapsed().as_millis() as u64;
                info!(
                    task = %task.name,
                    progress = "done",
                    duration_ms,
                    "task succeeded"
                );
                return TaskResult {
                    task_name: task.name.clone(),
                    status: TaskStatus::Success,
                    attempts: attempt,
                    output: task_output,
                };
            }
            Err(e) => {
                output = format!("{}", e);
                if attempt < max_attempts {
                    warn!(
                        task = %task.name,
                        progress = "retry",
                        attempt,
                        error = %e,
                        "task failed, retrying"
                    );
                } else {
                    error!(
                        task = %task.name,
                        progress = "failed",
                        attempts = max_attempts,
                        error = %e,
                        "task failed permanently"
                    );
                }
            }
        }
    }

    TaskResult {
        task_name: task.name.clone(),
        status: TaskStatus::Failed,
        attempts: max_attempts,
        output,
    }
}

async fn execute_once(
    task: &Task,
    stdin_data: Option<&str>,
    ssh_sessions: &SessionCache,
    service_env: &HashMap<String, String>,
    k8s_tracker: &ResourceTracker,
) -> Result<String, ExecutorError> {
    // handle join nodes - just pass through the stdin as output
    if task.is_join() {
        info!(task = %task.name, "join node - passing through input");
        return Ok(stdin_data.unwrap_or("").to_string());
    }

    let cmd = task.run.as_deref().unwrap_or("");

    // route to K8s if configured
    if let Some(ref k8s_config) = task.k8s {
        // wrap shebang scripts for remote execution
        let cmd = if let Some(ref shebang) = task.shebang {
            wrap_shebang_for_remote(cmd, shebang)
        } else {
            cmd.to_string()
        };

        let result = k8s::execute(
            k8s_config,
            &task.name,
            &cmd,
            stdin_data,
            task.timeout,
            k8s_tracker,
        )
        .await
        .map_err(|e| ExecutorError::K8s(e.to_string()))?;

        if result.success {
            return Ok(result.stdout);
        } else {
            return Err(ExecutorError::TaskFailed(task.name.clone(), 1));
        }
    }

    // route to SSH if configured
    if let Some(ref ssh_config) = task.ssh {
        let cmd = task.run.as_ref().unwrap();
        // wrap shebang scripts for remote execution
        let cmd = if let Some(ref shebang) = task.shebang {
            wrap_shebang_for_remote(cmd, shebang)
        } else {
            cmd.to_string()
        };
        return execute_remote(
            task,
            &cmd,
            stdin_data,
            ssh_config,
            ssh_sessions,
            service_env,
        )
        .await;
    }

    let cmd = task.run.as_ref().unwrap();

    // for shebang scripts, create a temp file and execute it directly
    let future = async {
        let mut child = if let Some(ref shebang) = task.shebang {
            let script_file = create_script_file(cmd, &task.name)?;
            let script_path = script_file.path().to_string_lossy().to_string();

            // build command: interpreter [args...] script_path
            let mut cmd_builder = Command::new(&shebang.interpreter);
            for arg in &shebang.args {
                cmd_builder.arg(arg);
            }
            cmd_builder.arg(&script_path);

            // keep the temp file alive until command completes (moved into closure)
            let child = cmd_builder
                .envs(service_env)
                .stdin(if stdin_data.is_some() {
                    Stdio::piped()
                } else {
                    Stdio::null()
                })
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;

            // leak the script file handle so it stays around during execution
            // it will be cleaned up when the NamedTempFile is dropped at end of scope
            std::mem::forget(script_file);
            child
        } else {
            Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .envs(service_env)
                .stdin(if stdin_data.is_some() {
                    Stdio::piped()
                } else {
                    Stdio::null()
                })
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?
        };

        // write stdin if we have data to pipe
        if let Some(data) = stdin_data
            && let Some(mut stdin) = child.stdin.take()
        {
            let data = data.to_string();
            tokio::spawn(async move {
                let _ = stdin.write_all(data.as_bytes()).await;
            });
        }

        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        let stdout_is_tty = std::io::stdout().is_terminal();
        let stderr_is_tty = std::io::stderr().is_terminal();

        let task_name = task.name.clone();
        let color = task_color(&task_name);
        let stdout_handle = tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            let mut collected = String::new();
            while let Ok(Some(line)) = lines.next_line().await {
                if stdout_is_tty {
                    println!("  {} {}", format!("[{}]", task_name).color(color), line);
                } else {
                    println!("{}", line);
                }
                collected.push_str(&line);
                collected.push('\n');
            }
            collected
        });

        let task_name = task.name.clone();
        let color = task_color(&task_name);
        let stderr_handle = tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if stderr_is_tty {
                    eprintln!("  {} {}", format!("[{}]", task_name).color(color), line);
                } else {
                    eprintln!("{}", line);
                }
            }
        });

        let status = child.wait().await?;
        let output = stdout_handle.await.unwrap();
        stderr_handle.await.unwrap();

        if status.success() {
            Ok(output)
        } else {
            Err(ExecutorError::TaskFailed(task.name.clone(), 1))
        }
    };

    if let Some(task_timeout) = task.timeout {
        timeout(task_timeout, future)
            .await
            .map_err(|_| ExecutorError::Timeout(task.name.clone(), task_timeout))?
    } else {
        future.await
    }
}

/// Execute a task on a remote host via SSH
async fn execute_remote(
    task: &Task,
    cmd: &str,
    stdin_data: Option<&str>,
    ssh_config: &SshConfig,
    ssh_sessions: &SessionCache,
    service_env: &HashMap<String, String>,
) -> Result<String, ExecutorError> {
    let session = ssh::get_session(ssh_config, ssh_sessions)
        .await
        .map_err(|e| ExecutorError::Ssh(e.to_string()))?;

    info!(
        task = %task.name,
        host = %ssh_config.host,
        "executing on remote host"
    );

    // upload files before command execution (expanding globs)
    let uploads = expand_upload_globs(&ssh_config.upload);
    for transfer in &uploads {
        ssh::upload_file(session.clone(), &transfer.local, &transfer.remote)
            .await
            .map_err(|e| ExecutorError::Ssh(format!("upload failed: {}", e)))?;
    }

    // prepend service env vars to remote command (ssh protocol env requests are
    // typically rejected by servers unless AcceptEnv is configured in sshd_config)
    let cmd_with_env = if service_env.is_empty() {
        cmd.to_string()
    } else {
        let exports: Vec<String> = service_env
            .iter()
            .map(|(k, v)| format!("export {}={}", k, escape(v.into())))
            .collect();
        format!("{} && {}", exports.join(" && "), cmd)
    };

    let result = ssh::execute_remote(
        &session,
        &task.name,
        &cmd_with_env,
        ssh_config.workdir.as_deref(),
        stdin_data,
    )
    .await
    .map_err(|e| ExecutorError::Ssh(e.to_string()))?;

    // download files after command execution (only on success)
    if result.success {
        for transfer in &ssh_config.download {
            ssh::download_file(session.clone(), &transfer.remote, &transfer.local)
                .await
                .map_err(|e| ExecutorError::Ssh(format!("download failed: {}", e)))?;
        }
    }

    if result.success {
        Ok(result.stdout)
    } else {
        Err(ExecutorError::TaskFailed(task.name.clone(), 1))
    }
}

/// create a temp script file with the given content, make it executable, return the path
fn create_script_file(content: &str, task_name: &str) -> std::io::Result<tempfile::NamedTempFile> {
    let mut file = tempfile::Builder::new()
        .prefix(&format!("justflow-{}-", task_name))
        .suffix(".sh")
        .tempfile()?;

    file.write_all(content.as_bytes())?;
    file.flush()?;

    // make executable
    let mut perms = file.as_file().metadata()?.permissions();
    perms.set_mode(0o755);
    file.as_file().set_permissions(perms)?;

    Ok(file)
}

/// wrap a shebang script for remote execution (SSH/K8s) using heredoc
fn wrap_shebang_for_remote(script: &str, shebang: &Shebang) -> String {
    // generate a unique-ish temp path
    let script_path = "/tmp/justflow_script_$$.sh";

    // build the interpreter command
    let interpreter_cmd = if shebang.args.is_empty() {
        shebang.interpreter.clone()
    } else {
        format!("{} {}", shebang.interpreter, shebang.args.join(" "))
    };

    // create a self-contained command that:
    // 1. writes script to temp file via heredoc
    // 2. makes it executable
    // 3. runs it with the interpreter
    // 4. cleans up
    format!(
        r#"_jf_script="{script_path}"
cat > "$_jf_script" << 'JUSTFLOW_SCRIPT_EOF'
{script}
JUSTFLOW_SCRIPT_EOF
chmod +x "$_jf_script"
{interpreter_cmd} "$_jf_script"
_jf_exit=$?
rm -f "$_jf_script"
exit $_jf_exit"#,
        script_path = script_path,
        script = script,
        interpreter_cmd = interpreter_cmd
    )
}

/// expand glob patterns in upload file transfers
fn expand_upload_globs(transfers: &[FileTransfer]) -> Vec<FileTransfer> {
    let mut result = Vec::new();

    for transfer in transfers {
        let has_glob = transfer.local.contains('*')
            || transfer.local.contains('?')
            || transfer.local.contains('[');

        if !has_glob {
            result.push(transfer.clone());
            continue;
        }

        match glob(&transfer.local) {
            Ok(paths) => {
                let matched: Vec<_> = paths.filter_map(|p| p.ok()).collect();

                if matched.is_empty() {
                    warn!(pattern = %transfer.local, "glob pattern matched no files");
                    continue;
                }

                if matched.len() > 1 && !transfer.remote.ends_with('/') {
                    warn!(
                        pattern = %transfer.local,
                        "glob matched multiple files but remote path doesn't end with /"
                    );
                }

                for path in matched {
                    let filename = path
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();

                    let remote = if transfer.remote.ends_with('/') {
                        format!("{}{}", transfer.remote, filename)
                    } else {
                        format!("{}/{}", transfer.remote, filename)
                    };

                    result.push(FileTransfer {
                        local: path.to_string_lossy().to_string(),
                        remote,
                    });
                }
            }
            Err(e) => {
                warn!(pattern = %transfer.local, error = %e, "invalid glob pattern");
            }
        }
    }

    result
}

/// Build a name->value mapping from positional args and task parameters
fn build_param_bindings(task: &Task, args: &[String]) -> Result<HashMap<String, String>, String> {
    let params = &task.parameters;

    // validate argument count
    let required_count = params.iter().filter(|p| p.default.is_none()).count();
    if args.len() < required_count {
        let param_names: Vec<_> = params
            .iter()
            .filter(|p| p.default.is_none())
            .map(|p| p.name.as_str())
            .collect();
        return Err(format!(
            "Task '{}' requires {} argument(s) ({}) but got {}",
            task.name,
            required_count,
            param_names.join(", "),
            args.len()
        ));
    }
    if args.len() > params.len() {
        return Err(format!(
            "Task '{}' accepts {} argument(s) but got {}",
            task.name,
            params.len(),
            args.len()
        ));
    }

    // build parameter bindings
    let mut bindings = HashMap::new();
    for (i, param) in params.iter().enumerate() {
        let value = if i < args.len() {
            args[i].clone()
        } else if let Some(default) = &param.default {
            default.clone()
        } else {
            return Err(format!("Missing required argument '{}'", param.name));
        };
        bindings.insert(param.name.clone(), value);
    }

    Ok(bindings)
}

/// Apply pre-built bindings to a task, using defaults for unbound params
fn apply_bindings(task: &Task, bindings: &HashMap<String, String>) -> Result<Task, String> {
    let params = &task.parameters;

    // build final bindings for this task
    let mut task_bindings = HashMap::new();
    for param in params {
        let value = if let Some(v) = bindings.get(&param.name) {
            v.clone()
        } else if let Some(default) = &param.default {
            default.clone()
        } else {
            return Err(format!(
                "Task '{}' requires parameter '{}' but it was not provided",
                task.name, param.name
            ));
        };
        task_bindings.insert(param.name.clone(), value);
    }

    // substitute parameters in task body
    let run = task.run.as_ref().map(|body| {
        let mut result = body.clone();
        for (name, value) in &task_bindings {
            let pattern = format!("{{{{{}}}}}", name);
            result = result.replace(&pattern, value);
        }
        result
    });

    Ok(Task {
        run,
        ..task.clone()
    })
}

/// Bind positional args to task parameters (for --only mode)
fn bind_task_params(task: &Task, args: &[String]) -> Result<Task, String> {
    let bindings = build_param_bindings(task, args)?;
    apply_bindings(task, &bindings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_expand_upload_globs_no_glob() {
        let transfers = vec![FileTransfer {
            local: "./file.txt".to_string(),
            remote: "/remote/file.txt".to_string(),
        }];

        let result = expand_upload_globs(&transfers);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].local, "./file.txt");
        assert_eq!(result[0].remote, "/remote/file.txt");
    }

    #[test]
    fn test_expand_upload_globs_with_pattern() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        fs::write(dir_path.join("a.rs"), "").unwrap();
        fs::write(dir_path.join("b.rs"), "").unwrap();
        fs::write(dir_path.join("c.txt"), "").unwrap();

        let pattern = format!("{}/*.rs", dir_path.display());
        let transfers = vec![FileTransfer {
            local: pattern,
            remote: "/remote/src/".to_string(),
        }];

        let result = expand_upload_globs(&transfers);
        assert_eq!(result.len(), 2);

        let locals: Vec<_> = result.iter().map(|t| &t.local).collect();
        assert!(locals.iter().any(|l| l.ends_with("a.rs")));
        assert!(locals.iter().any(|l| l.ends_with("b.rs")));

        for t in &result {
            assert!(t.remote.starts_with("/remote/src/"));
            assert!(t.remote.ends_with(".rs"));
        }
    }
}
