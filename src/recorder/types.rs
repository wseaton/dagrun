//! Data types for run history recording.

use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime};

/// Opaque run identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RunId(pub i64);

/// Opaque task execution identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskExecutionId(pub i64);

/// Output stream type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamType {
    Stdout,
    Stderr,
}

impl StreamType {
    pub fn as_str(&self) -> &'static str {
        match self {
            StreamType::Stdout => "stdout",
            StreamType::Stderr => "stderr",
        }
    }
}

/// A chunk of output from task execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputChunk {
    pub stream: StreamType,
    pub line: String,
    pub timestamp: SystemTime,
}

impl OutputChunk {
    pub fn stdout(line: String) -> Self {
        Self {
            stream: StreamType::Stdout,
            line,
            timestamp: SystemTime::now(),
        }
    }

    pub fn stderr(line: String) -> Self {
        Self {
            stream: StreamType::Stderr,
            line,
            timestamp: SystemTime::now(),
        }
    }
}

/// Summary of a historical run for listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSummary {
    pub id: RunId,
    pub config_path: String,
    pub target_task: String,
    pub args: Vec<String>,
    pub started_at: SystemTime,
    pub completed_at: Option<SystemTime>,
    pub success: Option<bool>,
    pub duration: Option<Duration>,
    pub task_count: u32,
    pub failed_count: u32,
}

/// Detailed task execution record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskExecution {
    pub id: TaskExecutionId,
    pub run_id: RunId,
    pub task_name: String,
    pub attempt: u32,
    pub started_at: SystemTime,
    pub completed_at: Option<SystemTime>,
    pub status: Option<String>,
    pub duration: Option<Duration>,
}

/// Configuration for retention policies.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct RetentionPolicy {
    /// Maximum age of runs to keep.
    pub max_age_days: u32,
    /// Maximum total size of output data in bytes.
    pub max_output_bytes: u64,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            max_age_days: 30,
            max_output_bytes: 100 * 1024 * 1024, // 100MB
        }
    }
}
