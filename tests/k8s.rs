//! Kubernetes integration tests
//!
//! These tests require a running Kubernetes cluster. By default they use `kind`.
//! To run: `kind create cluster --name dagrun-test`
//!
//! Set DAGRUN_K8S_TESTS=1 to enable these tests (they're ignored by default).

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::process::Command as StdCommand;
use tempfile::TempDir;

#[allow(deprecated)]
fn dr_cmd() -> Command {
    Command::cargo_bin("dr").unwrap()
}

fn create_dagfile(dir: &TempDir, content: &str) -> std::path::PathBuf {
    let path = dir.path().join("dagfile");
    fs::write(&path, content).unwrap();
    path
}

fn k8s_tests_enabled() -> bool {
    std::env::var("DAGRUN_K8S_TESTS")
        .map(|v| v == "1")
        .unwrap_or(false)
}

fn kind_cluster_exists() -> bool {
    StdCommand::new("kind")
        .args(["get", "clusters"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("dagrun-test"))
        .unwrap_or(false)
}

fn ensure_namespace(ns: &str) {
    let _ = StdCommand::new("kubectl")
        .args(["create", "namespace", ns])
        .output();
}

fn cleanup_namespace(ns: &str) {
    let _ = StdCommand::new("kubectl")
        .args(["delete", "namespace", ns, "--ignore-not-found"])
        .output();
}

/// create a configmap for testing
fn create_configmap(ns: &str, name: &str, data: &str) {
    let _ = StdCommand::new("kubectl")
        .args([
            "-n",
            ns,
            "create",
            "configmap",
            name,
            &format!("--from-literal=config.yaml={}", data),
        ])
        .output();
}

/// deploy a simple pod for exec tests
fn deploy_test_pod(ns: &str) -> bool {
    let manifest = r#"
apiVersion: v1
kind: Pod
metadata:
  name: test-pod
  labels:
    app: test
spec:
  containers:
  - name: main
    image: alpine:3
    command: ["sleep", "infinity"]
"#;

    let result = StdCommand::new("kubectl")
        .args(["-n", ns, "apply", "-f", "-"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child
                .stdin
                .as_mut()
                .unwrap()
                .write_all(manifest.as_bytes())?;
            child.wait()
        });

    if result.is_ok() {
        // wait for pod to be ready
        let _ = StdCommand::new("kubectl")
            .args([
                "-n",
                ns,
                "wait",
                "--for=condition=Ready",
                "pod/test-pod",
                "--timeout=60s",
            ])
            .output();
        true
    } else {
        false
    }
}

// ============================================================================
// JOB MODE TESTS
// ============================================================================

#[test]
#[ignore] // run with: cargo test --test integration k8s -- --ignored
fn test_k8s_job_simple() {
    if !k8s_tests_enabled() && !kind_cluster_exists() {
        eprintln!("Skipping K8s test: set DAGRUN_K8S_TESTS=1 or create kind cluster 'dagrun-test'");
        return;
    }

    let ns = "dagrun-job-simple";
    ensure_namespace(ns);

    let dir = TempDir::new().unwrap();
    let config = create_dagfile(
        &dir,
        &format!(
            r#"
@k8s job image=alpine:3 namespace={ns}
@timeout 2m
hello:
    echo "hello from k8s job"
"#,
            ns = ns
        ),
    );

    let result = dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("hello")
        .timeout(std::time::Duration::from_secs(120))
        .assert();

    cleanup_namespace(ns);

    result
        .success()
        .stdout(predicate::str::contains("hello from k8s job"));
}

#[test]
#[ignore]
fn test_k8s_job_with_resources() {
    if !k8s_tests_enabled() && !kind_cluster_exists() {
        return;
    }

    let ns = "dagrun-job-resources";
    ensure_namespace(ns);

    let dir = TempDir::new().unwrap();
    let config = create_dagfile(
        &dir,
        &format!(
            r#"
@k8s job image=alpine:3 namespace={ns}
@k8s cpu=100m memory=64Mi
@timeout 2m
with_resources:
    echo "job with resources" && cat /proc/meminfo | head -1
"#,
            ns = ns
        ),
    );

    let result = dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("with_resources")
        .timeout(std::time::Duration::from_secs(120))
        .assert();

    cleanup_namespace(ns);

    result
        .success()
        .stdout(predicate::str::contains("job with resources"));
}

#[test]
#[ignore]
fn test_k8s_job_with_configmap() {
    if !k8s_tests_enabled() && !kind_cluster_exists() {
        return;
    }

    let ns = "dagrun-job-configmap";
    ensure_namespace(ns);
    create_configmap(ns, "test-config", "key: value");

    let dir = TempDir::new().unwrap();
    let config = create_dagfile(
        &dir,
        &format!(
            r#"
@k8s job image=alpine:3 namespace={ns}
@k8s-configmap test-config:/etc/config
@timeout 2m
read_config:
    cat /etc/config/config.yaml
"#,
            ns = ns
        ),
    );

    let result = dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("read_config")
        .timeout(std::time::Duration::from_secs(120))
        .assert();

    cleanup_namespace(ns);

    result
        .success()
        .stdout(predicate::str::contains("key: value"));
}

#[test]
#[ignore]
fn test_k8s_job_failure() {
    if !k8s_tests_enabled() && !kind_cluster_exists() {
        return;
    }

    let ns = "dagrun-job-failure";
    ensure_namespace(ns);

    let dir = TempDir::new().unwrap();
    let config = create_dagfile(
        &dir,
        &format!(
            r#"
@k8s job image=alpine:3 namespace={ns}
@timeout 1m
fail_job:
    exit 1
"#,
            ns = ns
        ),
    );

    let result = dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("fail_job")
        .timeout(std::time::Duration::from_secs(120))
        .assert();

    cleanup_namespace(ns);

    result.failure();
}

// ============================================================================
// EXEC MODE TESTS
// ============================================================================

#[test]
#[ignore]
fn test_k8s_exec_by_selector() {
    if !k8s_tests_enabled() && !kind_cluster_exists() {
        return;
    }

    let ns = "dagrun-exec-selector";
    ensure_namespace(ns);

    if !deploy_test_pod(ns) {
        cleanup_namespace(ns);
        panic!("Failed to deploy test pod");
    }

    let dir = TempDir::new().unwrap();
    let config = create_dagfile(
        &dir,
        &format!(
            r#"
@k8s exec namespace={ns} selector=app=test
@timeout 1m
exec_in_pod:
    echo "hello from exec"
"#,
            ns = ns
        ),
    );

    let result = dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("exec_in_pod")
        .timeout(std::time::Duration::from_secs(60))
        .assert();

    cleanup_namespace(ns);

    result
        .success()
        .stdout(predicate::str::contains("hello from exec"));
}

#[test]
#[ignore]
fn test_k8s_exec_by_pod_name() {
    if !k8s_tests_enabled() && !kind_cluster_exists() {
        return;
    }

    let ns = "dagrun-exec-name";
    ensure_namespace(ns);

    if !deploy_test_pod(ns) {
        cleanup_namespace(ns);
        panic!("Failed to deploy test pod");
    }

    let dir = TempDir::new().unwrap();
    let config = create_dagfile(
        &dir,
        &format!(
            r#"
@k8s exec namespace={ns} pod=test-pod
@timeout 1m
exec_named:
    hostname
"#,
            ns = ns
        ),
    );

    let result = dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("exec_named")
        .timeout(std::time::Duration::from_secs(60))
        .assert();

    cleanup_namespace(ns);

    result
        .success()
        .stdout(predicate::str::contains("test-pod"));
}

#[test]
#[ignore]
fn test_k8s_exec_with_upload() {
    if !k8s_tests_enabled() && !kind_cluster_exists() {
        return;
    }

    let ns = "dagrun-exec-upload";
    ensure_namespace(ns);

    if !deploy_test_pod(ns) {
        cleanup_namespace(ns);
        panic!("Failed to deploy test pod");
    }

    let dir = TempDir::new().unwrap();
    let script = dir.path().join("test-script.sh");
    fs::write(&script, "#!/bin/sh\necho 'script executed'").unwrap();

    let config = create_dagfile(
        &dir,
        &format!(
            r#"
@k8s exec namespace={ns} selector=app=test
@k8s-upload {script}:/tmp/script.sh
@timeout 1m
upload_and_run:
    chmod +x /tmp/script.sh && /tmp/script.sh
"#,
            ns = ns,
            script = script.display()
        ),
    );

    let result = dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("upload_and_run")
        .timeout(std::time::Duration::from_secs(60))
        .assert();

    cleanup_namespace(ns);

    result
        .success()
        .stdout(predicate::str::contains("script executed"));
}

#[test]
#[ignore]
fn test_k8s_exec_with_download() {
    if !k8s_tests_enabled() && !kind_cluster_exists() {
        return;
    }

    let ns = "dagrun-exec-download";
    ensure_namespace(ns);

    if !deploy_test_pod(ns) {
        cleanup_namespace(ns);
        panic!("Failed to deploy test pod");
    }

    let dir = TempDir::new().unwrap();
    let output_file = dir.path().join("output.txt");

    let config = create_dagfile(
        &dir,
        &format!(
            r#"
@k8s exec namespace={ns} selector=app=test
@k8s-download /tmp/result.txt:{output}
@timeout 1m
generate_and_download:
    echo "downloaded content" > /tmp/result.txt
"#,
            ns = ns,
            output = output_file.display()
        ),
    );

    let result = dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("generate_and_download")
        .timeout(std::time::Duration::from_secs(60))
        .assert();

    cleanup_namespace(ns);

    result.success();

    // verify the file was downloaded
    assert!(output_file.exists(), "Downloaded file should exist");
    let content = fs::read_to_string(&output_file).unwrap();
    assert!(content.contains("downloaded content"));
}

// ============================================================================
// APPLY MODE TESTS
// ============================================================================

#[test]
#[ignore]
fn test_k8s_apply_manifest() {
    if !k8s_tests_enabled() && !kind_cluster_exists() {
        return;
    }

    let ns = "dagrun-apply-test";
    ensure_namespace(ns);

    let dir = TempDir::new().unwrap();
    let manifest_dir = dir.path().join("manifests");
    fs::create_dir(&manifest_dir).unwrap();

    // create a simple configmap manifest
    let manifest = r#"
apiVersion: v1
kind: ConfigMap
metadata:
  name: applied-config
data:
  key: applied-value
"#;
    fs::write(manifest_dir.join("configmap.yaml"), manifest).unwrap();

    let config = create_dagfile(
        &dir,
        &format!(
            r#"
@k8s apply path={manifest} namespace={ns}
@timeout 1m
apply_config:
"#,
            manifest = manifest_dir.display(),
            ns = ns
        ),
    );

    let result = dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("apply_config")
        .timeout(std::time::Duration::from_secs(60))
        .assert();

    // verify configmap exists
    let cm_check = StdCommand::new("kubectl")
        .args([
            "-n",
            ns,
            "get",
            "configmap",
            "applied-config",
            "-o",
            "jsonpath={.data.key}",
        ])
        .output();

    cleanup_namespace(ns);

    result.success();

    if let Ok(output) = cm_check {
        let value = String::from_utf8_lossy(&output.stdout);
        assert_eq!(value, "applied-value");
    }
}

#[test]
#[ignore]
fn test_k8s_apply_with_wait() {
    if !k8s_tests_enabled() && !kind_cluster_exists() {
        return;
    }

    let ns = "dagrun-apply-wait";
    ensure_namespace(ns);

    let dir = TempDir::new().unwrap();
    let manifest_dir = dir.path().join("manifests");
    fs::create_dir(&manifest_dir).unwrap();

    // create a deployment manifest
    let manifest = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: test-deploy
spec:
  replicas: 1
  selector:
    matchLabels:
      app: test-deploy
  template:
    metadata:
      labels:
        app: test-deploy
    spec:
      containers:
      - name: app
        image: alpine:3
        command: ["sleep", "infinity"]
"#;
    fs::write(manifest_dir.join("deployment.yaml"), manifest).unwrap();

    let config = create_dagfile(
        &dir,
        &format!(
            r#"
@k8s apply path={manifest} namespace={ns}
@k8s wait=deployment/test-deploy timeout=2m
@timeout 3m
deploy_with_wait:
"#,
            manifest = manifest_dir.display(),
            ns = ns
        ),
    );

    let result = dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("deploy_with_wait")
        .timeout(std::time::Duration::from_secs(180))
        .assert();

    cleanup_namespace(ns);

    result.success();
}

// ============================================================================
// LUA K8S TESTS
// ============================================================================

#[test]
#[ignore]
fn test_k8s_lua_dynamic_jobs() {
    if !k8s_tests_enabled() && !kind_cluster_exists() {
        return;
    }

    let ns = "dagrun-lua-jobs";
    ensure_namespace(ns);

    let dir = TempDir::new().unwrap();
    let config_path = dir.path().join("dagfile.lua");

    let lua_config = format!(
        r#"
for i = 1, 2 do
    task("worker-" .. i, {{
        run = "echo 'worker " .. i .. " done'",
        k8s = {{
            mode = "job",
            image = "alpine:3",
            namespace = "{ns}",
            cpu = "100m",
            memory = "32Mi"
        }}
    }})
end

task("all-done", {{
    run = "echo 'all workers complete'",
    depends_on = {{"worker-1", "worker-2"}}
}})
"#,
        ns = ns
    );
    fs::write(&config_path, lua_config).unwrap();

    let result = dr_cmd()
        .arg("-c")
        .arg(&config_path)
        .arg("run")
        .arg("all-done")
        .timeout(std::time::Duration::from_secs(180))
        .assert();

    cleanup_namespace(ns);

    result
        .success()
        .stdout(predicate::str::contains("worker 1 done"))
        .stdout(predicate::str::contains("worker 2 done"))
        .stdout(predicate::str::contains("all workers complete"));
}

// ============================================================================
// HYBRID WORKFLOW TESTS
// ============================================================================

#[test]
#[ignore]
fn test_k8s_mixed_local_and_k8s() {
    if !k8s_tests_enabled() && !kind_cluster_exists() {
        return;
    }

    let ns = "dagrun-hybrid";
    ensure_namespace(ns);

    let dir = TempDir::new().unwrap();
    let config = create_dagfile(
        &dir,
        &format!(
            r#"
local_build:
    echo "building locally"

@k8s job image=alpine:3 namespace={ns}
@timeout 2m
k8s_process: local_build
    echo "processing in k8s"

local_finish: k8s_process
    echo "finished locally"
"#,
            ns = ns
        ),
    );

    let result = dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("local_finish")
        .timeout(std::time::Duration::from_secs(120))
        .assert();

    cleanup_namespace(ns);

    result
        .success()
        .stdout(predicate::str::contains("building locally"))
        .stdout(predicate::str::contains("processing in k8s"))
        .stdout(predicate::str::contains("finished locally"));
}
