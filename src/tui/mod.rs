//! TUI application for browsing run history.

mod views;

use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::recorder::types::{RunSummary, TaskExecution, TaskExecutionId};
use crate::recorder::{OutputChunk, RunId, SqliteRecorder};

/// Current view in the TUI.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum View {
    RunList,
    TaskList,
    Output,
}

/// TUI application state.
pub struct TuiState {
    pub view: View,
    pub runs: Vec<RunSummary>,
    pub run_cursor: usize,
    pub selected_run: Option<RunId>,
    pub tasks: Vec<TaskExecution>,
    pub task_cursor: usize,
    pub selected_task: Option<TaskExecutionId>,
    pub output: Vec<OutputChunk>,
    pub output_scroll: usize,
    pub should_quit: bool,
    pub message: Option<String>,
}

impl Default for TuiState {
    fn default() -> Self {
        Self {
            view: View::RunList,
            runs: vec![],
            run_cursor: 0,
            selected_run: None,
            tasks: vec![],
            task_cursor: 0,
            selected_task: None,
            output: vec![],
            output_scroll: 0,
            should_quit: false,
            message: None,
        }
    }
}

/// TUI application.
pub struct TuiApp {
    state: TuiState,
    recorder: Arc<SqliteRecorder>,
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TuiApp {
    /// Create a new TUI application.
    pub fn new(recorder: Arc<SqliteRecorder>) -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;

        Ok(Self {
            state: TuiState::default(),
            recorder,
            terminal,
        })
    }

    /// Run the TUI event loop.
    pub async fn run(&mut self) -> io::Result<()> {
        // Load initial data
        self.refresh_runs().await;

        loop {
            // Draw UI
            self.terminal.draw(|f| views::render(f, &self.state))?;

            // Handle events
            if event::poll(Duration::from_millis(100))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                self.handle_key(key.code).await;
            }

            if self.state.should_quit {
                break;
            }
        }

        Ok(())
    }

    async fn handle_key(&mut self, key: KeyCode) {
        match self.state.view {
            View::RunList => self.handle_run_list_key(key).await,
            View::TaskList => self.handle_task_list_key(key).await,
            View::Output => self.handle_output_key(key).await,
        }
    }

    async fn handle_run_list_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Char('q') => self.state.should_quit = true,
            KeyCode::Up | KeyCode::Char('k') => {
                self.state.run_cursor = self.state.run_cursor.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.state.run_cursor + 1 < self.state.runs.len() {
                    self.state.run_cursor += 1;
                }
            }
            KeyCode::Enter => {
                if let Some(run) = self.state.runs.get(self.state.run_cursor) {
                    self.state.selected_run = Some(run.id);
                    self.load_tasks().await;
                    self.state.view = View::TaskList;
                    self.state.task_cursor = 0;
                }
            }
            KeyCode::Char('r') => {
                self.refresh_runs().await;
            }
            KeyCode::Char('R') => {
                if let Some(run) = self.state.runs.get(self.state.run_cursor).cloned() {
                    self.rerun(&run).await;
                }
            }
            _ => {}
        }
    }

    async fn handle_task_list_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.state.view = View::RunList;
                self.state.selected_run = None;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.state.task_cursor = self.state.task_cursor.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.state.task_cursor + 1 < self.state.tasks.len() {
                    self.state.task_cursor += 1;
                }
            }
            KeyCode::Enter => {
                if let Some(task) = self.state.tasks.get(self.state.task_cursor) {
                    self.state.selected_task = Some(task.id);
                    self.load_output().await;
                    self.state.view = View::Output;
                    self.state.output_scroll = 0;
                }
            }
            KeyCode::Char('R') => {
                if let Some(run_id) = self.state.selected_run
                    && let Ok(run) = self.recorder.get_run(run_id).await
                {
                    self.rerun(&run).await;
                }
            }
            _ => {}
        }
    }

    async fn handle_output_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.state.view = View::TaskList;
                self.state.selected_task = None;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.state.output_scroll = self.state.output_scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.state.output_scroll + 1 < self.state.output.len() {
                    self.state.output_scroll += 1;
                }
            }
            KeyCode::PageUp => {
                self.state.output_scroll = self.state.output_scroll.saturating_sub(20);
            }
            KeyCode::PageDown => {
                self.state.output_scroll =
                    (self.state.output_scroll + 20).min(self.state.output.len().saturating_sub(1));
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.state.output_scroll = 0;
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.state.output_scroll = self.state.output.len().saturating_sub(1);
            }
            _ => {}
        }
    }

    async fn refresh_runs(&mut self) {
        match self.recorder.list_runs(100, None, false).await {
            Ok(runs) => {
                self.state.runs = runs;
                self.state.run_cursor = self
                    .state
                    .run_cursor
                    .min(self.state.runs.len().saturating_sub(1).max(0));
            }
            Err(e) => {
                self.state.message = Some(format!("Failed to load runs: {}", e));
            }
        }
    }

    async fn load_tasks(&mut self) {
        if let Some(run_id) = self.state.selected_run {
            match self.recorder.get_tasks_for_run(run_id).await {
                Ok(tasks) => self.state.tasks = tasks,
                Err(e) => {
                    self.state.message = Some(format!("Failed to load tasks: {}", e));
                }
            }
        }
    }

    async fn load_output(&mut self) {
        if let Some(task_id) = self.state.selected_task {
            match self.recorder.get_output_for_task(task_id).await {
                Ok(output) => self.state.output = output,
                Err(e) => {
                    self.state.message = Some(format!("Failed to load output: {}", e));
                }
            }
        }
    }

    async fn rerun(&mut self, run: &RunSummary) {
        // Shell out to dr run
        let mut cmd = std::process::Command::new("dr");
        cmd.arg("-c").arg(&run.config_path);
        cmd.arg("run").arg(&run.target_task);
        for arg in &run.args {
            cmd.arg(arg);
        }

        match cmd.spawn() {
            Ok(_) => {
                self.state.message = Some(format!("Rerunning: {}", run.target_task));
            }
            Err(e) => {
                self.state.message = Some(format!("Failed to rerun: {}", e));
            }
        }
    }
}

impl Drop for TuiApp {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
    }
}

/// Launch the TUI.
pub async fn run_tui() -> io::Result<()> {
    let recorder = Arc::new(SqliteRecorder::open(None).map_err(io::Error::other)?);

    let mut app = TuiApp::new(recorder)?;
    app.run().await
}
