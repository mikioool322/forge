//! `forge-core` contains the deterministic, I/O-free heart of Forge:
//!
//! * the durable task state machine,
//! * the dependency graph (DAG) utilities,
//! * the scheduler decision function,
//! * the shared domain model and configuration schema.
//!
//! Nothing in this crate performs I/O with the outside world beyond reading
//! configuration files, which keeps it trivially unit-testable.

pub mod config;
pub mod dag;
pub mod model;
pub mod scheduler;
pub mod state;

pub use config::{ForgeConfig, ForgePaths};
pub use dag::{Dag, DagError};
pub use model::{
    now_ms, AgentInfo, AgentKind, AgentSession, AgentSpec, AgentState, EventKind, EventRecord,
    NormalizedEvent, PlanFile, PlanTaskSpec, ProbeStatus, Project, ResourceLease, SessionState,
    Severity, StreamKind, Task, TaskArtifact, TaskId, TaskState, Worktree, WorktreeState,
};
pub use scheduler::{schedule, AgentScheduleView, ScheduledAction, SchedulerInput, SchedulerTask};
pub use state::{is_legal_transition, TransitionError};
