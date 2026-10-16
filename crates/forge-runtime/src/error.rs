//! Runtime errors.

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("agent executable {0:?} was not found (not on PATH and not an existing path)")]
    ExecutableNotFound(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("adapter error: {0}")]
    Adapter(String),
    #[error("run failed to start: {0}")]
    Spawn(String),
}
