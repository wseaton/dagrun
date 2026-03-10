//! CLI history query command.

use colored::Colorize;
use std::sync::Arc;

use crate::recorder::{RecorderError, SqliteRecorder};

/// Run the history command - list recent runs or show details.
pub async fn run_history(
    limit: usize,
    task_filter: Option<&str>,
    failed_only: bool,
    run_id: Option<i64>,
    format: &str,
) -> Result<(), RecorderError> {
    let recorder = Arc::new(SqliteRecorder::open(None)?);

    if let Some(rid) = run_id {
        // Show details for specific run
        let run = recorder.get_run(crate::recorder::RunId(rid)).await?;
        let tasks = recorder
            .get_tasks_for_run(crate::recorder::RunId(rid))
            .await?;

        match format {
            "json" => {
                let output = serde_json::json!({
                    "run": run,
                    "tasks": tasks,
                });
                println!("{}", serde_json::to_string_pretty(&output).unwrap());
            }
            _ => {
                print_run_details(&run, &tasks);
            }
        }
    } else {
        // List recent runs
        let runs = recorder.list_runs(limit, task_filter, failed_only).await?;

        match format {
            "json" => {
                println!("{}", serde_json::to_string_pretty(&runs).unwrap());
            }
            _ => {
                print_runs_table(&runs);
            }
        }
    }

    Ok(())
}

fn print_runs_table(runs: &[crate::recorder::types::RunSummary]) {
    if runs.is_empty() {
        println!("{}", "No runs found.".dimmed());
        return;
    }

    println!(
        "{:>6}  {:19}  {:20}  {:8}  {:>5}  {:>8}",
        "ID".bold(),
        "Started".bold(),
        "Target".bold(),
        "Status".bold(),
        "Tasks".bold(),
        "Duration".bold()
    );
    println!("{}", "-".repeat(75));

    for run in runs {
        let status = match run.success {
            Some(true) => "success".green(),
            Some(false) => "failed".red(),
            None => "running".yellow(),
        };

        let duration = run
            .duration
            .map(|d| format!("{:.1}s", d.as_secs_f64()))
            .unwrap_or_else(|| "-".to_string());

        let started = run
            .started_at
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| {
                let secs = d.as_secs();

                chrono_lite(secs)
            })
            .unwrap_or_else(|_| "unknown".to_string());

        println!(
            "{:>6}  {:19}  {:20}  {:8}  {:>5}  {:>8}",
            run.id.0,
            started,
            truncate(&run.target_task, 20),
            status,
            run.task_count,
            duration
        );
    }
}

fn print_run_details(
    run: &crate::recorder::types::RunSummary,
    tasks: &[crate::recorder::types::TaskExecution],
) {
    let status = match run.success {
        Some(true) => "success".green(),
        Some(false) => "failed".red(),
        None => "running".yellow(),
    };

    println!("{} #{}", "Run".bold(), run.id.0);
    println!("  Target: {}", run.target_task);
    println!("  Status: {}", status);
    println!("  Config: {}", run.config_path);
    if !run.args.is_empty() {
        println!("  Args: {}", run.args.join(" "));
    }
    if let Some(duration) = run.duration {
        println!("  Duration: {:.2}s", duration.as_secs_f64());
    }

    if !tasks.is_empty() {
        println!("\n{}", "Tasks:".bold());
        for task in tasks {
            let task_status = match task.status.as_deref() {
                Some("success") => "✓".green(),
                Some("failed") => "✗".red(),
                Some("running") => "▶".blue(),
                _ => "·".dimmed(),
            };
            let duration = task
                .duration
                .map(|d| format!("{:.2}s", d.as_secs_f64()))
                .unwrap_or_else(|| "-".to_string());
            let attempt = if task.attempt > 1 {
                format!(" (attempt {})", task.attempt)
            } else {
                String::new()
            };
            println!(
                "  {} {:30} {:>8}{}",
                task_status, task.task_name, duration, attempt
            );
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max - 3])
    }
}

/// Simple timestamp formatting without chrono dependency.
fn chrono_lite(unix_secs: u64) -> String {
    // This is a simplified version; for production use chrono crate
    let secs_per_day = 86400u64;
    let secs_per_hour = 3600u64;
    let secs_per_min = 60u64;

    // Days since Unix epoch
    let days = unix_secs / secs_per_day;
    let remaining = unix_secs % secs_per_day;

    let hours = remaining / secs_per_hour;
    let remaining = remaining % secs_per_hour;
    let minutes = remaining / secs_per_min;
    let seconds = remaining % secs_per_min;

    // Very rough date calculation (doesn't handle leap years properly)
    let mut year = 1970u32;
    let mut remaining_days = days;

    loop {
        let days_in_year =
            if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) {
                366
            } else {
                365
            };
        if remaining_days < days_in_year {
            break;
        }
        remaining_days -= days_in_year;
        year += 1;
    }

    let days_in_months: [u64; 12] =
        if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) {
            [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
        } else {
            [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
        };

    let mut month = 1u32;
    for &days_in_month in &days_in_months {
        if remaining_days < days_in_month {
            break;
        }
        remaining_days -= days_in_month;
        month += 1;
    }
    let day = remaining_days + 1;

    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        year, month, day, hours, minutes, seconds
    )
}
