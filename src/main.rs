mod dag;
mod env;
mod executor;
mod justfile;
mod k8s;
mod lua;
mod service;
mod ssh;

use clap::{Parser, Subcommand};
use colored::Colorize;
use std::path::PathBuf;
use std::process::Command as StdCommand;
use tracing::Level;
use tracing_subscriber::EnvFilter;

use crate::dag::TaskGraph;
use crate::executor::{Executor, TaskStatus};
use crate::justfile::load_justflow;
use crate::lua::load_lua_config;
use dagrun_ast::{Config, Task};
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
#[command(name = "dagrun")]
#[command(about = "DAG-based task runner with retry and timeout support", long_about = None)]
struct Cli {
    /// Path to config file (dagrun or .lua)
    #[arg(short, long)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a specific task and its dependencies
    Run {
        /// Task name to run
        task: String,

        /// Run only this task, skip dependencies
        #[arg(long)]
        only: bool,
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(Level::INFO.into()))
        .init();

    let cli = Cli::parse();

    let config_path = cli.config.unwrap_or_else(find_config_file);
    let config = load_config(&config_path)?;

    // load dotenv files if configured
    if let Err(e) = env::load_dotenv(&config.dotenv) {
        anyhow::bail!("Failed to load dotenv: {}", e);
    }

    let graph = TaskGraph::from_config(config)?;

    match cli.command {
        Commands::Run { task, only } => {
            let executor = Executor::new(graph);
            executor.register_services().await;

            let results = if only {
                // run just this task (no deps)
                if let Some(t) = executor.graph.task(&task) {
                    vec![executor.execute_single(t).await]
                } else {
                    executor.close().await;
                    anyhow::bail!("Task '{}' not found", task);
                }
            } else {
                executor.run_task(&task).await?
            };

            executor.close().await;
            print_results(&results);
            if results.iter().any(|r| r.status == TaskStatus::Failed) {
                std::process::exit(1);
            }
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
        }

        Commands::List { format } => match format.as_str() {
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
        },

        Commands::Graph { format, output } => match format.as_str() {
            "ascii" => {
                println!("{}", graph.to_ascii());
            }
            "dot" => {
                println!("{}", graph.to_dot());
            }
            "png" => {
                let dot = graph.to_dot();
                let out_path = output.unwrap_or_else(|| PathBuf::from("dagrun-graph.png"));

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
        },

        Commands::Validate => {
            println!("{} Config is valid!", "✓".green());
            println!("  {} tasks defined", graph.task_names().len());
        }
    }

    Ok(())
}

fn find_config_file() -> PathBuf {
    for name in ["dagrun", "dagrun.dr", "dagrun.lua"] {
        let path = PathBuf::from(name);
        if path.exists() {
            return path;
        }
    }
    PathBuf::from("dagrun")
}

fn load_config(path: &PathBuf) -> anyhow::Result<Config> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

    match ext {
        "lua" => load_lua_config(path).map_err(|e| anyhow::anyhow!("{}", e)),
        "dr" | "dagrun" => load_justflow(path).map_err(|e| anyhow::anyhow!("{}", e)),
        _ if filename == "dagrun" || ext.is_empty() => {
            load_justflow(path).map_err(|e| anyhow::anyhow!("{}", e))
        }
        _ => anyhow::bail!(
            "Unknown config format: {}. Use .dr, .dagrun, or .lua",
            path.display()
        ),
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
