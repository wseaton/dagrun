//! Semantic parser - converts syntactic AST to semantic Config

use std::collections::HashMap;
use std::process::Command;
use std::time::Duration;

use crate::Span;
use crate::ast::{self, AnnotationKind, BodyLine, CommandSegment, Dependency, Item, VariableValue};
use crate::parser;
use crate::semantic::{
    Config, ConfigMount, DotenvSettings, FileTransfer, K8sConfig, K8sMode, LogOutput, PortForward,
    ReadinessCheck, ServiceConfig, ServiceKind, Shebang, SshConfig, Task, TaskParameter,
};

/// Parse a dagrun source file into a semantic Config
pub fn parse_config(source: &str) -> Result<Config, ParseConfigError> {
    let (ast, parse_errors) = parser::parse(source);

    // collect parse errors but continue
    let mut errors: Vec<ParseConfigError> = parse_errors
        .into_iter()
        .map(|e| ParseConfigError {
            span: e.span,
            message: e.message,
        })
        .collect();

    let mut ctx = Context::new(source);

    // first pass: collect variables
    for item in &ast.items {
        if let Item::Variable(var) = &item.node {
            match ctx.eval_variable_value(&var.value.node) {
                Ok(value) => {
                    ctx.variables.insert(var.name.node.clone(), value);
                }
                Err(e) => errors.push(e),
            }
        }
    }

    // second pass: process tasks, lua blocks, set directives
    for item in &ast.items {
        match &item.node {
            Item::Task(task_decl) => match ctx.lower_task(task_decl, item.span) {
                Ok(task) => {
                    ctx.tasks.insert(task.name.clone(), task);
                }
                Err(e) => errors.push(e),
            },
            Item::LuaBlock(lua) => {
                // lua blocks need special handling - we'll skip for now and let caller handle
                // since mlua is a heavy dependency
                ctx.lua_blocks.push(lua.content.node.clone());
            }
            Item::SetDirective(set) => {
                ctx.handle_set_directive(&set.key.node, &set.value.node);
            }
            Item::Variable(_) | Item::Comment(_) => {}
        }
    }

    // return errors if any are fatal (for now, treat all as warnings)
    if !errors.is_empty() {
        for e in &errors {
            eprintln!("warning: {}", e.message);
        }
    }

    Ok(Config {
        tasks: ctx.tasks,
        dotenv: ctx.dotenv,
    })
}

/// Get lua blocks from a parsed file (for external lua processing)
pub fn extract_lua_blocks(source: &str) -> Vec<String> {
    let (ast, _) = parser::parse(source);
    ast.items
        .iter()
        .filter_map(|item| {
            if let Item::LuaBlock(lua) = &item.node {
                Some(lua.content.node.clone())
            } else {
                None
            }
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct ParseConfigError {
    pub span: Span,
    pub message: String,
}

impl std::fmt::Display for ParseConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "at {}: {}", self.span.start, self.message)
    }
}

impl std::error::Error for ParseConfigError {}

struct Context<'a> {
    #[allow(dead_code)]
    source: &'a str,
    variables: HashMap<String, String>,
    tasks: HashMap<String, Task>,
    dotenv: DotenvSettings,
    lua_blocks: Vec<String>,
}

impl<'a> Context<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            source,
            variables: HashMap::new(),
            tasks: HashMap::new(),
            dotenv: DotenvSettings::default(),
            lua_blocks: Vec::new(),
        }
    }

    fn eval_variable_value(&self, value: &VariableValue) -> Result<String, ParseConfigError> {
        match value {
            VariableValue::Static(s) => Ok(self.substitute_variables(s)),
            VariableValue::Shell(shell) => {
                let cmd = self.substitute_variables(&shell.command.node);
                evaluate_shell_command(&cmd).map_err(|e| ParseConfigError {
                    span: shell.command.span,
                    message: e,
                })
            }
        }
    }

    fn substitute_variables(&self, text: &str) -> String {
        let mut result = text.to_string();
        for (name, value) in &self.variables {
            let pattern = format!("{{{{{}}}}}", name);
            result = result.replace(&pattern, value);
        }
        result
    }

    fn lower_task(
        &self,
        task_decl: &ast::TaskDecl,
        task_span: Span,
    ) -> Result<Task, ParseConfigError> {
        let name = task_decl.name.node.clone();

        // lower parameters
        let parameters = self.lower_task_parameters(&task_decl.parameters);

        let mut timeout: Option<Duration> = None;
        let mut retry: u32 = 0;
        let mut pipe_from: Vec<String> = Vec::new();
        let mut join = false;
        let mut ssh: Option<SshConfig> = None;
        let mut service: Option<ServiceConfig> = None;
        let mut k8s: Option<K8sConfig> = None;

        for ann in &task_decl.annotations {
            match &ann.node.kind {
                AnnotationKind::Timeout(val) => {
                    let dur_str = self.substitute_variables(&val.node);
                    timeout = Some(parse_duration(&dur_str).map_err(|e| ParseConfigError {
                        span: val.span,
                        message: e,
                    })?);
                }
                AnnotationKind::Retry(val) => {
                    let val_str = self.substitute_variables(&val.node);
                    retry = val_str.parse().map_err(|_| ParseConfigError {
                        span: val.span,
                        message: "invalid retry count".to_string(),
                    })?;
                }
                AnnotationKind::PipeFrom(tasks) => {
                    pipe_from = tasks.iter().map(|t| t.node.clone()).collect();
                }
                AnnotationKind::Join => {
                    join = true;
                }
                AnnotationKind::Ssh(ssh_ann) => {
                    ssh = Some(self.lower_ssh_annotation(ssh_ann));
                }
                AnnotationKind::Upload(ft) => {
                    if let Some(ref mut s) = ssh {
                        s.upload.push(self.lower_file_transfer(ft));
                    }
                }
                AnnotationKind::Download(ft) => {
                    if let Some(ref mut s) = ssh {
                        s.download.push(self.lower_file_transfer(ft));
                    }
                }
                AnnotationKind::Service(svc) => {
                    service = Some(self.lower_service_annotation(svc, ServiceKind::Managed)?);
                }
                AnnotationKind::Extern(svc) => {
                    service = Some(self.lower_service_annotation(svc, ServiceKind::External)?);
                }
                AnnotationKind::K8s(k8s_ann) => {
                    k8s = Some(self.lower_k8s_annotation(k8s_ann)?);
                }
                AnnotationKind::K8sConfigmap(cm) => {
                    if let Some(ref mut k) = k8s {
                        k.configmaps.push(ConfigMount {
                            name: cm.name.node.clone(),
                            mount_path: cm.path.node.clone(),
                        });
                    }
                }
                AnnotationKind::K8sSecret(cm) => {
                    if let Some(ref mut k) = k8s {
                        k.secrets.push(ConfigMount {
                            name: cm.name.node.clone(),
                            mount_path: cm.path.node.clone(),
                        });
                    }
                }
                AnnotationKind::K8sUpload(ft) => {
                    if let Some(ref mut k) = k8s {
                        k.upload.push(self.lower_file_transfer(ft));
                    }
                }
                AnnotationKind::K8sDownload(ft) => {
                    if let Some(ref mut k) = k8s {
                        k.download.push(self.lower_file_transfer(ft));
                    }
                }
                AnnotationKind::K8sForward(pf) => {
                    if let Some(ref mut k) = k8s
                        && let Ok(forward) = self.lower_port_forward(pf)
                    {
                        k.forwards.push(forward);
                    }
                }
                AnnotationKind::Unknown { .. } => {}
            }
        }

        // extract dependencies
        let mut depends_on = Vec::new();
        let mut service_deps = Vec::new();
        for dep in &task_decl.dependencies {
            match &dep.node {
                Dependency::Task(name) => depends_on.push(name.clone()),
                Dependency::Service(name) => service_deps.push(name.clone()),
            }
        }

        // build body
        let (run, shebang) = self.lower_task_body(&task_decl.body);

        Ok(Task {
            name,
            parameters,
            run,
            depends_on,
            service_deps,
            pipe_from,
            timeout,
            retry,
            join,
            ssh,
            k8s,
            service,
            shebang,
            span: Some(task_span),
        })
    }

    fn lower_task_parameters(
        &self,
        params: &[crate::Spanned<ast::Parameter>],
    ) -> Vec<TaskParameter> {
        params
            .iter()
            .map(|p| {
                let default = p.node.default.as_ref().map(|d| match &d.node {
                    ast::ParameterDefault::Literal(s) => s.clone(),
                    ast::ParameterDefault::Variable(interp) => {
                        // resolve variable reference for default
                        self.variables
                            .get(&interp.name.node)
                            .cloned()
                            .unwrap_or_else(|| format!("{{{{{}}}}}", interp.name.node))
                    }
                });
                TaskParameter {
                    name: p.node.name.node.clone(),
                    default,
                    span: Some(p.span),
                }
            })
            .collect()
    }

    fn lower_task_body(&self, body: &Option<ast::TaskBody>) -> (Option<String>, Option<Shebang>) {
        let body = match body {
            Some(b) => b,
            None => return (None, None),
        };

        let mut lines_text = Vec::new();
        let mut shebang: Option<Shebang> = None;

        for (i, line) in body.lines.iter().enumerate() {
            match &line.node {
                BodyLine::Shebang(sh) => {
                    let shebang_line = format!("#!{}", sh.interpreter.node);
                    let args_str = sh
                        .args
                        .iter()
                        .map(|a| a.node.as_str())
                        .collect::<Vec<_>>()
                        .join(" ");
                    let full_line = if args_str.is_empty() {
                        shebang_line
                    } else {
                        format!("{} {}", shebang_line, args_str)
                    };

                    if i == 0 {
                        shebang = Shebang::parse(&full_line);
                    }
                    lines_text.push(full_line);
                }
                BodyLine::Command(cmd) => {
                    let mut line_text = String::new();
                    for seg in &cmd.segments {
                        match &seg.node {
                            CommandSegment::Text(t) => line_text.push_str(t),
                            CommandSegment::Interpolation(interp) => {
                                let var_name = &interp.name.node;
                                if let Some(val) = self.variables.get(var_name) {
                                    line_text.push_str(val);
                                } else {
                                    line_text.push_str(&format!("{{{{{}}}}}", var_name));
                                }
                            }
                        }
                    }
                    lines_text.push(line_text);
                }
                BodyLine::Empty => {
                    lines_text.push(String::new());
                }
            }
        }

        let run = if lines_text.is_empty() {
            None
        } else {
            Some(lines_text.join("\n"))
        };

        (run, shebang)
    }

    fn lower_ssh_annotation(&self, ssh: &ast::SshAnnotation) -> SshConfig {
        let host = ssh
            .host
            .as_ref()
            .map(|h| self.substitute_variables(&h.node))
            .unwrap_or_default();

        let mut config = SshConfig {
            host,
            ..Default::default()
        };

        for opt in &ssh.options {
            let key = &opt.node.key.node;
            let value = self.substitute_variables(&opt.node.value.node);
            match key.as_str() {
                "user" => config.user = Some(value),
                "port" => config.port = value.parse().ok(),
                "workdir" => config.workdir = Some(value),
                "identity" => config.identity = Some(value),
                _ => {}
            }
        }

        config
    }

    fn lower_file_transfer(&self, ft: &ast::FileTransferAnnotation) -> FileTransfer {
        FileTransfer {
            local: self.substitute_variables(&ft.local.node),
            remote: self.substitute_variables(&ft.remote.node),
        }
    }

    fn lower_service_annotation(
        &self,
        svc: &ast::ServiceAnnotation,
        kind: ServiceKind,
    ) -> Result<ServiceConfig, ParseConfigError> {
        let mut config = ServiceConfig {
            kind,
            ..Default::default()
        };

        for opt in &svc.options {
            let key = &opt.node.key.node;
            let value = self.substitute_variables(&opt.node.value.node);
            match key.as_str() {
                "ready" => config.ready = ReadinessCheck::parse(&value),
                "startup_timeout" => {
                    config.startup_timeout =
                        parse_duration(&value).map_err(|e| ParseConfigError {
                            span: opt.span,
                            message: e,
                        })?;
                }
                "shutdown_grace" => {
                    config.shutdown_grace =
                        parse_duration(&value).map_err(|e| ParseConfigError {
                            span: opt.span,
                            message: e,
                        })?;
                }
                "shutdown_kill" => {
                    config.shutdown_kill =
                        parse_duration(&value).map_err(|e| ParseConfigError {
                            span: opt.span,
                            message: e,
                        })?;
                }
                "interval" => {
                    config.interval = parse_duration(&value).map_err(|e| ParseConfigError {
                        span: opt.span,
                        message: e,
                    })?;
                }
                "log" => {
                    config.log = if value == "quiet" {
                        LogOutput::Quiet
                    } else {
                        LogOutput::Stream
                    };
                }
                "forward" => {
                    config.forward = value == "true" || value == "1";
                }
                "preflight" => {
                    config.preflight = Some(value);
                }
                _ => {}
            }
        }

        Ok(config)
    }

    fn lower_k8s_annotation(
        &self,
        k8s: &ast::K8sAnnotation,
    ) -> Result<K8sConfig, ParseConfigError> {
        let mut config = K8sConfig::default();

        if let Some(mode) = &k8s.mode {
            config.mode = match mode.node.as_str() {
                "exec" => K8sMode::Exec,
                "job" => K8sMode::Job,
                "apply" => K8sMode::Apply,
                _ => K8sMode::Job,
            };
        }

        for opt in &k8s.options {
            let key = &opt.node.key.node;
            let value = self.substitute_variables(&opt.node.value.node);
            match key.as_str() {
                "context" => config.context = Some(value),
                "namespace" => config.namespace = value,
                "selector" => config.selector = Some(value),
                "pod" => config.pod = Some(value),
                "container" => config.container = Some(value),
                "image" => config.image = Some(value),
                "cpu" => config.cpu = Some(value),
                "memory" => config.memory = Some(value),
                "service_account" => config.service_account = Some(value),
                "ttl_seconds" => config.ttl_seconds = value.parse().ok(),
                "path" => config.path = Some(value),
                "workdir" => config.workdir = Some(value),
                "wait_timeout" => {
                    config.wait_timeout =
                        Some(parse_duration(&value).map_err(|e| ParseConfigError {
                            span: opt.span,
                            message: e,
                        })?);
                }
                _ => {}
            }
        }

        Ok(config)
    }

    fn lower_port_forward(
        &self,
        pf: &ast::PortForwardAnnotation,
    ) -> Result<PortForward, ParseConfigError> {
        let local_port: u16 = pf.local_port.node.parse().map_err(|_| ParseConfigError {
            span: pf.local_port.span,
            message: "invalid local port".to_string(),
        })?;

        let remote_port: u16 = pf.remote_port.node.parse().map_err(|_| ParseConfigError {
            span: pf.remote_port.span,
            message: "invalid remote port".to_string(),
        })?;

        let resource_str = &pf.resource.node;
        let (resource_type, resource) = if let Some(idx) = resource_str.find('/') {
            (
                Some(resource_str[..idx].to_string()),
                resource_str[idx + 1..].to_string(),
            )
        } else {
            (None, resource_str.clone())
        };

        Ok(PortForward {
            local_port,
            remote_port,
            resource_type,
            resource,
        })
    }

    fn handle_set_directive(&mut self, key: &str, value: &str) {
        match key {
            "dotenv" => {
                self.dotenv.load = value == "true" || value == "1";
            }
            "dotenv-path" => {
                self.dotenv.paths.push(value.to_string());
            }
            "dotenv-required" => {
                self.dotenv.required = value == "true" || value == "1";
            }
            _ => {}
        }
    }
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    s.parse::<humantime::Duration>()
        .map(|d| d.into())
        .map_err(|e| format!("invalid duration '{}': {}", s, e))
}

fn evaluate_shell_command(cmd: &str) -> Result<String, String> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .map_err(|e| format!("failed to execute shell command: {}", e))?;

    if !output.status.success() {
        return Err(format!(
            "shell command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_service_annotation_parsing() {
        let source = r#"
# @service ready=tcp:127.0.0.1:8080 startup_timeout=1s interval=100ms log=quiet
my_service:
    sleep 30
"#;
        let config = parse_config(source).unwrap();
        let task = config.tasks.get("my_service").unwrap();
        assert!(task.service.is_some(), "service should be Some");
        let svc = task.service.as_ref().unwrap();
        assert_eq!(svc.kind, ServiceKind::Managed);
        assert_eq!(svc.startup_timeout, Duration::from_secs(1));
        // verify ready is set correctly
        assert!(svc.ready.is_some(), "ready check should be Some");
        match svc.ready.as_ref().unwrap() {
            ReadinessCheck::Tcp { host, port } => {
                assert_eq!(host, "127.0.0.1");
                assert_eq!(*port, 8080);
            }
            _ => panic!("expected TCP readiness check"),
        }
    }

    #[test]
    fn test_service_dependency_parsing() {
        let source = r#"
# @service ready=tcp:127.0.0.1:8080
svc:
    sleep 30

use_svc: service:svc
    echo hi
"#;
        let config = parse_config(source).unwrap();
        let task = config.tasks.get("use_svc").unwrap();
        assert_eq!(task.service_deps, vec!["svc"]);
    }

    #[test]
    fn test_service_preflight_with_quotes() {
        let source = r#"
# @service ready=tcp:127.0.0.1:8080 preflight="test -f /tmp/marker"
svc:
    sleep 30
"#;
        let config = parse_config(source).unwrap();
        let task = config.tasks.get("svc").unwrap();
        let svc = task.service.as_ref().unwrap();
        // preflight should NOT have quotes
        assert_eq!(svc.preflight, Some("test -f /tmp/marker".to_string()));
    }

    #[test]
    fn test_command_readiness_with_quotes() {
        let source = r#"
# @service ready=cmd:"test -f /tmp/marker" startup_timeout=10s interval=100ms log=quiet
svc:
    touch /tmp/marker && sleep 30
"#;
        let config = parse_config(source).unwrap();
        let task = config.tasks.get("svc").unwrap();
        let svc = task.service.as_ref().unwrap();
        assert!(svc.ready.is_some(), "ready check should be Some");
        match svc.ready.as_ref().unwrap() {
            ReadinessCheck::Command { cmd } => {
                assert_eq!(cmd, "test -f /tmp/marker");
            }
            _ => panic!("expected Command readiness check, got {:?}", svc.ready),
        }
    }
}
