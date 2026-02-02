//! Basic task execution integration tests

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
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

#[test]
fn test_simple_task_execution() {
    let dir = TempDir::new().unwrap();
    let config = create_dagfile(
        &dir,
        r#"
hello:
    echo "hello world"
"#,
    );

    dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("hello")
        .assert()
        .success()
        .stdout(predicate::str::contains("hello world"));
}

#[test]
fn test_task_with_dependencies() {
    let dir = TempDir::new().unwrap();
    let config = create_dagfile(
        &dir,
        r#"
first:
    echo "step 1"

second: first
    echo "step 2"

third: second
    echo "step 3"
"#,
    );

    dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("third")
        .assert()
        .success()
        .stdout(predicate::str::contains("step 1"))
        .stdout(predicate::str::contains("step 2"))
        .stdout(predicate::str::contains("step 3"));
}

#[test]
fn test_task_with_only_flag() {
    let dir = TempDir::new().unwrap();
    let config = create_dagfile(
        &dir,
        r#"
first:
    echo "step 1"

second: first
    echo "step 2"
"#,
    );

    dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("second")
        .arg("--only")
        .assert()
        .success()
        .stdout(predicate::str::contains("step 2"))
        .stdout(predicate::str::contains("step 1").not());
}

#[test]
fn test_parallel_task_execution() {
    let dir = TempDir::new().unwrap();
    let config = create_dagfile(
        &dir,
        r#"
a:
    echo "task a"

b:
    echo "task b"

c: a b
    echo "task c"
"#,
    );

    dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("c")
        .assert()
        .success()
        .stdout(predicate::str::contains("task a"))
        .stdout(predicate::str::contains("task b"))
        .stdout(predicate::str::contains("task c"));
}

#[test]
fn test_task_timeout() {
    let dir = TempDir::new().unwrap();
    let config = create_dagfile(
        &dir,
        r#"
@timeout 1s
slow:
    sleep 10
"#,
    );

    dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("slow")
        .assert()
        .failure();
}

#[test]
fn test_task_retry() {
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("retry_marker");

    let config = create_dagfile(
        &dir,
        &format!(
            r#"
@retry 2
flaky:
    if [ -f "{}" ]; then echo "success"; else touch "{}"; exit 1; fi
"#,
            marker.display(),
            marker.display()
        ),
    );

    dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("flaky")
        .assert()
        .success()
        .stdout(predicate::str::contains("success"));
}

#[test]
fn test_task_failure_stops_dag() {
    let dir = TempDir::new().unwrap();
    let config = create_dagfile(
        &dir,
        r#"
fail:
    exit 1

after: fail
    echo "should not run"
"#,
    );

    dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("after")
        .assert()
        .failure()
        .stdout(predicate::str::contains("should not run").not());
}

#[test]
fn test_variable_substitution() {
    let dir = TempDir::new().unwrap();
    let config = create_dagfile(
        &dir,
        r#"
name := world

greet:
    echo "hello {{name}}"
"#,
    );

    dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("greet")
        .assert()
        .success()
        .stdout(predicate::str::contains("hello world"));
}

#[test]
fn test_shell_variable_expansion() {
    let dir = TempDir::new().unwrap();
    let config = create_dagfile(
        &dir,
        r#"
count := `expr 2 + 2`

show:
    echo "count is {{count}}"
"#,
    );

    dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("show")
        .assert()
        .success()
        .stdout(predicate::str::contains("count is 4"));
}

#[test]
fn test_list_command() {
    let dir = TempDir::new().unwrap();
    let config = create_dagfile(
        &dir,
        r#"
build:
    cargo build

test: build
    cargo test

deploy: test
    ./deploy.sh
"#,
    );

    dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("build"))
        .stdout(predicate::str::contains("test"))
        .stdout(predicate::str::contains("deploy"));
}

#[test]
fn test_list_json_format() {
    let dir = TempDir::new().unwrap();
    let config = create_dagfile(
        &dir,
        r#"
build:
    cargo build

test: build
    cargo test
"#,
    );

    let output = dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("list")
        .arg("-f")
        .arg("json")
        .assert()
        .success();

    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(json["tasks"].is_array());
}

#[test]
fn test_validate_command() {
    let dir = TempDir::new().unwrap();
    let config = create_dagfile(
        &dir,
        r#"
build:
    cargo build
"#,
    );

    dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("validate")
        .assert()
        .success()
        .stdout(predicate::str::contains("Config is valid"));
}

#[test]
fn test_graph_ascii() {
    let dir = TempDir::new().unwrap();
    let config = create_dagfile(
        &dir,
        r#"
a:
    echo a

b: a
    echo b
"#,
    );

    dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("graph")
        .assert()
        .success()
        .stdout(predicate::str::contains("Legend"));
}

#[test]
fn test_graph_dot_format() {
    let dir = TempDir::new().unwrap();
    let config = create_dagfile(
        &dir,
        r#"
a:
    echo a

b: a
    echo b
"#,
    );

    dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("graph")
        .arg("-f")
        .arg("dot")
        .assert()
        .success()
        .stdout(predicate::str::contains("digraph"))
        .stdout(predicate::str::contains("->"));
}

#[test]
fn test_cycle_detection() {
    let dir = TempDir::new().unwrap();
    let config = create_dagfile(
        &dir,
        r#"
a: b
    echo a

b: a
    echo b
"#,
    );

    dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("a")
        .assert()
        .failure()
        .stderr(predicate::str::contains("cycle"));
}

#[test]
fn test_task_not_found() {
    let dir = TempDir::new().unwrap();
    let config = create_dagfile(
        &dir,
        r#"
build:
    cargo build
"#,
    );

    dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("nonexistent")
        .assert()
        .failure();
}

#[test]
fn test_run_all() {
    let dir = TempDir::new().unwrap();
    let config = create_dagfile(
        &dir,
        r#"
a:
    echo "running a"

b:
    echo "running b"
"#,
    );

    dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run-all")
        .assert()
        .success()
        .stdout(predicate::str::contains("running a"))
        .stdout(predicate::str::contains("running b"));
}

#[test]
fn test_multiline_command() {
    let dir = TempDir::new().unwrap();
    let config = create_dagfile(
        &dir,
        r#"
multi:
    echo "line 1"
    echo "line 2"
    echo "line 3"
"#,
    );

    dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("multi")
        .assert()
        .success()
        .stdout(predicate::str::contains("line 1"))
        .stdout(predicate::str::contains("line 2"))
        .stdout(predicate::str::contains("line 3"));
}
