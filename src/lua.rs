//! Lua-based dynamic graph definition
//!
//! Allows defining task graphs dynamically with Lua scripts:
//!
//! ```lua
//! -- dagrun.lua
//! task("build", {
//!     run = "cargo build",
//!     timeout = "5m",
//!     retry = 1
//! })
//!
//! task("test", {
//!     run = "cargo test",
//!     depends_on = {"build"}
//! })
//!
//! -- dynamic task generation
//! for i = 1, 3 do
//!     task("worker-" .. i, {
//!         run = "echo 'worker " .. i .. "'",
//!         pipe_from = {"build"}
//!     })
//! end
//!
//! -- join node
//! task("collect", {
//!     join = true,
//!     pipe_from = {"worker-1", "worker-2", "worker-3"}
//! })
//! ```

use mlua::{Lua, Result as LuaResult, Table};
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;
use thiserror::Error;

use dagrun_ast::{Config, DotenvSettings, K8sConfig, K8sMode, Shebang, SshConfig, Task};

#[derive(Error, Debug)]
pub enum LuaConfigError {
    #[error("lua error: {0}")]
    Lua(#[from] mlua::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub fn load_lua_config<P: AsRef<Path>>(path: P) -> Result<Config, LuaConfigError> {
    let content = std::fs::read_to_string(path)?;
    parse_lua_config(&content)
}

pub fn parse_lua_config(content: &str) -> Result<Config, LuaConfigError> {
    let lua = Lua::new();

    // storage for tasks defined in lua
    let tasks_ref = std::sync::Arc::new(std::sync::Mutex::new(HashMap::<String, Task>::new()));

    // create the task() function and execute in a scope
    {
        let tasks_clone = tasks_ref.clone();
        let task_fn = lua.create_function(move |_, (name, opts): (String, Table)| {
            let task = parse_task_table(&name, &opts)?;
            tasks_clone.lock().unwrap().insert(name, task);
            Ok(())
        })?;

        lua.globals().set("task", task_fn)?;

        // add helper functions
        add_helpers(&lua)?;

        // execute the lua script
        lua.load(content).exec()?;
    }

    // drop lua to release closure references
    drop(lua);

    // now we can unwrap - clone the data out instead of try_unwrap
    let tasks = tasks_ref.lock().unwrap().clone();

    Ok(Config {
        tasks,
        dotenv: DotenvSettings::default(),
    })
}

fn parse_ssh_config(opts: &Table) -> LuaResult<SshConfig> {
    Ok(SshConfig {
        host: opts.get("host").unwrap_or_default(),
        user: opts.get("user").ok(),
        port: opts.get("port").ok(),
        identity: opts.get("identity").ok(),
        workdir: opts.get("workdir").ok(),
        upload: Vec::new(),
        download: Vec::new(),
    })
}

fn parse_k8s_config(opts: &Table) -> LuaResult<K8sConfig> {
    let mode_str: String = opts.get("mode").unwrap_or_else(|_| "job".to_string());
    let mode = match mode_str.as_str() {
        "exec" => K8sMode::Exec,
        "apply" => K8sMode::Apply,
        _ => K8sMode::Job,
    };

    // parse node selector from table
    let node_selector = if let Ok(sel_table) = opts.get::<Table>("node_selector") {
        let mut map = std::collections::HashMap::new();
        for (k, v) in sel_table.pairs::<String, String>().flatten() {
            map.insert(k, v);
        }
        if map.is_empty() { None } else { Some(map) }
    } else {
        None
    };

    // parse tolerations from array
    let tolerations: Vec<String> = opts.get::<Vec<String>>("tolerations").unwrap_or_default();

    // parse wait_for from array
    let wait_for: Vec<String> = opts.get::<Vec<String>>("wait_for").unwrap_or_default();

    // parse wait_timeout
    let wait_timeout: Option<Duration> = match opts.get::<String>("wait_timeout") {
        Ok(s) => s.parse::<humantime::Duration>().ok().map(|d| d.into()),
        Err(_) => None,
    };

    Ok(K8sConfig {
        mode,
        context: opts.get("context").ok(),
        namespace: opts
            .get("namespace")
            .unwrap_or_else(|_| "default".to_string()),
        selector: opts.get("selector").ok(),
        pod: opts.get("pod").ok(),
        container: opts.get("container").ok(),
        image: opts.get("image").ok(),
        cpu: opts.get("cpu").ok(),
        memory: opts.get("memory").ok(),
        node_selector,
        tolerations,
        service_account: opts.get("service_account").ok(),
        ttl_seconds: opts.get("ttl_seconds").ok(),
        path: opts.get("path").ok(),
        wait_for,
        wait_timeout,
        upload: Vec::new(),
        download: Vec::new(),
        configmaps: Vec::new(),
        secrets: Vec::new(),
        forwards: Vec::new(),
        workdir: opts.get("workdir").ok(),
    })
}

fn parse_task_table(name: &str, opts: &Table) -> LuaResult<Task> {
    let run: Option<String> = opts.get("run")?;

    let depends_on: Vec<String> = opts.get::<Vec<String>>("depends_on").unwrap_or_default();

    let pipe_from: Vec<String> = opts.get::<Vec<String>>("pipe_from").unwrap_or_default();

    let timeout: Option<Duration> = match opts.get::<String>("timeout") {
        Ok(s) => s.parse::<humantime::Duration>().ok().map(|d| d.into()),
        Err(_) => None,
    };

    let retry: u32 = opts.get("retry").unwrap_or(0);
    let join: bool = opts.get("join").unwrap_or(false);

    // parse ssh config if present
    let ssh = if let Ok(ssh_table) = opts.get::<Table>("ssh") {
        Some(parse_ssh_config(&ssh_table)?)
    } else if let Ok(host) = opts.get::<String>("ssh") {
        // shorthand: ssh = "hostname"
        Some(SshConfig {
            host,
            ..Default::default()
        })
    } else {
        None
    };

    // parse k8s config if present
    let k8s = if let Ok(k8s_table) = opts.get::<Table>("k8s") {
        Some(parse_k8s_config(&k8s_table)?)
    } else {
        None
    };

    // parse shebang from run if present
    let shebang = run
        .as_ref()
        .and_then(|r| r.lines().next().and_then(Shebang::parse));

    Ok(Task {
        name: name.to_string(),
        run,
        depends_on,
        service_deps: Vec::new(),
        pipe_from,
        timeout,
        retry,
        join,
        ssh,
        k8s,
        service: None,
        shebang,
        span: None,
    })
}

fn add_helpers(lua: &Lua) -> LuaResult<()> {
    // env(name) - get environment variable
    let env_fn =
        lua.create_function(|_, name: String| Ok(std::env::var(&name).unwrap_or_default()))?;
    lua.globals().set("env", env_fn)?;

    // shell(cmd) - execute shell command and return output
    let shell_fn = lua.create_function(|_, cmd: String| {
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .output()
            .map_err(mlua::Error::external)?;
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    })?;
    lua.globals().set("shell", shell_fn)?;

    // glob(pattern) - return list of files matching pattern
    let glob_fn = lua.create_function(|lua, pattern: String| {
        let paths: Vec<String> = glob::glob(&pattern)
            .map_err(mlua::Error::external)?
            .filter_map(|p| p.ok())
            .map(|p| p.to_string_lossy().to_string())
            .collect();

        let table = lua.create_table()?;
        for (i, path) in paths.into_iter().enumerate() {
            table.set(i + 1, path)?;
        }
        Ok(table)
    })?;
    lua.globals().set("glob", glob_fn)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_lua_config() {
        let lua = r#"
            task("build", {
                run = "cargo build",
                timeout = "5m",
                retry = 1
            })

            task("test", {
                run = "cargo test",
                depends_on = {"build"}
            })
        "#;

        let config = parse_lua_config(lua).unwrap();
        assert_eq!(config.tasks.len(), 2);
        assert_eq!(config.tasks["build"].retry, 1);
        assert_eq!(config.tasks["test"].depends_on, vec!["build"]);
    }

    #[test]
    fn test_dynamic_task_generation() {
        let lua = r#"
            for i = 1, 3 do
                task("worker-" .. i, {
                    run = "echo " .. i
                })
            end
        "#;

        let config = parse_lua_config(lua).unwrap();
        assert_eq!(config.tasks.len(), 3);
        assert!(config.tasks.contains_key("worker-1"));
        assert!(config.tasks.contains_key("worker-2"));
        assert!(config.tasks.contains_key("worker-3"));
    }

    #[test]
    fn test_join_node() {
        let lua = r#"
            task("a", { run = "echo a" })
            task("b", { run = "echo b" })
            task("join", {
                join = true,
                pipe_from = {"a", "b"}
            })
        "#;

        let config = parse_lua_config(lua).unwrap();
        assert!(config.tasks["join"].is_join());
        assert_eq!(config.tasks["join"].pipe_from, vec!["a", "b"]);
    }

    #[test]
    fn test_k8s_job_config() {
        let lua = r#"
            task("train", {
                run = "python train.py",
                k8s = {
                    mode = "job",
                    image = "python:3.11",
                    namespace = "ml-jobs",
                    cpu = "2",
                    memory = "4Gi"
                }
            })
        "#;

        let config = parse_lua_config(lua).unwrap();
        let k8s = config.tasks["train"].k8s.as_ref().unwrap();
        assert_eq!(k8s.mode, K8sMode::Job);
        assert_eq!(k8s.image, Some("python:3.11".to_string()));
        assert_eq!(k8s.namespace, "ml-jobs");
        assert_eq!(k8s.cpu, Some("2".to_string()));
        assert_eq!(k8s.memory, Some("4Gi".to_string()));
    }

    #[test]
    fn test_k8s_exec_config() {
        let lua = r#"
            task("migrate", {
                run = "python manage.py migrate",
                k8s = {
                    mode = "exec",
                    namespace = "prod",
                    selector = "app=api"
                }
            })
        "#;

        let config = parse_lua_config(lua).unwrap();
        let k8s = config.tasks["migrate"].k8s.as_ref().unwrap();
        assert_eq!(k8s.mode, K8sMode::Exec);
        assert_eq!(k8s.selector, Some("app=api".to_string()));
    }

    #[test]
    fn test_dynamic_k8s_workers() {
        let lua = r#"
            for i = 1, 3 do
                task("worker-" .. i, {
                    run = "python process.py --shard " .. i,
                    k8s = {
                        mode = "job",
                        image = "python:3.11",
                        namespace = "staging",
                        cpu = "1",
                        memory = "2Gi"
                    }
                })
            end
        "#;

        let config = parse_lua_config(lua).unwrap();
        assert_eq!(config.tasks.len(), 3);
        for i in 1..=3 {
            let name = format!("worker-{}", i);
            let k8s = config.tasks[&name].k8s.as_ref().unwrap();
            assert_eq!(k8s.mode, K8sMode::Job);
            assert_eq!(k8s.image, Some("python:3.11".to_string()));
        }
    }
}
