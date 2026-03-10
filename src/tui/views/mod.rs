//! TUI view rendering.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};

use crate::recorder::types::StreamType;
use crate::tui::{TuiState, View};

/// Render the current view.
pub fn render(frame: &mut Frame, state: &TuiState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(frame.area());

    match state.view {
        View::RunList => render_run_list(frame, state, chunks[0]),
        View::TaskList => render_task_list(frame, state, chunks[0]),
        View::Output => render_output(frame, state, chunks[0]),
    }

    render_help(frame, state, chunks[1]);
}

fn render_run_list(frame: &mut Frame, state: &TuiState, area: Rect) {
    let items: Vec<ListItem> = state
        .runs
        .iter()
        .enumerate()
        .map(|(i, run)| {
            let status_style = match run.success {
                Some(true) => Style::default().fg(Color::Green),
                Some(false) => Style::default().fg(Color::Red),
                None => Style::default().fg(Color::Yellow),
            };

            let status = match run.success {
                Some(true) => "✓",
                Some(false) => "✗",
                None => "▶",
            };

            let duration = run
                .duration
                .map(|d| format!("{:.1}s", d.as_secs_f64()))
                .unwrap_or_else(|| "-".to_string());

            let line = Line::from(vec![
                Span::styled(
                    format!("{:>5} ", run.id.0),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(format!("{} ", status), status_style),
                Span::raw(format!("{:20} ", truncate(&run.target_task, 20))),
                Span::styled(
                    format!("{:>3} tasks ", run.task_count),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(format!("{:>8}", duration)),
            ]);

            let style = if i == state.run_cursor {
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            ListItem::new(line).style(style)
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Run History "),
    );

    frame.render_widget(list, area);
}

fn render_task_list(frame: &mut Frame, state: &TuiState, area: Rect) {
    let run_info = state
        .selected_run
        .and_then(|id| state.runs.iter().find(|r| r.id == id))
        .map(|r| format!(" Run #{}: {} ", r.id.0, r.target_task))
        .unwrap_or_else(|| " Tasks ".to_string());

    let items: Vec<ListItem> = state
        .tasks
        .iter()
        .enumerate()
        .map(|(i, task)| {
            let status_style = match task.status.as_deref() {
                Some("success") => Style::default().fg(Color::Green),
                Some("failed") => Style::default().fg(Color::Red),
                Some("running") => Style::default().fg(Color::Blue),
                _ => Style::default().fg(Color::DarkGray),
            };

            let status = match task.status.as_deref() {
                Some("success") => "✓",
                Some("failed") => "✗",
                Some("running") => "▶",
                _ => "·",
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

            let line = Line::from(vec![
                Span::styled(format!("{} ", status), status_style),
                Span::raw(format!("{:30}", task.task_name)),
                Span::styled(
                    format!("{:>8}", duration),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(attempt, Style::default().fg(Color::Yellow)),
            ]);

            let style = if i == state.task_cursor {
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            ListItem::new(line).style(style)
        })
        .collect();

    let list = List::new(items).block(Block::default().borders(Borders::ALL).title(run_info));

    frame.render_widget(list, area);
}

fn render_output(frame: &mut Frame, state: &TuiState, area: Rect) {
    let task_info = state
        .selected_task
        .and_then(|id| state.tasks.iter().find(|t| t.id == id))
        .map(|t| format!(" Output: {} ", t.task_name))
        .unwrap_or_else(|| " Output ".to_string());

    let lines: Vec<Line> = state
        .output
        .iter()
        .skip(state.output_scroll)
        .map(|chunk| {
            let style = match chunk.stream {
                StreamType::Stdout => Style::default(),
                StreamType::Stderr => Style::default().fg(Color::Red),
            };
            Line::styled(&chunk.line, style)
        })
        .collect();

    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(task_info))
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, area);
}

fn render_help(frame: &mut Frame, state: &TuiState, area: Rect) {
    let help_text = match state.view {
        View::RunList => "[Enter] view tasks  [R] rerun  [r] refresh  [j/k] navigate  [q] quit",
        View::TaskList => "[Enter] view output  [R] rerun  [Esc] back  [j/k] navigate  [q] back",
        View::Output => "[Esc/q] back  [j/k] scroll  [PgUp/PgDn] page  [g/G] top/bottom",
    };

    let help = Paragraph::new(help_text)
        .style(Style::default().fg(Color::DarkGray))
        .block(Block::default().borders(Borders::ALL));

    frame.render_widget(help, area);
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max - 3])
    }
}
