//! Error types for the store layer.

use std::path::PathBuf;

use forge_core::model::TaskId;
use forge_core::state::TransitionError;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("cannot create data directory {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("store worker thread is gone")]
    WorkerGone,
    #[error(
        "database schema version {db} is newer than this binary supports ({binary}); upgrade forge"
    )]
    SchemaNewer { db: i64, binary: i64 },
    #[error("illegal state transition: {0}")]
    IllegalTransition(#[from] TransitionError),
    #[error("task {id} is not in expected state {expected}")]
    StateConflict { id: TaskId, expected: &'static str },
    #[error("task {0} not found")]
    TaskNotFound(TaskId),
    #[error("project {0:?} not found")]
    ProjectNotFound(String),
    #[error("duplicate request id {0:?}")]
    DuplicateRequest(String),
    #[error("{entity} {key:?} not found")]
    NotFound { entity: &'static str, key: String },
    #[error("corrupt data in column {column}: {detail}")]
    Corrupt {
        column: &'static str,
        detail: String,
    },
}
