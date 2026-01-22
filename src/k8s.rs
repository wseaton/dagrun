//! Kubernetes remote execution support using kube-rs
//!
//! Execute tasks in Kubernetes clusters via:
//! - kubectl exec into existing pods
//! - ephemeral Job creation with completion waiting
//! - kubectl apply for manifest folders with cleanup

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::RwLock;
use tracing::{info, warn};

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{Container, PodSpec, PodTemplateSpec, EnvVar, ConfigMapVolumeSource, SecretVolumeSource, Volume, VolumeMount};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, DeleteParams, PostParams, LogParams};
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::{Client, Config};
use kube::runtime::wait::{await_condition, conditions};

use crate::config::{K8sConfig, K8sMode, PortForward};
use tokio::process::Child;

#[derive(Error, Debug)]
pub enum K8sError {
    #[error("kube error: {0}")]
    Kube(#[from] kube::Error),
    #[error("config error: {0}")]
    Config(#[from] kube::config::KubeconfigError),
    #[error("pod not found: {0}")]
    PodNotFound(String),
    #[error("job '{0}' failed")]
    JobFailed(String),
    #[error("job '{0}' timed out after {1:?}")]
    JobTimeout(String, Duration),
    #[error("missing required field: {0}")]
    MissingField(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("infer config error: {0}")]
    InferConfig(#[from] kube::config::InferConfigError),
}

/// Result of K8s task execution
pub struct K8sOutput {
    pub stdout: String,
    pub success: bool,
}

/// Tracks applied manifests for cleanup on shutdown
struct AppliedManifest {
    path: String,
    namespace: String,
    context: Option<String>,
}

/// Tracks running jobs for cleanup on Ctrl+C
struct TrackedJob {
    name: String,
    namespace: String,
    context: Option<String>,
}

/// Tracks K8s resources created during workflow for cleanup
pub struct K8sResourceTracker {
    applied: Vec<AppliedManifest>,
    jobs: Vec<TrackedJob>,
}

impl K8sResourceTracker {
    pub fn new() -> Self {
        Self {
            applied: vec![],
            jobs: vec![],
        }
    }

    /// Record that manifests were applied
    pub fn track_apply(&mut self, config: &K8sConfig) {
        if let Some(ref path) = config.path {
            self.applied.push(AppliedManifest {
                path: path.clone(),
                namespace: config.namespace.clone(),
                context: config.context.clone(),
            });
        }
    }

    /// Record a job was created
    pub fn track_job(&mut self, job_name: &str, config: &K8sConfig) {
        self.jobs.push(TrackedJob {
            name: job_name.to_string(),
            namespace: config.namespace.clone(),
            context: config.context.clone(),
        });
    }

    /// Untrack job after normal completion
    pub fn untrack_job(&mut self, job_name: &str) {
        self.jobs.retain(|j| j.name != job_name);
    }

    /// Cleanup all tracked resources (called on shutdown)
    pub async fn cleanup_all(&self) {
        // delete jobs first
        for job in &self.jobs {
            info!(job = %job.name, namespace = %job.namespace, "cleaning up job");
            if let Ok(client) = get_client(job.context.as_deref()).await {
                let jobs: Api<Job> = Api::namespaced(client, &job.namespace);
                let _ = jobs.delete(&job.name, &DeleteParams::default()).await;
            }
        }

        // delete applied manifests (via kubectl for now - kube-rs doesn't have apply)
        for manifest in self.applied.iter().rev() {
            info!(path = %manifest.path, namespace = %manifest.namespace, "cleaning up applied manifests");
            let _ = delete_manifests_kubectl(&manifest.path, &manifest.namespace, manifest.context.as_deref()).await;
        }
    }
}

pub type ResourceTracker = Arc<RwLock<K8sResourceTracker>>;

pub fn new_tracker() -> ResourceTracker {
    Arc::new(RwLock::new(K8sResourceTracker::new()))
}

/// Get a kube client for the given context (or default)
async fn get_client(context: Option<&str>) -> Result<Client, K8sError> {
    let config = if let Some(ctx) = context {
        let kubeconfig = Kubeconfig::read()?;
        let options = KubeConfigOptions {
            context: Some(ctx.to_string()),
            ..Default::default()
        };
        Config::from_custom_kubeconfig(kubeconfig, &options).await?
    } else {
        Config::infer().await?
    };

    Ok(Client::try_from(config)?)
}

/// Generate unique job name from task name
fn generate_job_name(task_name: &str) -> String {
    let suffix: String = (0..6)
        .map(|_| fastrand::alphanumeric())
        .collect::<String>()
        .to_lowercase();
    let sanitized = sanitize_k8s_name(task_name);
    format!("{}-{}", sanitized, suffix)
}

/// Sanitize name for K8s (lowercase, alphanumeric, dashes, max 63 chars)
fn sanitize_k8s_name(s: &str) -> String {
    let sanitized: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '-' })
        .collect();
    let trimmed = sanitized.trim_matches('-');
    if trimmed.len() > 50 {
        trimmed[..50].trim_end_matches('-').to_string()
    } else {
        trimmed.to_string()
    }
}

/// Build a K8s Job object
fn build_job(config: &K8sConfig, job_name: &str, command: &str) -> Result<Job, K8sError> {
    let image = config
        .image
        .as_ref()
        .ok_or_else(|| K8sError::MissingField("image required for job mode".into()))?;

    // workdir prefix
    let full_command = match &config.workdir {
        Some(dir) => format!("cd {} && {}", dir, command),
        None => command.to_string(),
    };

    // build resource requirements
    let resources = if config.cpu.is_some() || config.memory.is_some() {
        let mut requests = BTreeMap::new();
        let mut limits = BTreeMap::new();

        if let Some(ref cpu) = config.cpu {
            requests.insert("cpu".to_string(), k8s_openapi::apimachinery::pkg::api::resource::Quantity(cpu.clone()));
            limits.insert("cpu".to_string(), k8s_openapi::apimachinery::pkg::api::resource::Quantity(cpu.clone()));
        }
        if let Some(ref mem) = config.memory {
            requests.insert("memory".to_string(), k8s_openapi::apimachinery::pkg::api::resource::Quantity(mem.clone()));
            limits.insert("memory".to_string(), k8s_openapi::apimachinery::pkg::api::resource::Quantity(mem.clone()));
        }

        Some(k8s_openapi::api::core::v1::ResourceRequirements {
            requests: Some(requests),
            limits: Some(limits),
            ..Default::default()
        })
    } else {
        None
    };

    // build volume mounts and volumes
    let mut volume_mounts = Vec::new();
    let mut volumes = Vec::new();

    for cm in &config.configmaps {
        let vol_name = format!("cm-{}", sanitize_k8s_name(&cm.name));
        volume_mounts.push(VolumeMount {
            name: vol_name.clone(),
            mount_path: cm.mount_path.clone(),
            ..Default::default()
        });
        volumes.push(Volume {
            name: vol_name,
            config_map: Some(ConfigMapVolumeSource {
                name: cm.name.clone(),
                ..Default::default()
            }),
            ..Default::default()
        });
    }

    for secret in &config.secrets {
        let vol_name = format!("secret-{}", sanitize_k8s_name(&secret.name));
        volume_mounts.push(VolumeMount {
            name: vol_name.clone(),
            mount_path: secret.mount_path.clone(),
            ..Default::default()
        });
        volumes.push(Volume {
            name: vol_name,
            secret: Some(SecretVolumeSource {
                secret_name: Some(secret.name.clone()),
                ..Default::default()
            }),
            ..Default::default()
        });
    }

    // build environment variables from service env (passed in later)
    let env: Vec<EnvVar> = vec![];

    // build node selector (convert HashMap to BTreeMap)
    let node_selector: Option<BTreeMap<String, String>> = config
        .node_selector
        .as_ref()
        .map(|h| h.iter().map(|(k, v)| (k.clone(), v.clone())).collect());

    // build tolerations
    let tolerations: Vec<k8s_openapi::api::core::v1::Toleration> = config
        .tolerations
        .iter()
        .map(|key| k8s_openapi::api::core::v1::Toleration {
            key: Some(key.clone()),
            operator: Some("Exists".to_string()),
            effect: Some("NoSchedule".to_string()),
            ..Default::default()
        })
        .collect();

    let container = Container {
        name: "task".to_string(),
        image: Some(image.clone()),
        command: Some(vec!["sh".to_string(), "-c".to_string()]),
        args: Some(vec![full_command]),
        resources,
        volume_mounts: if volume_mounts.is_empty() { None } else { Some(volume_mounts) },
        env: if env.is_empty() { None } else { Some(env) },
        ..Default::default()
    };

    let mut labels = BTreeMap::new();
    labels.insert("justflow.task".to_string(), job_name.to_string());

    let job = Job {
        metadata: ObjectMeta {
            name: Some(job_name.to_string()),
            namespace: Some(config.namespace.clone()),
            labels: Some(labels.clone()),
            ..Default::default()
        },
        spec: Some(k8s_openapi::api::batch::v1::JobSpec {
            ttl_seconds_after_finished: config.ttl_seconds.map(|t| t as i32),
            backoff_limit: Some(0),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    restart_policy: Some("Never".to_string()),
                    service_account_name: config.service_account.clone(),
                    node_selector,
                    tolerations: if tolerations.is_empty() { None } else { Some(tolerations) },
                    containers: vec![container],
                    volumes: if volumes.is_empty() { None } else { Some(volumes) },
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    };

    Ok(job)
}

/// Create and run an ephemeral Job
pub async fn run_job(
    config: &K8sConfig,
    task_name: &str,
    command: &str,
    task_timeout: Option<Duration>,
    tracker: &ResourceTracker,
) -> Result<K8sOutput, K8sError> {
    let job_name = generate_job_name(task_name);
    let client = get_client(config.context.as_deref()).await?;
    let jobs: Api<Job> = Api::namespaced(client.clone(), &config.namespace);

    info!(task = %task_name, job = %job_name, namespace = %config.namespace, "creating ephemeral job");

    // build and create job
    let job = build_job(config, &job_name, command)?;
    jobs.create(&PostParams::default(), &job).await?;

    // track for cleanup
    tracker.write().await.track_job(&job_name, config);

    // wait for completion
    let timeout_duration = task_timeout.unwrap_or(Duration::from_secs(3600));

    let cond = await_condition(jobs.clone(), &job_name, conditions::is_job_completed());
    let result = tokio::time::timeout(timeout_duration, cond).await;

    // get logs regardless of outcome
    let logs = get_job_logs(&client, &config.namespace, &job_name).await.unwrap_or_default();

    // print logs
    for line in logs.lines() {
        println!("[{}] {}", task_name, line);
    }

    // check result
    let success = match result {
        Ok(Ok(Some(job))) => {
            // check if succeeded
            job.status
                .as_ref()
                .and_then(|s| s.succeeded)
                .map(|s| s > 0)
                .unwrap_or(false)
        }
        Ok(Ok(None)) => false,
        Ok(Err(e)) => {
            warn!(job = %job_name, error = %e, "job watch error");
            false
        }
        Err(_) => {
            warn!(job = %job_name, "job timed out");
            // cleanup on timeout
            let _ = jobs.delete(&job_name, &DeleteParams::default()).await;
            tracker.write().await.untrack_job(&job_name);
            return Err(K8sError::JobTimeout(job_name, timeout_duration));
        }
    };

    // cleanup (TTL will also handle it)
    let _ = jobs.delete(&job_name, &DeleteParams::default()).await;
    tracker.write().await.untrack_job(&job_name);

    if success {
        Ok(K8sOutput { stdout: logs, success: true })
    } else {
        Ok(K8sOutput { stdout: logs, success: false })
    }
}

/// Get logs from a job's pod
async fn get_job_logs(client: &Client, namespace: &str, job_name: &str) -> Result<String, K8sError> {
    use k8s_openapi::api::core::v1::Pod;

    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let label_selector = format!("job-name={}", job_name);

    // find pod for this job
    let pod_list = pods.list(&kube::api::ListParams::default().labels(&label_selector)).await?;

    if let Some(pod) = pod_list.items.first() {
        if let Some(ref name) = pod.metadata.name {
            let logs = pods.logs(name, &LogParams::default()).await?;
            return Ok(logs);
        }
    }

    Ok(String::new())
}

/// Apply manifests from a path (uses kubectl since kube-rs doesn't have apply)
pub async fn apply_manifests(config: &K8sConfig, tracker: &ResourceTracker) -> Result<(), K8sError> {
    let path = config
        .path
        .as_ref()
        .ok_or_else(|| K8sError::MissingField("path required for apply mode".into()))?;

    info!(path = %path, namespace = %config.namespace, "applying manifests");

    // use kubectl for apply (kube-rs doesn't support server-side apply of arbitrary manifests easily)
    let mut cmd = tokio::process::Command::new("kubectl");
    if let Some(ref ctx) = config.context {
        cmd.arg("--context").arg(ctx);
    }
    cmd.arg("-n").arg(&config.namespace);
    cmd.args(["apply", "-f", path]);

    let output = cmd.output().await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(K8sError::MissingField(format!("kubectl apply failed: {}", stderr)));
    }

    // print what was applied
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        info!("{}", line);
    }

    // track for cleanup
    tracker.write().await.track_apply(config);

    Ok(())
}

/// Wait for resources to become ready
pub async fn wait_for_resources(config: &K8sConfig) -> Result<(), K8sError> {
    if config.wait_for.is_empty() {
        return Ok(());
    }

    let timeout_duration = config.wait_timeout.unwrap_or(Duration::from_secs(300));

    // use kubectl wait for simplicity (handles deployments, statefulsets, etc.)
    for resource in &config.wait_for {
        info!(resource = %resource, timeout = ?timeout_duration, "waiting for resource");

        let mut cmd = tokio::process::Command::new("kubectl");
        if let Some(ref ctx) = config.context {
            cmd.arg("--context").arg(ctx);
        }
        cmd.arg("-n").arg(&config.namespace);
        cmd.args([
            "wait",
            "--for=condition=available",
            resource,
            &format!("--timeout={}s", timeout_duration.as_secs()),
        ]);

        let output = tokio::time::timeout(timeout_duration + Duration::from_secs(10), cmd.output()).await;

        match output {
            Ok(Ok(out)) => {
                if !out.status.success() {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    return Err(K8sError::MissingField(format!(
                        "failed waiting for {}: {}",
                        resource, stderr
                    )));
                }
                info!(resource = %resource, "resource ready");
            }
            Ok(Err(e)) => return Err(K8sError::Io(e)),
            Err(_) => {
                return Err(K8sError::MissingField(format!(
                    "timeout waiting for {}",
                    resource
                )));
            }
        }
    }

    Ok(())
}

/// Delete manifests via kubectl
async fn delete_manifests_kubectl(
    path: &str,
    namespace: &str,
    context: Option<&str>,
) -> Result<(), K8sError> {
    info!(path = %path, namespace = %namespace, "deleting manifests");

    let mut cmd = tokio::process::Command::new("kubectl");
    if let Some(ctx) = context {
        cmd.arg("--context").arg(ctx);
    }
    cmd.arg("-n").arg(namespace);
    cmd.args(["delete", "-f", path, "--ignore-not-found"]);

    let _ = cmd.output().await;
    Ok(())
}

/// Find a pod matching the selector or name (for exec mode)
pub async fn find_pod(config: &K8sConfig) -> Result<String, K8sError> {
    if let Some(ref pod_name) = config.pod {
        return Ok(pod_name.clone());
    }

    let selector = config
        .selector
        .as_ref()
        .ok_or_else(|| K8sError::PodNotFound("no selector or pod name".into()))?;

    let client = get_client(config.context.as_deref()).await?;

    use k8s_openapi::api::core::v1::Pod;
    let pods: Api<Pod> = Api::namespaced(client, &config.namespace);
    let pod_list = pods.list(&kube::api::ListParams::default().labels(selector)).await?;

    pod_list
        .items
        .first()
        .and_then(|p| p.metadata.name.clone())
        .ok_or_else(|| K8sError::PodNotFound(format!("no pods found with selector: {}", selector)))
}

/// Start a kubectl port-forward process in the background
/// Returns the child process handle for later cleanup
pub async fn start_port_forward(
    config: &K8sConfig,
    forward: &PortForward,
) -> Result<Child, K8sError> {
    let resource = match &forward.resource_type {
        Some(rt) => format!("{}/{}", rt, forward.resource),
        None => forward.resource.clone(), // assume it's a pod name
    };

    info!(
        resource = %resource,
        local_port = forward.local_port,
        remote_port = forward.remote_port,
        "starting port forward"
    );

    let mut cmd = tokio::process::Command::new("kubectl");
    if let Some(ref ctx) = config.context {
        cmd.arg("--context").arg(ctx);
    }
    cmd.arg("-n").arg(&config.namespace);
    cmd.arg("port-forward");
    cmd.arg(&resource);
    cmd.arg(format!("{}:{}", forward.local_port, forward.remote_port));

    // run in background, suppress output
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());

    let child = cmd.spawn()?;

    // give it a moment to establish
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    Ok(child)
}

/// Stop port-forward processes
pub async fn stop_port_forwards(mut handles: Vec<Child>) {
    for child in &mut handles {
        if let Err(e) = child.kill().await {
            warn!(error = %e, "failed to kill port-forward process");
        }
    }
}

/// Upload a file to a pod via kubectl cp
pub async fn upload_file(
    config: &K8sConfig,
    pod: &str,
    local_path: &str,
    remote_path: &str,
) -> Result<(), K8sError> {
    info!(pod = %pod, local = %local_path, remote = %remote_path, "uploading file to pod");

    let mut cmd = tokio::process::Command::new("kubectl");
    if let Some(ref ctx) = config.context {
        cmd.arg("--context").arg(ctx);
    }
    cmd.arg("-n").arg(&config.namespace);
    cmd.arg("cp").arg(local_path);

    // format: namespace/pod:path or pod:path -c container
    let dest = format!("{}:{}", pod, remote_path);
    cmd.arg(&dest);

    if let Some(ref container) = config.container {
        cmd.arg("-c").arg(container);
    }

    let output = cmd.output().await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(K8sError::MissingField(format!(
            "kubectl cp upload failed: {}",
            stderr.trim()
        )));
    }

    Ok(())
}

/// Download a file from a pod via kubectl cp
pub async fn download_file(
    config: &K8sConfig,
    pod: &str,
    remote_path: &str,
    local_path: &str,
) -> Result<(), K8sError> {
    info!(pod = %pod, remote = %remote_path, local = %local_path, "downloading file from pod");

    let mut cmd = tokio::process::Command::new("kubectl");
    if let Some(ref ctx) = config.context {
        cmd.arg("--context").arg(ctx);
    }
    cmd.arg("-n").arg(&config.namespace);
    cmd.arg("cp");

    // format: namespace/pod:path or pod:path -c container
    let src = format!("{}:{}", pod, remote_path);
    cmd.arg(&src).arg(local_path);

    if let Some(ref container) = config.container {
        cmd.arg("-c").arg(container);
    }

    let output = cmd.output().await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(K8sError::MissingField(format!(
            "kubectl cp download failed: {}",
            stderr.trim()
        )));
    }

    Ok(())
}

/// Execute command via kubectl exec (kube-rs exec is complex, use kubectl for now)
pub async fn exec_in_pod(
    config: &K8sConfig,
    task_name: &str,
    command: &str,
    _stdin_data: Option<&str>,
) -> Result<K8sOutput, K8sError> {
    let pod = find_pod(config).await?;

    // upload files before execution
    for transfer in &config.upload {
        upload_file(config, &pod, &transfer.local, &transfer.remote).await?;
    }

    info!(task = %task_name, pod = %pod, namespace = %config.namespace, "executing in pod");

    let mut cmd = tokio::process::Command::new("kubectl");
    if let Some(ref ctx) = config.context {
        cmd.arg("--context").arg(ctx);
    }
    cmd.arg("-n").arg(&config.namespace);
    cmd.arg("exec");

    if let Some(ref container) = config.container {
        cmd.arg("-c").arg(container);
    }

    cmd.arg(&pod).arg("--").arg("sh").arg("-c");

    let full_cmd = match &config.workdir {
        Some(dir) => format!("cd {} && {}", dir, command),
        None => command.to_string(),
    };
    cmd.arg(&full_cmd);

    let output = cmd.output().await?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    for line in stdout.lines() {
        println!("[{}] {}", task_name, line);
    }

    let success = output.status.success();

    // download files after execution (only on success)
    if success {
        for transfer in &config.download {
            download_file(config, &pod, &transfer.remote, &transfer.local).await?;
        }
    }

    Ok(K8sOutput { stdout, success })
}

/// Main execution entry point
pub async fn execute(
    config: &K8sConfig,
    task_name: &str,
    command: &str,
    stdin_data: Option<&str>,
    task_timeout: Option<Duration>,
    tracker: &ResourceTracker,
) -> Result<K8sOutput, K8sError> {
    // start port forwards before task execution
    let mut port_forward_handles = Vec::new();
    for forward in &config.forwards {
        let handle = start_port_forward(config, forward).await?;
        port_forward_handles.push(handle);
    }

    // execute the task
    let result = match config.mode {
        K8sMode::Job => run_job(config, task_name, command, task_timeout, tracker).await,
        K8sMode::Apply => {
            apply_manifests(config, tracker).await?;
            wait_for_resources(config).await?;
            Ok(K8sOutput {
                stdout: String::new(),
                success: true,
            })
        }
        K8sMode::Exec => exec_in_pod(config, task_name, command, stdin_data).await,
    };

    // stop port forwards after task completes
    if !port_forward_handles.is_empty() {
        stop_port_forwards(port_forward_handles).await;
    }

    result
}
