use colored::{Color, Colorize};
use std::io::{self, IsTerminal, Write};
use tracing::field::Visit;
use tracing::{Event, Subscriber};
use tracing_subscriber::{Layer, layer::Context, registry::LookupSpan};

/// color palette for task output cycling
const TASK_COLORS: &[Color] = &[
    Color::Cyan,
    Color::Magenta,
    Color::Yellow,
    Color::Blue,
    Color::Green,
];

/// get a consistent color for a task name based on hash
pub fn task_color(task_name: &str) -> Color {
    let hash: usize = task_name.bytes().map(|b| b as usize).sum();
    TASK_COLORS[hash % TASK_COLORS.len()]
}

pub struct PrettyProgressLayer {
    is_tty: bool,
}

impl Default for PrettyProgressLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl PrettyProgressLayer {
    pub fn new() -> Self {
        Self {
            is_tty: io::stderr().is_terminal(),
        }
    }
}

impl<S> Layer<S> for PrettyProgressLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = ProgressVisitor::default();
        event.record(&mut visitor);

        let Some(progress) = visitor.progress else {
            return;
        };

        let task = visitor.task.as_deref().unwrap_or("unknown");
        let color = task_color(task);

        let mut stderr = io::stderr().lock();
        match progress.as_str() {
            "start" => {
                if self.is_tty {
                    let _ = writeln!(stderr, "{} {}", "▶".color(color), task.color(color));
                } else {
                    let _ = writeln!(stderr, "▶ {}", task);
                }
            }
            "done" => {
                let duration = format_duration(visitor.duration_ms);
                if self.is_tty {
                    let _ = writeln!(
                        stderr,
                        "{} {} {}",
                        "✓".green(),
                        task.color(color),
                        duration.dimmed()
                    );
                } else {
                    let _ = writeln!(stderr, "✓ {} {}", task, duration);
                }
            }
            "retry" => {
                let attempt = visitor.attempt.unwrap_or(0);
                if self.is_tty {
                    let _ = writeln!(
                        stderr,
                        "{} {} (attempt {}, retrying...)",
                        "↻".yellow(),
                        task.color(color),
                        attempt
                    );
                } else {
                    let _ = writeln!(stderr, "↻ {} (attempt {}, retrying...)", task, attempt);
                }
            }
            "failed" => {
                if self.is_tty {
                    let _ = writeln!(stderr, "{} {}", "✗".red(), task.color(color));
                    if let Some(err) = &visitor.error {
                        let _ = writeln!(stderr, "  {} {}", "│".red(), err);
                    }
                } else {
                    let _ = writeln!(stderr, "✗ {}", task);
                    if let Some(err) = &visitor.error {
                        let _ = writeln!(stderr, "  │ {}", err);
                    }
                }
            }
            _ => {}
        }
    }
}

#[derive(Default)]
struct ProgressVisitor {
    progress: Option<String>,
    task: Option<String>,
    duration_ms: Option<u64>,
    attempt: Option<u32>,
    error: Option<String>,
}

impl Visit for ProgressVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        match field.name() {
            "progress" => self.progress = Some(value.to_string()),
            "task" => self.task = Some(value.to_string()),
            "error" => self.error = Some(value.to_string()),
            _ => {}
        }
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        match field.name() {
            "duration_ms" => self.duration_ms = Some(value),
            "attempt" => self.attempt = Some(value as u32),
            "max_attempts" => {}
            _ => {}
        }
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        match field.name() {
            "duration_ms" => self.duration_ms = Some(value as u64),
            "attempt" => self.attempt = Some(value as u32),
            _ => {}
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let s = format!("{:?}", value);
        let s = s.trim_matches('"');
        match field.name() {
            "progress" => self.progress = Some(s.to_string()),
            "task" => self.task = Some(s.to_string()),
            "error" => self.error = Some(s.to_string()),
            _ => {}
        }
    }
}

fn format_duration(ms: Option<u64>) -> String {
    match ms {
        Some(ms) if ms < 1000 => format!("({}ms)", ms),
        Some(ms) => format!("({:.1}s)", ms as f64 / 1000.0),
        None => String::new(),
    }
}
