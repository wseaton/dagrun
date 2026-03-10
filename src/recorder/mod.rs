//! Run history recording infrastructure.
//!
//! This module provides a trait-based interface for recording task execution
//! history, with implementations for SQLite storage and a no-op recorder.

pub mod sqlite;
pub mod types;

use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;

use crate::executor::TaskStatus;
pub use crate::recorder::sqlite::SqliteRecorder;
pub use crate::recorder::types::{OutputChunk, RunId, TaskExecutionId};

#[derive(Error, Debug)]
pub enum RecorderError {
    #[error("database error: {0}")]
    Database(String),
    #[error("run not found: {0}")]
    RunNotFound(i64),
    #[allow(dead_code)]
    #[error("task execution not found: {0}")]
    TaskExecutionNotFound(i64),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Recording interface for task execution history.
///
/// All methods are async and implementations must be Send + Sync for use
/// across async task boundaries.
#[async_trait]
pub trait Recorder: Send + Sync {
    /// Record the start of a new run. Returns the run ID.
    async fn record_run_start(
        &self,
        config_path: &str,
        target_task: &str,
        args: &[String],
    ) -> Result<RunId, RecorderError>;

    /// Record run completion (success or failure).
    async fn record_run_complete(
        &self,
        run_id: RunId,
        success: bool,
        total_duration: Duration,
    ) -> Result<(), RecorderError>;

    /// Record the start of a task execution within a run.
    async fn record_task_start(
        &self,
        run_id: RunId,
        task_name: &str,
        attempt: u32,
    ) -> Result<TaskExecutionId, RecorderError>;

    /// Record task completion.
    async fn record_task_complete(
        &self,
        task_exec_id: TaskExecutionId,
        status: TaskStatus,
        duration: Duration,
    ) -> Result<(), RecorderError>;

    /// Stream an output chunk (stdout or stderr line).
    async fn record_output_chunk(
        &self,
        task_exec_id: TaskExecutionId,
        chunk: OutputChunk,
    ) -> Result<(), RecorderError>;
}

/// No-op recorder for --no-record mode or testing.
///
/// All operations succeed immediately without persisting anything.
pub struct NoOpRecorder;

#[async_trait]
impl Recorder for NoOpRecorder {
    async fn record_run_start(
        &self,
        _config_path: &str,
        _target_task: &str,
        _args: &[String],
    ) -> Result<RunId, RecorderError> {
        Ok(RunId(0))
    }

    async fn record_run_complete(
        &self,
        _run_id: RunId,
        _success: bool,
        _total_duration: Duration,
    ) -> Result<(), RecorderError> {
        Ok(())
    }

    async fn record_task_start(
        &self,
        _run_id: RunId,
        _task_name: &str,
        _attempt: u32,
    ) -> Result<TaskExecutionId, RecorderError> {
        Ok(TaskExecutionId(0))
    }

    async fn record_task_complete(
        &self,
        _task_exec_id: TaskExecutionId,
        _status: TaskStatus,
        _duration: Duration,
    ) -> Result<(), RecorderError> {
        Ok(())
    }

    async fn record_output_chunk(
        &self,
        _task_exec_id: TaskExecutionId,
        _chunk: OutputChunk,
    ) -> Result<(), RecorderError> {
        Ok(())
    }
}
