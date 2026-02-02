//! Lua scripting integration tests

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::TempDir;

#[allow(deprecated)]
fn dr_cmd() -> Command {
    Command::cargo_bin("dr").unwrap()
}

fn create_lua_file(dir: &TempDir, content: &str) -> std::path::PathBuf {
    let path = dir.path().join("dagfile.lua");
    fs::write(&path, content).unwrap();
    path
}

fn create_dagfile(dir: &TempDir, content: &str) -> std::path::PathBuf {
    let path = dir.path().join("dagfile");
    fs::write(&path, content).unwrap();
    path
}

#[test]
fn test_lua_basic_task() {
    let dir = TempDir::new().unwrap();
    let config = create_lua_file(
        &dir,
        r#"
task("hello", {
    run = "echo 'hello from lua'"
})
"#,
    );

    dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("hello")
        .assert()
        .success()
        .stdout(predicate::str::contains("hello from lua"));
}

#[test]
fn test_lua_task_with_dependencies() {
    let dir = TempDir::new().unwrap();
    let config = create_lua_file(
        &dir,
        r#"
task("build", {
    run = "echo 'building'"
})

task("test", {
    run = "echo 'testing'",
    depends_on = {"build"}
})
"#,
    );

    dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("test")
        .assert()
        .success()
        .stdout(predicate::str::contains("building"))
        .stdout(predicate::str::contains("testing"));
}

#[test]
fn test_lua_dynamic_task_generation() {
    let dir = TempDir::new().unwrap();
    let config = create_lua_file(
        &dir,
        r#"
for i = 1, 3 do
    task("worker-" .. i, {
        run = "echo 'worker " .. i .. "'"
    })
end

task("all", {
    run = "echo 'done'",
    depends_on = {"worker-1", "worker-2", "worker-3"}
})
"#,
    );

    dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("all")
        .assert()
        .success()
        .stdout(predicate::str::contains("worker 1"))
        .stdout(predicate::str::contains("worker 2"))
        .stdout(predicate::str::contains("worker 3"))
        .stdout(predicate::str::contains("done"));
}

#[test]
fn test_lua_timeout_and_retry() {
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("lua_retry_marker");

    let config = create_lua_file(
        &dir,
        &format!(
            r#"
task("flaky", {{
    run = 'if [ -f "{}" ]; then echo "success"; else touch "{}"; exit 1; fi',
    retry = 2
}})
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
fn test_lua_join_node() {
    let dir = TempDir::new().unwrap();
    let config = create_lua_file(
        &dir,
        r#"
task("a", { run = "echo 'a'" })
task("b", { run = "echo 'b'" })

task("collect", {
    join = true,
    pipe_from = {"a", "b"},
    depends_on = {"a", "b"}
})

task("final", {
    run = "echo 'final'",
    depends_on = {"collect"}
})
"#,
    );

    dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("final")
        .assert()
        .success()
        .stdout(predicate::str::contains("final"));
}

#[test]
fn test_lua_env_helper() {
    let dir = TempDir::new().unwrap();
    let config = create_lua_file(
        &dir,
        r#"
local home = env("HOME")
task("show-home", {
    run = "echo 'home exists'"
})
"#,
    );

    dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("show-home")
        .assert()
        .success();
}

#[test]
fn test_lua_shell_helper() {
    let dir = TempDir::new().unwrap();
    let config = create_lua_file(
        &dir,
        r#"
local result = shell("expr 1 + 1")
task("show-result", {
    run = "echo 'computed: " .. result .. "'"
})
"#,
    );

    dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("show-result")
        .assert()
        .success()
        .stdout(predicate::str::contains("computed: 2"));
}

#[test]
fn test_embedded_lua_in_dagfile() {
    let dir = TempDir::new().unwrap();
    let config = create_dagfile(
        &dir,
        r#"
build:
    echo "building"

@lua
for i = 1, 3 do
    task("worker-" .. i, {
        run = "echo 'processing chunk " .. i .. "'",
        depends_on = {"build"}
    })
end
@end

final: worker-1 worker-2 worker-3
    echo "all workers done"
"#,
    );

    dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("run")
        .arg("final")
        .assert()
        .success()
        .stdout(predicate::str::contains("building"))
        .stdout(predicate::str::contains("processing chunk 1"))
        .stdout(predicate::str::contains("processing chunk 2"))
        .stdout(predicate::str::contains("processing chunk 3"))
        .stdout(predicate::str::contains("all workers done"));
}

#[test]
fn test_lua_list_tasks() {
    let dir = TempDir::new().unwrap();
    let config = create_lua_file(
        &dir,
        r#"
for i = 1, 5 do
    task("task-" .. i, {
        run = "echo " .. i
    })
end
"#,
    );

    dr_cmd()
        .arg("-c")
        .arg(&config)
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("task-1"))
        .stdout(predicate::str::contains("task-5"));
}

#[test]
fn test_lua_validate() {
    let dir = TempDir::new().unwrap();
    let config = create_lua_file(
        &dir,
        r#"
task("valid", {
    run = "echo 'valid'"
})
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
