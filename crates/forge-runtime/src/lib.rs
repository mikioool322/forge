//! Forge agent runtime: vendor-independent adapters and subprocess
//! supervision.
//!
//! * [`adapter`] — the `AgentAdapter` trait, probe helpers, classification
//! * [`codex`] / [`opencode`] / [`generic`] — concrete adapters
//! * [`supervisor`] — process supervision, log streaming, cancellation
//! * [`platform`] — executable resolution, process groups, tree kill

pub mod adapter;
pub mod codex;
pub mod error;
pub mod generic;
pub mod opencode;
pub mod platform;
pub mod supervisor;

pub use adapter::{
    adapter_for, AgentAdapter, AgentJob, ExitClass, HelpFlags, PreparedCommand, ProbeOutcome,
};
pub use error::RuntimeError;
pub use supervisor::{spawn_run, RunEvent, RunHandle, RunOutcome, RunParams};
