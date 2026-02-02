mod dag;
mod env;
mod executor;
mod justfile;
mod k8s;
mod lua;
mod progress;
mod service;
mod ssh;

use clap::{Parser, Subcommand};
use colored::Colorize;
use std::path::PathBuf;
use std::process::Command as StdCommand;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

use crate::dag::TaskGraph;
use crate::executor::{Executor, TaskStatus};
use crate::justfile::load_justflow;
use crate::lua::load_lua_config;
use dr_ast::{Config, Task};
use serde::Serialize;

/// JSON output for `list --format json`
#[derive(Serialize)]
struct ListOutput {
    tasks: Vec<TaskInfo>,
}

#[derive(Serialize)]
struct TaskInfo {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    run: Option<String>,
    depends_on: Vec<String>,
    #[serde(skip_serializing_if = "JustflowExtras::is_empty")]
    justflow: JustflowExtras,
}

#[derive(Serialize, Default)]
struct JustflowExtras {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pipe_from: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout: Option<String>,
    #[serde(skip_serializing_if = "is_zero")]
    retry: u32,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    join: bool,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl JustflowExtras {
    fn is_empty(&self) -> bool {
        self.pipe_from.is_empty() && self.timeout.is_none() && self.retry == 0 && !self.join
    }
}

impl ListOutput {
    fn from_graph(graph: &TaskGraph) -> Self {
        let tasks = graph
            .task_names()
            .iter()
            .map(|name| {
                let task = graph.task(name).unwrap();
                TaskInfo::from_task(task)
            })
            .collect();
        ListOutput { tasks }
    }
}

impl TaskInfo {
    fn from_task(task: &Task) -> Self {
        TaskInfo {
            name: task.name.clone(),
            run: task.run.clone(),
            depends_on: task.depends_on.clone(),
            justflow: JustflowExtras {
                pipe_from: task.pipe_from.clone(),
                timeout: task
                    .timeout
                    .map(|d| humantime::format_duration(d).to_string()),
                retry: task.retry,
                join: task.join,
            },
        }
    }
}

#[derive(Parser)]
#[command(name = "dr")]
#[command(about = "DAG-based task runner with retry and timeout support", long_about = None)]
struct Cli {
    /// Path to config file (dagfile or .lua)
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    /// Enable verbose debug logging
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Suppress status output, only show task output
    #[arg(short, long, global = true)]
    quiet: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a specific task and its dependencies
    #[command(hide = true)]
    Run {
        /// Task name to run
        task: String,

        /// Run only this task, skip dependencies
        #[arg(long)]
        only: bool,

        /// Positional arguments for task parameters
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Run all tasks in the graph
    RunAll,

    /// List all available tasks
    List {
        /// Output format: text or json
        #[arg(short, long, default_value = "text")]
        format: String,
    },

    /// Show the task graph
    Graph {
        /// Output format: ascii, dot, or png
        #[arg(short, long, default_value = "ascii")]
        format: String,

        /// Output file for png format
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Validate the config file
    Validate,

    /// Run a task (implicit when task name is provided)
    #[command(external_subcommand)]
    External(Vec<String>),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    setup_tracing(cli.verbose, cli.quiet);

    let config_path = cli.config.unwrap_or_else(find_config_file);
    let config = load_config(&config_path)?;

    // load dotenv files if configured
    if let Err(e) = env::load_dotenv(&config.dotenv) {
        anyhow::bail!("Failed to load dotenv: {}", e);
    }

    let graph = TaskGraph::from_config(config)?;

    // determine what to run: explicit subcommand or implicit task name
    let (task, only, args) = match cli.command {
        Commands::Run { task, only, args } => (task, only, args),
        Commands::External(ext_args) => {
            // parse external args: first is task name, rest are args
            // check for --only flag
            let mut task_name = None;
            let mut only = false;
            let mut task_args = Vec::new();

            for arg in ext_args.iter() {
                if arg == "--only" {
                    only = true;
                } else if task_name.is_none() {
                    task_name = Some(arg);
                } else {
                    task_args.push(arg.to_owned());
                }
            }

            let Some(task) = task_name else {
                // no task specified, show help
                use clap::CommandFactory;
                Cli::command().print_help()?;
                return Ok(());
            };
            (task.to_owned(), only, task_args)
        }
        Commands::RunAll => {
            let executor = Executor::new(graph);
            executor.register_services().await;
            let results = executor.run_all().await?;
            executor.close().await;
            print_results(&results);
            if results.iter().any(|r| r.status == TaskStatus::Failed) {
                std::process::exit(1);
            }
            return Ok(());
        }
        Commands::List { format } => {
            match format.as_str() {
                "json" => {
                    let output = ListOutput::from_graph(&graph);
                    println!("{}", serde_json::to_string_pretty(&output).unwrap());
                }
                _ => {
                    println!("{}", "Tasks:".bold());
                    for name in graph.task_names() {
                        let task = graph.task(name).unwrap();
                        let deps = if task.depends_on.is_empty() {
                            String::new()
                        } else {
                            format!(" (depends on: {})", task.depends_on.join(", "))
                        };
                        println!("  {} {}{}", "•".cyan(), name, deps.dimmed());
                    }
                }
            }
            return Ok(());
        }
        Commands::Graph { format, output } => {
            match format.as_str() {
                "ascii" => {
                    println!("{}", graph.to_ascii());
                }
                "dot" => {
                    println!("{}", graph.to_dot());
                }
                "png" => {
                    let dot = graph.to_dot();
                    let out_path = output.unwrap_or_else(|| PathBuf::from("dr-graph.png"));

                    // pipe to dot command
                    let mut child = StdCommand::new("dot")
                        .args(["-Tpng", "-o"])
                        .arg(&out_path)
                        .stdin(std::process::Stdio::piped())
                        .spawn()?;

                    use std::io::Write;
                    child.stdin.as_mut().unwrap().write_all(dot.as_bytes())?;
                    child.wait()?;

                    println!("Graph written to {}", out_path.display());
                }
                _ => {
                    anyhow::bail!("Unknown format: {}. Use ascii, dot, or png", format);
                }
            }
            return Ok(());
        }
        Commands::Validate => {
            println!("{} Config is valid!", "✓".green());
            println!("  {} tasks defined", graph.task_names().len());
            return Ok(());
        }
    };

    let executor = Executor::new(graph);
    executor.register_services().await;

    let results = if only {
        // run just this task (no deps)
        if let Some(t) = executor.graph.task(&task) {
            let bound_task = bind_task_parameters(t, &args)?;
            vec![executor.execute_single(&bound_task).await]
        } else {
            executor.close().await;
            anyhow::bail!("Task '{}' not found", task);
        }
    } else {
        executor.run_task_with_args(&task, &args).await?
    };

    executor.close().await;
    print_results(&results);
    if results.iter().any(|r| r.status == TaskStatus::Failed) {
        std::process::exit(1);
    }

    Ok(())
}

fn find_config_file() -> PathBuf {
    for name in ["dagfile", "dagfile.dr", "dagfile.lua"] {
        let path = PathBuf::from(name);
        if path.exists() {
            return path;
        }
    }
    PathBuf::from("dagfile")
}

fn load_config(path: &PathBuf) -> anyhow::Result<Config> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

    match ext {
        "lua" => load_lua_config(path).map_err(|e| anyhow::anyhow!("{}", e)),
        "dr" => load_justflow(path).map_err(|e| anyhow::anyhow!("{}", e)),
        _ if filename == "dagfile" || ext.is_empty() => {
            load_justflow(path).map_err(|e| anyhow::anyhow!("{}", e))
        }
        _ => anyhow::bail!(
            "Unknown config format: {}. Use .dr or .lua extension, or name the file 'dagfile'",
            path.display()
        ),
    }
}

/// Bind CLI arguments to task parameters, substituting in the task body
fn bind_task_parameters(task: &Task, args: &[String]) -> anyhow::Result<Task> {
    let params = &task.parameters;

    // validate argument count
    let required_count = params.iter().filter(|p| p.default.is_none()).count();
    if args.len() < required_count {
        let param_names: Vec<_> = params
            .iter()
            .filter(|p| p.default.is_none())
            .map(|p| p.name.as_str())
            .collect();
        anyhow::bail!(
            "Task '{}' requires {} argument(s) ({}) but got {}",
            task.name,
            required_count,
            param_names.join(", "),
            args.len()
        );
    }
    if args.len() > params.len() {
        anyhow::bail!(
            "Task '{}' accepts {} argument(s) but got {}",
            task.name,
            params.len(),
            args.len()
        );
    }

    // build parameter bindings
    let mut bindings = std::collections::HashMap::new();
    for (i, param) in params.iter().enumerate() {
        let value = if i < args.len() {
            args[i].clone()
        } else if let Some(default) = &param.default {
            default.clone()
        } else {
            anyhow::bail!("Missing required argument '{}'", param.name);
        };
        bindings.insert(param.name.clone(), value);
    }

    // substitute parameters in task body
    let run = task.run.as_ref().map(|body| {
        let mut result = body.clone();
        for (name, value) in &bindings {
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

fn setup_tracing(verbose: bool, quiet: bool) {
    use crate::progress::PrettyProgressLayer;

    if quiet {
        // no output at all from tracing
        return;
    }

    let progress_layer = PrettyProgressLayer::new();

    if verbose {
        // add standard fmt layer for debug output
        let fmt_layer = tracing_subscriber::fmt::layer()
            .with_target(true)
            .with_level(true);
        let filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("dr=debug"));

        tracing_subscriber::registry()
            .with(progress_layer)
            .with(fmt_layer.with_filter(filter))
            .init();
    } else {
        // just pretty progress, filter out non-progress events
        tracing_subscriber::registry().with(progress_layer).init();
    }
}

fn print_results(results: &[executor::TaskResult]) {
    println!("\n{}", "Results:".bold());
    for result in results {
        let status = match result.status {
            TaskStatus::Success => "✓".green(),
            TaskStatus::Failed => "✗".red(),
            TaskStatus::Skipped => "○".yellow(),
            TaskStatus::Running => "▶".blue(),
            TaskStatus::Pending => "·".dimmed(),
        };
        let attempts = if result.attempts > 1 {
            format!(" ({} attempts)", result.attempts)
                .dimmed()
                .to_string()
        } else {
            String::new()
        };
        println!("  {} {}{}", status, result.task_name, attempts);
    }
}
