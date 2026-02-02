//! Service lifecycle integration tests

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::net::TcpListener;
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

fn find_free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[test]
fn test_managed_service_tcp_ready() {
    let port = find_free_port();
    let dir = TempDir::new().unwrap();
    let config = create_dagfile(
        &dir,
        &format!(
            r#"
@service ready=tcp:127.0.0.1:{port} startup_timeout=10s interval=100ms log=quiet
tcp_server:
    nc -l {port}

use_server: service:tcp_server
    echo "server is ready"
"#,
            port = port
        ),
    );

    dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("use_server")
        .timeout(std::time::Duration::from_secs(15))
        .assert()
        .success()
        .stdout(predicate::str::contains("server is ready"));
}

#[test]
fn test_managed_service_http_ready() {
    let port = find_free_port();
    let dir = TempDir::new().unwrap();
    let config = create_dagfile(
        &dir,
        &format!(
            r#"
@service ready=http://127.0.0.1:{port}/ startup_timeout=15s interval=100ms log=quiet
http_server:
    python3 -m http.server {port}

use_http: service:http_server
    echo "http server ready"
"#,
            port = port
        ),
    );

    dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("use_http")
        .timeout(std::time::Duration::from_secs(20))
        .assert()
        .success()
        .stdout(predicate::str::contains("http server ready"));
}

#[test]
fn test_managed_service_command_ready() {
    // use a fixed path to avoid quoting issues in temp dirs
    let marker = "/tmp/dagrun_test_ready_marker";
    let _ = std::fs::remove_file(marker);

    let dir = TempDir::new().unwrap();
    let config = create_dagfile(
        &dir,
        &format!(
            r#"
@service ready=cmd:"test -f {marker}" startup_timeout=10s interval=100ms log=quiet
file_creator:
    touch {marker} && sleep 30

use_file: service:file_creator
    echo "file exists"
"#,
            marker = marker
        ),
    );

    let result = dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("use_file")
        .timeout(std::time::Duration::from_secs(15))
        .assert();

    // cleanup
    let _ = std::fs::remove_file(marker);

    result
        .success()
        .stdout(predicate::str::contains("file exists"));
}

#[test]
fn test_external_service() {
    let port = find_free_port();

    // start an external listener
    let listener = std::process::Command::new("nc")
        .args(["-l", &port.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("failed to start nc");

    // give it time to bind
    std::thread::sleep(std::time::Duration::from_millis(200));

    let dir = TempDir::new().unwrap();
    let config = create_dagfile(
        &dir,
        &format!(
            r#"
@extern ready=tcp:127.0.0.1:{port} startup_timeout=5s interval=100ms
external_svc:

use_external: service:external_svc
    echo "external service detected"
"#,
            port = port
        ),
    );

    let result = dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("use_external")
        .timeout(std::time::Duration::from_secs(10))
        .assert();

    // cleanup
    drop(listener);

    result
        .success()
        .stdout(predicate::str::contains("external service detected"));
}

#[test]
fn test_service_timeout_fails() {
    let port = find_free_port();
    let dir = TempDir::new().unwrap();
    let config = create_dagfile(
        &dir,
        &format!(
            r#"
@service ready=tcp:127.0.0.1:{port} startup_timeout=1s interval=100ms log=quiet
never_ready:
    sleep 30

use_never: service:never_ready
    echo "should not reach here"
"#,
            port = port
        ),
    );

    dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("use_never")
        .timeout(std::time::Duration::from_secs(10))
        .assert()
        .failure();
}

#[test]
fn test_multiple_service_deps() {
    let port1 = find_free_port();
    let port2 = find_free_port();
    let dir = TempDir::new().unwrap();
    let config = create_dagfile(
        &dir,
        &format!(
            r#"
@service ready=tcp:127.0.0.1:{port1} startup_timeout=10s interval=100ms log=quiet
svc1:
    nc -l {port1}

@service ready=tcp:127.0.0.1:{port2} startup_timeout=10s interval=100ms log=quiet
svc2:
    nc -l {port2}

use_both: service:svc1 service:svc2
    echo "both services ready"
"#,
            port1 = port1,
            port2 = port2
        ),
    );

    dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("use_both")
        .timeout(std::time::Duration::from_secs(15))
        .assert()
        .success()
        .stdout(predicate::str::contains("both services ready"));
}

#[test]
fn test_service_with_preflight() {
    // use a fixed path to avoid quoting issues
    let marker = "/tmp/dagrun_test_preflight_marker";
    fs::write(marker, "ok").unwrap();

    let dir = TempDir::new().unwrap();
    let port = find_free_port();
    let config = create_dagfile(
        &dir,
        &format!(
            r#"
@service ready=tcp:127.0.0.1:{port} startup_timeout=10s interval=100ms log=quiet preflight="test -f {marker}"
with_preflight:
    nc -l {port}

use_preflight: service:with_preflight
    echo "preflight passed"
"#,
            port = port,
            marker = marker
        ),
    );

    let result = dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("use_preflight")
        .timeout(std::time::Duration::from_secs(15))
        .assert();

    // cleanup
    let _ = std::fs::remove_file(marker);

    result
        .success()
        .stdout(predicate::str::contains("preflight passed"));
}

#[test]
fn test_service_preflight_fails() {
    let port = find_free_port();
    let dir = TempDir::new().unwrap();
    let config = create_dagfile(
        &dir,
        &format!(
            r#"
@service ready=tcp:127.0.0.1:{port} startup_timeout=5s interval=100ms log=quiet preflight="test -f /nonexistent/file"
bad_preflight:
    nc -l {port}

use_bad_preflight: service:bad_preflight
    echo "should not run"
"#,
            port = port
        ),
    );

    dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("use_bad_preflight")
        .timeout(std::time::Duration::from_secs(10))
        .assert()
        .failure();
}
