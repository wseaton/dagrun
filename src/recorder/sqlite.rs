//! SQLite-based run history recorder.

use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use rusqlite::{Connection, params};
use tokio::sync::Mutex;

use crate::executor::TaskStatus;
use crate::recorder::types::{
    OutputChunk, RetentionPolicy, RunId, RunSummary, StreamType, TaskExecution, TaskExecutionId,
};
use crate::recorder::{Recorder, RecorderError};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    config_path TEXT NOT NULL,
    target_task TEXT NOT NULL,
    args TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    completed_at INTEGER,
    success INTEGER,
    duration_ms INTEGER
);

CREATE INDEX IF NOT EXISTS idx_runs_started_at ON runs(started_at DESC);
CREATE INDEX IF NOT EXISTS idx_runs_target_task ON runs(target_task);

CREATE TABLE IF NOT EXISTS task_executions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    task_name TEXT NOT NULL,
    attempt INTEGER NOT NULL DEFAULT 1,
    started_at INTEGER NOT NULL,
    completed_at INTEGER,
    status TEXT,
    duration_ms INTEGER
);

CREATE INDEX IF NOT EXISTS idx_task_exec_run_id ON task_executions(run_id);
CREATE INDEX IF NOT EXISTS idx_task_exec_started ON task_executions(started_at DESC);

CREATE TABLE IF NOT EXISTS task_output (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_exec_id INTEGER NOT NULL REFERENCES task_executions(id) ON DELETE CASCADE,
    stream TEXT NOT NULL,
    line TEXT NOT NULL,
    timestamp INTEGER NOT NULL,
    seq INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_output_task_exec ON task_output(task_exec_id, seq);
"#;

/// SQLite-backed recorder for run history.
pub struct SqliteRecorder {
    conn: Mutex<Connection>,
    /// Sequence counter for output chunks within each task execution.
    /// This is a simplification; in practice we'd track per-task-execution.
    output_seq: AtomicI64,
    #[allow(dead_code)]
    retention: RetentionPolicy,
}

impl SqliteRecorder {
    /// Open or create the history database at the given path.
    ///
    /// If no path is provided, uses the default location:
    /// `~/.local/share/dr/history.db`
    pub fn open(path: Option<PathBuf>) -> Result<Self, RecorderError> {
        let db_path = path.unwrap_or_else(default_db_path);

        // Ensure parent directory exists
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn =
            Connection::open(&db_path).map_err(|e| RecorderError::Database(e.to_string()))?;

        // Enable WAL mode for better concurrent read performance
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|e| RecorderError::Database(e.to_string()))?;

        // Run schema migrations
        conn.execute_batch(SCHEMA)
            .map_err(|e| RecorderError::Database(e.to_string()))?;

        Ok(Self {
            conn: Mutex::new(conn),
            output_seq: AtomicI64::new(0),
            retention: RetentionPolicy::default(),
        })
    }

    /// Set the retention policy.
    #[allow(dead_code)]
    pub fn with_retention(mut self, policy: RetentionPolicy) -> Self {
        self.retention = policy;
        self
    }

    /// Apply retention policies, deleting old runs and excess output.
    #[allow(dead_code)]
    pub async fn apply_retention(&self) -> Result<(), RecorderError> {
        let conn = self.conn.lock().await;

        // Time-based: delete runs older than max_age_days
        let cutoff_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64
            - (self.retention.max_age_days as i64 * 24 * 60 * 60 * 1000);

        conn.execute("DELETE FROM runs WHERE started_at <= ?", params![cutoff_ms])
            .map_err(|e| RecorderError::Database(e.to_string()))?;

        // Size-based: check total output size and prune if needed
        let total_bytes: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(LENGTH(line)), 0) FROM task_output",
                [],
                |row| row.get(0),
            )
            .map_err(|e| RecorderError::Database(e.to_string()))?;

        if total_bytes as u64 > self.retention.max_output_bytes {
            // Delete output from oldest 25% of runs
            conn.execute_batch(
                "DELETE FROM task_output WHERE task_exec_id IN (
                    SELECT te.id FROM task_executions te
                    JOIN runs r ON te.run_id = r.id
                    ORDER BY r.started_at ASC
                    LIMIT (SELECT COUNT(*) / 4 FROM task_executions)
                )",
            )
            .map_err(|e| RecorderError::Database(e.to_string()))?;
        }

        Ok(())
    }

    /// List recent runs, optionally filtered.
    pub async fn list_runs(
        &self,
        limit: usize,
        task_filter: Option<&str>,
        failed_only: bool,
    ) -> Result<Vec<RunSummary>, RecorderError> {
        let conn = self.conn.lock().await;

        let mut sql = String::from(
            "SELECT r.id, r.config_path, r.target_task, r.args, r.started_at,
                    r.completed_at, r.success, r.duration_ms,
                    (SELECT COUNT(*) FROM task_executions WHERE run_id = r.id) as task_count,
                    (SELECT COUNT(*) FROM task_executions WHERE run_id = r.id AND status = 'failed') as failed_count
             FROM runs r WHERE 1=1",
        );

        if let Some(task) = task_filter {
            sql.push_str(&format!(" AND r.target_task LIKE '%{}%'", task));
        }
        if failed_only {
            sql.push_str(" AND r.success = 0");
        }
        sql.push_str(" ORDER BY r.started_at DESC LIMIT ?");

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| RecorderError::Database(e.to_string()))?;

        let runs = stmt
            .query_map(params![limit as i64], |row| {
                let args_json: String = row.get(3)?;
                let args: Vec<String> = serde_json::from_str(&args_json).unwrap_or_default();

                let started_ms: i64 = row.get(4)?;
                let completed_ms: Option<i64> = row.get(5)?;
                let duration_ms: Option<i64> = row.get(7)?;

                Ok(RunSummary {
                    id: RunId(row.get(0)?),
                    config_path: row.get(1)?,
                    target_task: row.get(2)?,
                    args,
                    started_at: UNIX_EPOCH + Duration::from_millis(started_ms as u64),
                    completed_at: completed_ms
                        .map(|ms| UNIX_EPOCH + Duration::from_millis(ms as u64)),
                    success: row.get(6)?,
                    duration: duration_ms.map(|ms| Duration::from_millis(ms as u64)),
                    task_count: row.get(8)?,
                    failed_count: row.get(9)?,
                })
            })
            .map_err(|e| RecorderError::Database(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| RecorderError::Database(e.to_string()))?;

        Ok(runs)
    }

    /// Get a specific run by ID.
    pub async fn get_run(&self, run_id: RunId) -> Result<RunSummary, RecorderError> {
        let conn = self.conn.lock().await;

        let sql = "SELECT r.id, r.config_path, r.target_task, r.args, r.started_at,
                          r.completed_at, r.success, r.duration_ms,
                          (SELECT COUNT(*) FROM task_executions WHERE run_id = r.id) as task_count,
                          (SELECT COUNT(*) FROM task_executions WHERE run_id = r.id AND status = 'failed') as failed_count
                   FROM runs r WHERE r.id = ?";

        conn.query_row(sql, params![run_id.0], |row| {
            let args_json: String = row.get(3)?;
            let args: Vec<String> = serde_json::from_str(&args_json).unwrap_or_default();

            let started_ms: i64 = row.get(4)?;
            let completed_ms: Option<i64> = row.get(5)?;
            let duration_ms: Option<i64> = row.get(7)?;

            Ok(RunSummary {
                id: RunId(row.get(0)?),
                config_path: row.get(1)?,
                target_task: row.get(2)?,
                args,
                started_at: UNIX_EPOCH + Duration::from_millis(started_ms as u64),
                completed_at: completed_ms.map(|ms| UNIX_EPOCH + Duration::from_millis(ms as u64)),
                success: row.get(6)?,
                duration: duration_ms.map(|ms| Duration::from_millis(ms as u64)),
                task_count: row.get(8)?,
                failed_count: row.get(9)?,
            })
        })
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => RecorderError::RunNotFound(run_id.0),
            _ => RecorderError::Database(e.to_string()),
        })
    }

    /// Get task executions for a run.
    pub async fn get_tasks_for_run(
        &self,
        run_id: RunId,
    ) -> Result<Vec<TaskExecution>, RecorderError> {
        let conn = self.conn.lock().await;

        let mut stmt = conn
            .prepare(
                "SELECT id, run_id, task_name, attempt, started_at, completed_at, status, duration_ms
                 FROM task_executions WHERE run_id = ? ORDER BY started_at ASC",
            )
            .map_err(|e| RecorderError::Database(e.to_string()))?;

        let tasks = stmt
            .query_map(params![run_id.0], |row| {
                let started_ms: i64 = row.get(4)?;
                let completed_ms: Option<i64> = row.get(5)?;
                let duration_ms: Option<i64> = row.get(7)?;

                Ok(TaskExecution {
                    id: TaskExecutionId(row.get(0)?),
                    run_id: RunId(row.get(1)?),
                    task_name: row.get(2)?,
                    attempt: row.get(3)?,
                    started_at: UNIX_EPOCH + Duration::from_millis(started_ms as u64),
                    completed_at: completed_ms
                        .map(|ms| UNIX_EPOCH + Duration::from_millis(ms as u64)),
                    status: row.get(6)?,
                    duration: duration_ms.map(|ms| Duration::from_millis(ms as u64)),
                })
            })
            .map_err(|e| RecorderError::Database(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| RecorderError::Database(e.to_string()))?;

        Ok(tasks)
    }

    /// Get output for a task execution.
    pub async fn get_output_for_task(
        &self,
        task_exec_id: TaskExecutionId,
    ) -> Result<Vec<OutputChunk>, RecorderError> {
        let conn = self.conn.lock().await;

        let mut stmt = conn
            .prepare(
                "SELECT stream, line, timestamp FROM task_output
                 WHERE task_exec_id = ? ORDER BY seq ASC",
            )
            .map_err(|e| RecorderError::Database(e.to_string()))?;

        let chunks = stmt
            .query_map(params![task_exec_id.0], |row| {
                let stream_str: String = row.get(0)?;
                let stream = if stream_str == "stderr" {
                    StreamType::Stderr
                } else {
                    StreamType::Stdout
                };
                let timestamp_ms: i64 = row.get(2)?;

                Ok(OutputChunk {
                    stream,
                    line: row.get(1)?,
                    timestamp: UNIX_EPOCH + Duration::from_millis(timestamp_ms as u64),
                })
            })
            .map_err(|e| RecorderError::Database(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| RecorderError::Database(e.to_string()))?;

        Ok(chunks)
    }
}

#[async_trait]
impl Recorder for SqliteRecorder {
    async fn record_run_start(
        &self,
        config_path: &str,
        target_task: &str,
        args: &[String],
    ) -> Result<RunId, RecorderError> {
        let conn = self.conn.lock().await;

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;

        let args_json = serde_json::to_string(args).unwrap_or_else(|_| "[]".to_string());

        conn.execute(
            "INSERT INTO runs (config_path, target_task, args, started_at) VALUES (?, ?, ?, ?)",
            params![config_path, target_task, args_json, now_ms],
        )
        .map_err(|e| RecorderError::Database(e.to_string()))?;

        let id = conn.last_insert_rowid();
        Ok(RunId(id))
    }

    async fn record_run_complete(
        &self,
        run_id: RunId,
        success: bool,
        total_duration: Duration,
    ) -> Result<(), RecorderError> {
        let conn = self.conn.lock().await;

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;

        let duration_ms = total_duration.as_millis() as i64;

        conn.execute(
            "UPDATE runs SET completed_at = ?, success = ?, duration_ms = ? WHERE id = ?",
            params![now_ms, success as i32, duration_ms, run_id.0],
        )
        .map_err(|e| RecorderError::Database(e.to_string()))?;

        Ok(())
    }

    async fn record_task_start(
        &self,
        run_id: RunId,
        task_name: &str,
        attempt: u32,
    ) -> Result<TaskExecutionId, RecorderError> {
        let conn = self.conn.lock().await;

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;

        // Reset sequence counter for new task
        self.output_seq.store(0, Ordering::Relaxed);

        conn.execute(
            "INSERT INTO task_executions (run_id, task_name, attempt, started_at, status) VALUES (?, ?, ?, ?, 'running')",
            params![run_id.0, task_name, attempt, now_ms],
        )
        .map_err(|e| RecorderError::Database(e.to_string()))?;

        let id = conn.last_insert_rowid();
        Ok(TaskExecutionId(id))
    }

    async fn record_task_complete(
        &self,
        task_exec_id: TaskExecutionId,
        status: TaskStatus,
        duration: Duration,
    ) -> Result<(), RecorderError> {
        let conn = self.conn.lock().await;

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;

        let status_str = match status {
            TaskStatus::Success => "success",
            TaskStatus::Failed => "failed",
            TaskStatus::Skipped => "skipped",
            TaskStatus::Pending => "pending",
            TaskStatus::Running => "running",
        };

        let duration_ms = duration.as_millis() as i64;

        conn.execute(
            "UPDATE task_executions SET completed_at = ?, status = ?, duration_ms = ? WHERE id = ?",
            params![now_ms, status_str, duration_ms, task_exec_id.0],
        )
        .map_err(|e| RecorderError::Database(e.to_string()))?;

        Ok(())
    }

    async fn record_output_chunk(
        &self,
        task_exec_id: TaskExecutionId,
        chunk: OutputChunk,
    ) -> Result<(), RecorderError> {
        let conn = self.conn.lock().await;

        let timestamp_ms = chunk
            .timestamp
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;

        let seq = self.output_seq.fetch_add(1, Ordering::Relaxed);

        conn.execute(
            "INSERT INTO task_output (task_exec_id, stream, line, timestamp, seq) VALUES (?, ?, ?, ?, ?)",
            params![task_exec_id.0, chunk.stream.as_str(), chunk.line, timestamp_ms, seq],
        )
        .map_err(|e| RecorderError::Database(e.to_string()))?;

        Ok(())
    }
}

/// Get the default database path.
fn default_db_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("dr")
        .join("history.db")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_record_run_lifecycle() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let recorder = SqliteRecorder::open(Some(db_path)).unwrap();

        // Start a run
        let run_id = recorder
            .record_run_start("dagfile", "build", &["--release".to_string()])
            .await
            .unwrap();
        assert!(run_id.0 > 0);

        // Start a task
        let task_id = recorder
            .record_task_start(run_id, "compile", 1)
            .await
            .unwrap();
        assert!(task_id.0 > 0);

        // Record some output
        recorder
            .record_output_chunk(task_id, OutputChunk::stdout("Building...".to_string()))
            .await
            .unwrap();
        recorder
            .record_output_chunk(
                task_id,
                OutputChunk::stderr("Warning: deprecated".to_string()),
            )
            .await
            .unwrap();

        // Complete the task
        recorder
            .record_task_complete(task_id, TaskStatus::Success, Duration::from_secs(5))
            .await
            .unwrap();

        // Complete the run
        recorder
            .record_run_complete(run_id, true, Duration::from_secs(10))
            .await
            .unwrap();

        // Verify data
        let runs = recorder.list_runs(10, None, false).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].target_task, "build");
        assert_eq!(runs[0].success, Some(true));
        assert_eq!(runs[0].task_count, 1);

        // Verify tasks
        let tasks = recorder.get_tasks_for_run(run_id).await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].task_name, "compile");
        assert_eq!(tasks[0].status, Some("success".to_string()));

        // Verify output
        let output = recorder.get_output_for_task(task_id).await.unwrap();
        assert_eq!(output.len(), 2);
        assert_eq!(output[0].line, "Building...");
        assert_eq!(output[1].stream, StreamType::Stderr);
    }

    #[tokio::test]
    async fn test_list_runs_with_filter() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("filter.db");
        let recorder = SqliteRecorder::open(Some(db_path)).unwrap();

        // Create multiple runs
        let run1 = recorder
            .record_run_start("dagfile", "build", &[])
            .await
            .unwrap();
        recorder
            .record_run_complete(run1, true, Duration::from_secs(1))
            .await
            .unwrap();

        let run2 = recorder
            .record_run_start("dagfile", "test", &[])
            .await
            .unwrap();
        recorder
            .record_run_complete(run2, false, Duration::from_secs(2))
            .await
            .unwrap();

        let run3 = recorder
            .record_run_start("dagfile", "build", &[])
            .await
            .unwrap();
        recorder
            .record_run_complete(run3, true, Duration::from_secs(1))
            .await
            .unwrap();

        // Filter by task
        let builds = recorder.list_runs(10, Some("build"), false).await.unwrap();
        assert_eq!(builds.len(), 2);

        // Filter by failed
        let failed = recorder.list_runs(10, None, true).await.unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].target_task, "test");
    }

    #[tokio::test]
    async fn test_retention_time_based() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("retention.db");
        let recorder =
            SqliteRecorder::open(Some(db_path))
                .unwrap()
                .with_retention(RetentionPolicy {
                    max_age_days: 0, // Expire immediately
                    max_output_bytes: u64::MAX,
                });

        // Create a run
        let run_id = recorder
            .record_run_start("dagfile", "old_task", &[])
            .await
            .unwrap();
        recorder
            .record_run_complete(run_id, true, Duration::from_secs(1))
            .await
            .unwrap();

        // Apply retention (should delete the run since max_age_days = 0)
        recorder.apply_retention().await.unwrap();

        let runs = recorder.list_runs(10, None, false).await.unwrap();
        assert!(runs.is_empty());
    }
}
