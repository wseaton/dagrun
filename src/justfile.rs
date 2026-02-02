//! Justfile-like syntax parser with Lua block support

use std::path::Path;
use thiserror::Error;

use crate::lua::parse_lua_config;
use dr_ast::Config;

#[derive(Error, Debug)]
pub enum ParseError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse error at line {0}: {1}")]
    Syntax(usize, String),
    #[error("lua error: {0}")]
    Lua(String),
}

pub fn load_justflow<P: AsRef<Path>>(path: P) -> Result<Config, ParseError> {
    let content = std::fs::read_to_string(path)?;
    parse_justflow(&content)
}

pub fn parse_justflow(content: &str) -> Result<Config, ParseError> {
    // parse using the new semantic parser
    let mut config = dr_ast::parse_config(content).map_err(|e| {
        let line = content[..e.span.start as usize].lines().count();
        ParseError::Syntax(line, e.message)
    })?;

    // process lua blocks separately (dr_ast doesn't have mlua dependency)
    for lua_block in dr_ast::extract_lua_blocks(content) {
        let lua_config =
            parse_lua_config(&lua_block).map_err(|e| ParseError::Lua(e.to_string()))?;
        config.tasks.extend(lua_config.tasks);
    }

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_task() {
        let config = parse_justflow("build:\n\tcargo build").unwrap();
        assert!(config.tasks.contains_key("build"));
    }

    #[test]
    fn test_task_with_deps() {
        let config = parse_justflow("test: build\n\tcargo test").unwrap();
        let task = config.tasks.get("test").unwrap();
        assert_eq!(task.depends_on, vec!["build"]);
    }

    #[test]
    fn test_annotations() {
        let config = parse_justflow("@timeout 5m\n@retry 2\nbuild:\n\tcargo build").unwrap();
        let task = config.tasks.get("build").unwrap();
        assert!(task.timeout.is_some());
        assert_eq!(task.retry, 2);
    }

    #[test]
    fn test_variable() {
        let config = parse_justflow("version := 1.0.0\nbuild:\n\techo {{version}}").unwrap();
        let task = config.tasks.get("build").unwrap();
        assert!(task.run.as_ref().unwrap().contains("1.0.0"));
    }

    #[test]
    fn test_ssh_annotation() {
        let config =
            parse_justflow("@ssh host=host.example.com user=deploy\nremote:\n\techo hi").unwrap();
        let task = config.tasks.get("remote").unwrap();
        assert!(task.ssh.is_some());
        let ssh = task.ssh.as_ref().unwrap();
        assert_eq!(ssh.host, "host.example.com");
        assert_eq!(ssh.user, Some("deploy".to_string()));
    }

    #[test]
    fn test_service_dependency() {
        let config = parse_justflow("test: service:db\n\tcargo test").unwrap();
        let task = config.tasks.get("test").unwrap();
        assert_eq!(task.service_deps, vec!["db"]);
    }

    #[test]
    fn test_join_node() {
        let config = parse_justflow("@join\ncollect: a b").unwrap();
        let task = config.tasks.get("collect").unwrap();
        assert!(task.join);
    }

    #[test]
    fn test_pipe_from() {
        let config = parse_justflow("@pipe_from a, b\ntransform: a b\n\tcat").unwrap();
        let task = config.tasks.get("transform").unwrap();
        assert_eq!(task.pipe_from, vec!["a", "b"]);
    }

    #[test]
    fn test_shebang_bash() {
        let config = parse_justflow("build:\n\t#!/bin/bash\n\techo hello").unwrap();
        let task = config.tasks.get("build").unwrap();
        assert!(task.shebang.is_some());
        assert_eq!(task.shebang.as_ref().unwrap().interpreter, "/bin/bash");
    }

    #[test]
    fn test_no_shebang() {
        let config = parse_justflow("build:\n\techo hello").unwrap();
        let task = config.tasks.get("build").unwrap();
        assert!(task.shebang.is_none());
    }

    #[test]
    fn test_file_transfers() {
        let config = parse_justflow(
            "@ssh host\n@upload ./local.txt:/remote.txt\n@download /remote.txt:./local.txt\nremote:\n\techo hi",
        )
        .unwrap();
        let task = config.tasks.get("remote").unwrap();
        let ssh = task.ssh.as_ref().unwrap();
        assert_eq!(ssh.upload.len(), 1);
        assert_eq!(ssh.download.len(), 1);
    }

    #[test]
    fn test_service_managed() {
        let config = parse_justflow(
            "@service ready=http://localhost:8080/health startup_timeout=30s\nweb:\n\t./server",
        )
        .unwrap();
        let task = config.tasks.get("web").unwrap();
        assert!(task.service.is_some());
    }

    #[test]
    fn test_service_external() {
        let config = parse_justflow("@extern ready=tcp:localhost:5432\ndb:").unwrap();
        let task = config.tasks.get("db").unwrap();
        assert!(task.service.is_some());
        assert_eq!(
            task.service.as_ref().unwrap().kind,
            dr_ast::ServiceKind::External
        );
    }

    #[test]
    fn test_dotenv_settings() {
        let config = parse_justflow("set dotenv := true\nbuild:\n\techo hi").unwrap();
        assert!(config.dotenv.load);
    }
}
