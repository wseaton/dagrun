//! Piped data flow integration tests

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::TempDir;

fn dagrun_cmd() -> Command {
    Command::cargo_bin("dagrun").unwrap()
}

fn create_dagrun_file(dir: &TempDir, content: &str) -> std::path::PathBuf {
    let path = dir.path().join("dagrun");
    fs::write(&path, content).unwrap();
    path
}

#[test]
fn test_single_pipe_from() {
    let dir = TempDir::new().unwrap();
    let config = create_dagrun_file(
        &dir,
        r#"
gen:
    echo "hello"

# @pipe_from gen
transform: gen
    tr 'a-z' 'A-Z'
"#,
    );

    dagrun_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("transform")
        .assert()
        .success()
        .stdout(predicate::str::contains("HELLO"));
}

#[test]
fn test_multiple_pipe_from() {
    let dir = TempDir::new().unwrap();
    let config = create_dagrun_file(
        &dir,
        r#"
gen_a:
    echo "aaa"

gen_b:
    echo "bbb"

# @pipe_from gen_a, gen_b
combine: gen_a gen_b
    cat
"#,
    );

    dagrun_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("combine")
        .assert()
        .success()
        .stdout(predicate::str::contains("aaa"))
        .stdout(predicate::str::contains("bbb"));
}

#[test]
fn test_pipe_chain() {
    let dir = TempDir::new().unwrap();
    let config = create_dagrun_file(
        &dir,
        r#"
source:
    echo "test data"

# @pipe_from source
upper: source
    tr 'a-z' 'A-Z'

# @pipe_from upper
count: upper
    wc -c
"#,
    );

    dagrun_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("count")
        .assert()
        .success();
}

#[test]
fn test_join_node() {
    let dir = TempDir::new().unwrap();
    let config = create_dagrun_file(
        &dir,
        r#"
worker_1:
    echo "result 1"

worker_2:
    echo "result 2"

worker_3:
    echo "result 3"

# @join
# @pipe_from worker_1, worker_2, worker_3
collect: worker_1 worker_2 worker_3

# @pipe_from collect
final: collect
    wc -l
"#,
    );

    dagrun_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("final")
        .assert()
        .success();
}

#[test]
fn test_pipe_with_multiline_output() {
    let dir = TempDir::new().unwrap();
    let config = create_dagrun_file(
        &dir,
        r#"
gen:
    echo "line 1"
    echo "line 2"
    echo "line 3"

# @pipe_from gen
count: gen
    wc -l
"#,
    );

    dagrun_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("count")
        .assert()
        .success()
        .stdout(predicate::str::contains("3"));
}

#[test]
fn test_pipe_preserves_data() {
    let dir = TempDir::new().unwrap();
    let config = create_dagrun_file(
        &dir,
        r#"
gen:
    printf "exact data"

# @pipe_from gen
passthrough: gen
    cat

# @pipe_from passthrough
verify: passthrough
    cat
"#,
    );

    dagrun_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("verify")
        .assert()
        .success()
        .stdout(predicate::str::contains("exact data"));
}
