//! Shared domain model for Forge.
//!
//! These types are serialized over IPC and persisted in SQLite (as JSON for
//! list-like fields, as scalar columns for the rest).

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

pub type ProjectId = i64;
pub type TaskId = i64;
pub type PlanId = i64;
pub type SessionId = i64;
pub type WorktreeId = i64;
pub type EventId = i64;

/// Milliseconds since the Unix epoch (UTC).
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

// ---------------------------------------------------------------------------
// Task state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskState {
    Created,
    Ready,
    Leased,
    Running,
    ReviewPending,
    Blocked,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl TaskState {
    pub const ALL: [TaskState; 10] = [
        TaskState::Created,
        TaskState::Ready,
        TaskState::Leased,
        TaskState::Running,
        TaskState::ReviewPending,
        TaskState::Blocked,
        TaskState::Completed,
        TaskState::Failed,
        TaskState::Cancelled,
        TaskState::Interrupted,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            TaskState::Created => "CREATED",
            TaskState::Ready => "READY",
            TaskState::Leased => "LEASED",
            TaskState::Running => "RUNNING",
            TaskState::ReviewPending => "REVIEW_PENDING",
            TaskState::Blocked => "BLOCKED",
            TaskState::Completed => "COMPLETED",
            TaskState::Failed => "FAILED",
            TaskState::Cancelled => "CANCELLED",
            TaskState::Interrupted => "INTERRUPTED",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|st| st.as_str() == s)
    }

    /// A task in a terminal state will not progress without an explicit user
    /// action (retry for `Failed`/`Interrupted` is a user action).
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskState::Completed | TaskState::Failed | TaskState::Cancelled
        )
    }

    /// The task owns a lease / a running process.
    pub fn is_active(self) -> bool {
        matches!(self, TaskState::Leased | TaskState::Running)
    }

    /// Waiting for something else (dependencies, agent capacity).
    pub fn is_pending(self) -> bool {
        matches!(
            self,
            TaskState::Created | TaskState::Ready | TaskState::Blocked | TaskState::ReviewPending
        )
    }
}

impl fmt::Display for TaskState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TaskState {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        TaskState::parse(s).ok_or_else(|| format!("unknown task state: {s}"))
    }
}

// ---------------------------------------------------------------------------
// Agents
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Codex,
    Opencode,
    /// Vendor-independent adapter: runs an arbitrary command with templated
    /// arguments. Used for controlled test processes and user-defined CLIs.
    Generic,
}

impl AgentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentKind::Codex => "codex",
            AgentKind::Opencode => "opencode",
            AgentKind::Generic => "generic",
        }
    }
}

impl fmt::Display for AgentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AgentKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "codex" => Ok(AgentKind::Codex),
            "opencode" => Ok(AgentKind::Opencode),
            "generic" => Ok(AgentKind::Generic),
            other => Err(format!("unknown agent kind: {other}")),
        }
    }
}

pub const fn default_slots() -> u32 {
    1
}

pub const fn default_true() -> bool {
    true
}

/// Static agent configuration (from `config.toml`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSpec {
    pub id: String,
    pub kind: AgentKind,
    /// Executable name or absolute path. `@self` resolves to the running
    /// `forge` binary (used by the bundled demo/test agent).
    pub command: String,
    #[serde(default)]
    pub extra_args: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default = "default_slots")]
    pub slots: u32,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// If true the task prompt is written to the process stdin instead of
    /// being appended to the argument list (adapter specific).
    #[serde(default)]
    pub stdin_prompt: bool,
}

impl AgentSpec {
    pub fn validate(&self) -> Result<(), String> {
        if self.id.is_empty()
            || !self
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            return Err(format!("agent id {:?} must match [A-Za-z0-9._-]+", self.id));
        }
        if self.command.trim().is_empty() {
            return Err(format!("agent {:?} has an empty command", self.id));
        }
        if self.slots == 0 {
            return Err(format!("agent {:?} must have slots >= 1", self.id));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AgentState {
    Idle,
    Running,
    Offline,
    Disabled,
}

impl AgentState {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentState::Idle => "IDLE",
            AgentState::Running => "RUNNING",
            AgentState::Offline => "OFFLINE",
            AgentState::Disabled => "DISABLED",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "IDLE" => Some(AgentState::Idle),
            "RUNNING" => Some(AgentState::Running),
            "OFFLINE" => Some(AgentState::Offline),
            "DISABLED" => Some(AgentState::Disabled),
            _ => None,
        }
    }
}

/// Result of probing an agent executable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ProbeStatus {
    Unknown,
    Ok { version: Option<String> },
    Missing { detail: String },
    AuthRequired { detail: String },
    Incompatible { detail: String },
    Error { detail: String },
}

impl ProbeStatus {
    pub fn label(&self) -> &'static str {
        match self {
            ProbeStatus::Unknown => "unknown",
            ProbeStatus::Ok { .. } => "ok",
            ProbeStatus::Missing { .. } => "missing",
            ProbeStatus::AuthRequired { .. } => "auth_required",
            ProbeStatus::Incompatible { .. } => "incompatible",
            ProbeStatus::Error { .. } => "error",
        }
    }

    pub fn detail(&self) -> Option<&str> {
        match self {
            ProbeStatus::Unknown | ProbeStatus::Ok { .. } => None,
            ProbeStatus::Missing { detail }
            | ProbeStatus::AuthRequired { detail }
            | ProbeStatus::Incompatible { detail }
            | ProbeStatus::Error { detail } => Some(detail),
        }
    }

    pub fn is_ok(&self) -> bool {
        matches!(self, ProbeStatus::Ok { .. })
    }
}

/// Live view of an agent (config + runtime state).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub id: String,
    pub kind: AgentKind,
    pub command: String,
    pub capabilities: Vec<String>,
    pub slots: u32,
    pub enabled: bool,
    pub state: AgentState,
    pub running: u32,
    pub version: Option<String>,
    pub probe: ProbeStatus,
    pub probed_at: Option<i64>,
}

// ---------------------------------------------------------------------------
// Projects, tasks, worktrees, sessions, leases, artifacts
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    pub root: PathBuf,
    pub default_branch: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub project_id: ProjectId,
    pub seq_no: i64,
    pub title: String,
    pub spec: String,
    pub state: TaskState,
    pub priority: i64,
    pub required_caps: Vec<String>,
    pub claims: Vec<String>,
    pub preferred_agent: Option<String>,
    pub agent_id: Option<String>,
    pub worktree_id: Option<WorktreeId>,
    pub base_commit: Option<String>,
    pub timeout_secs: Option<u64>,
    pub attempts: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub exit_code: Option<i32>,
    pub failure_reason: Option<String>,
    pub log_path: Option<String>,
    pub summary: Option<String>,
    pub plan_id: Option<PlanId>,
    pub plan_key: Option<String>,
}

impl Task {
    /// Human/CLI-facing identifier, e.g. `T-001`.
    pub fn display_id(&self) -> String {
        format!("T-{:03}", self.seq_no)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WorktreeState {
    Active,
    Retained,
    Removed,
    Failed,
}

impl WorktreeState {
    pub fn as_str(self) -> &'static str {
        match self {
            WorktreeState::Active => "ACTIVE",
            WorktreeState::Retained => "RETAINED",
            WorktreeState::Removed => "REMOVED",
            WorktreeState::Failed => "FAILED",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ACTIVE" => Some(WorktreeState::Active),
            "RETAINED" => Some(WorktreeState::Retained),
            "REMOVED" => Some(WorktreeState::Removed),
            "FAILED" => Some(WorktreeState::Failed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Worktree {
    pub id: WorktreeId,
    pub project_id: ProjectId,
    pub task_id: TaskId,
    pub path: PathBuf,
    pub branch: String,
    pub base_commit: Option<String>,
    pub state: WorktreeState,
    pub created_at: i64,
    pub removed_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SessionState {
    Starting,
    Running,
    Exited,
    Killed,
    Crashed,
}

impl SessionState {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionState::Starting => "STARTING",
            SessionState::Running => "RUNNING",
            SessionState::Exited => "EXITED",
            SessionState::Killed => "KILLED",
            SessionState::Crashed => "CRASHED",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "STARTING" => Some(SessionState::Starting),
            "RUNNING" => Some(SessionState::Running),
            "EXITED" => Some(SessionState::Exited),
            "KILLED" => Some(SessionState::Killed),
            "CRASHED" => Some(SessionState::Crashed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSession {
    pub id: SessionId,
    pub agent_id: String,
    pub task_id: TaskId,
    pub pid: Option<u32>,
    pub state: SessionState,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub exit_code: Option<i32>,
    pub worktree_id: Option<WorktreeId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceLease {
    pub id: i64,
    pub resource: String,
    pub task_id: TaskId,
    pub session_id: Option<SessionId>,
    pub acquired_at: i64,
    pub expires_at: i64,
    pub heartbeat_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskArtifact {
    pub id: i64,
    pub task_id: TaskId,
    pub kind: String,
    pub path: Option<String>,
    pub value: Option<String>,
    pub created_at: i64,
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warn,
    Error,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warn => "warn",
            Severity::Error => "error",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "info" => Some(Severity::Info),
            "warn" => Some(Severity::Warn),
            "error" => Some(Severity::Error),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    TaskCreated,
    TaskStateChanged,
    TaskDispatched,
    ProcessStarted,
    ProcessExited,
    OutputEvent,
    WorktreeCreated,
    WorktreeRetained,
    LeaseAcquired,
    LeaseReleased,
    ArtifactRecorded,
    AuthRequired,
    AgentProbe,
    Recovery,
    DaemonNote,
    LogDropped,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::TaskCreated => "task_created",
            EventKind::TaskStateChanged => "task_state_changed",
            EventKind::TaskDispatched => "task_dispatched",
            EventKind::ProcessStarted => "process_started",
            EventKind::ProcessExited => "process_exited",
            EventKind::OutputEvent => "output_event",
            EventKind::WorktreeCreated => "worktree_created",
            EventKind::WorktreeRetained => "worktree_retained",
            EventKind::LeaseAcquired => "lease_acquired",
            EventKind::LeaseReleased => "lease_released",
            EventKind::ArtifactRecorded => "artifact_recorded",
            EventKind::AuthRequired => "auth_required",
            EventKind::AgentProbe => "agent_probe",
            EventKind::Recovery => "recovery",
            EventKind::DaemonNote => "daemon_note",
            EventKind::LogDropped => "log_dropped",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "task_created" => EventKind::TaskCreated,
            "task_state_changed" => EventKind::TaskStateChanged,
            "task_dispatched" => EventKind::TaskDispatched,
            "process_started" => EventKind::ProcessStarted,
            "process_exited" => EventKind::ProcessExited,
            "output_event" => EventKind::OutputEvent,
            "worktree_created" => EventKind::WorktreeCreated,
            "worktree_retained" => EventKind::WorktreeRetained,
            "lease_acquired" => EventKind::LeaseAcquired,
            "lease_released" => EventKind::LeaseReleased,
            "artifact_recorded" => EventKind::ArtifactRecorded,
            "auth_required" => EventKind::AuthRequired,
            "agent_probe" => EventKind::AgentProbe,
            "recovery" => EventKind::Recovery,
            "daemon_note" => EventKind::DaemonNote,
            "log_dropped" => EventKind::LogDropped,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamKind {
    Stdout,
    Stderr,
    System,
}

/// Event emitted by an agent adapter after normalizing raw process output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizedEvent {
    pub kind: EventKind,
    pub severity: Severity,
    pub message: String,
    #[serde(default)]
    pub data: Option<serde_json::Value>,
    #[serde(default = "default_stream")]
    pub stream: StreamKind,
}

fn default_stream() -> StreamKind {
    StreamKind::Stdout
}

/// Durable event journal entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
    pub id: EventId,
    pub ts: i64,
    pub project_id: Option<ProjectId>,
    pub task_id: Option<TaskId>,
    pub agent_id: Option<String>,
    pub kind: EventKind,
    pub severity: Severity,
    pub message: String,
    #[serde(default)]
    pub data: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Plans
// ---------------------------------------------------------------------------

/// Deterministic task plan submitted via `forge run <plan.toml>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanFile {
    #[serde(default)]
    pub plan_version: Option<u32>,
    /// Registered project name (see `forge init`).
    pub project: String,
    /// Optional idempotency key; if omitted the CLI generates one.
    #[serde(default)]
    pub request_id: Option<String>,
    pub tasks: Vec<PlanTaskSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanTaskSpec {
    /// Local key used only to express dependencies inside the plan.
    pub key: String,
    pub title: String,
    #[serde(default)]
    pub spec: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub deps: Vec<String>,
    #[serde(default)]
    pub claims: Vec<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub priority: i64,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlanError {
    #[error("plan contains no tasks")]
    EmptyPlan,
    #[error("duplicate task key {0:?}")]
    DuplicateKey(String),
    #[error("task key {0:?} is invalid (allowed: [A-Za-z0-9._-], 1..=64 chars)")]
    BadKey(String),
    #[error("task {task:?} depends on unknown key {dep:?}")]
    UnknownDep { task: String, dep: String },
    #[error("dependency cycle involving keys {0:?}")]
    Cycle(Vec<String>),
}

pub fn validate_plan(plan: &PlanFile) -> Result<(), PlanError> {
    if plan.tasks.is_empty() {
        return Err(PlanError::EmptyPlan);
    }
    let mut keys = std::collections::BTreeSet::new();
    for t in &plan.tasks {
        if t.key.is_empty()
            || t.key.len() > 64
            || !t
                .key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            return Err(PlanError::BadKey(t.key.clone()));
        }
        if !keys.insert(t.key.clone()) {
            return Err(PlanError::DuplicateKey(t.key.clone()));
        }
    }
    for t in &plan.tasks {
        for dep in &t.deps {
            if !keys.contains(dep) {
                return Err(PlanError::UnknownDep {
                    task: t.key.clone(),
                    dep: dep.clone(),
                });
            }
        }
    }
    // Cycle detection (Kahn over string keys).
    let mut indeg: BTreeMap<&str, usize> = plan
        .tasks
        .iter()
        .map(|t| (t.key.as_str(), t.deps.len()))
        .collect();
    let mut ready: std::collections::BTreeSet<&str> = indeg
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(k, _)| *k)
        .collect();
    let mut removed = 0usize;
    while let Some(k) = ready.pop_first() {
        removed += 1;
        for t in &plan.tasks {
            if t.deps.iter().any(|d| d == k) {
                let e = indeg.get_mut(t.key.as_str()).expect("known key");
                *e -= 1;
                if *e == 0 {
                    ready.insert(t.key.as_str());
                }
            }
        }
    }
    if removed != plan.tasks.len() {
        let mut cycle: Vec<String> = indeg
            .iter()
            .filter(|(_, d)| **d > 0)
            .map(|(k, _)| (*k).to_string())
            .collect();
        cycle.sort();
        return Err(PlanError::Cycle(cycle));
    }
    Ok(())
}

/// Normalize a resource claim string into `kind:value` form.
///
/// Recognized prefixes: `file:` (default), `dir:`, `resource:`.
pub fn normalize_claim(raw: &str) -> String {
    let raw = raw.trim();
    if let Some(rest) = raw.strip_prefix("file:") {
        format!("file:{}", rest.trim_start_matches('/'))
    } else if let Some(rest) = raw.strip_prefix("dir:") {
        format!("dir:{}", rest.trim_start_matches('/').trim_end_matches('/'))
    } else if raw.starts_with("resource:") {
        raw.to_string()
    } else {
        format!("file:{}", raw.trim_start_matches('/'))
    }
}

/// Two claims conflict if they refer to overlapping filesystem paths or to
/// the same opaque resource.
pub fn claims_conflict(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (ka, va) = a.split_once(':').unwrap_or(("resource", a));
    let (kb, vb) = b.split_once(':').unwrap_or(("resource", b));
    match (ka, kb) {
        ("resource", "resource") => va == vb,
        ("file", "file") => false,
        ("dir", "dir") => is_path_prefix(va, vb) || is_path_prefix(vb, va),
        ("dir", "file") => is_path_prefix(va, vb),
        ("file", "dir") => is_path_prefix(vb, va),
        // `file` vs `resource` and `dir` vs `resource` never overlap.
        _ => false,
    }
}

fn is_path_prefix(dir: &str, file: &str) -> bool {
    file == dir || file.starts_with(&format!("{dir}/"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_display_id_is_padded() {
        let t = Task {
            id: 1,
            project_id: 1,
            seq_no: 7,
            title: "x".into(),
            spec: String::new(),
            state: TaskState::Created,
            priority: 0,
            required_caps: vec![],
            claims: vec![],
            preferred_agent: None,
            agent_id: None,
            worktree_id: None,
            base_commit: None,
            timeout_secs: None,
            attempts: 0,
            created_at: 0,
            updated_at: 0,
            started_at: None,
            finished_at: None,
            exit_code: None,
            failure_reason: None,
            log_path: None,
            summary: None,
            plan_id: None,
            plan_key: None,
        };
        assert_eq!(t.display_id(), "T-007");
    }

    #[test]
    fn claim_normalization() {
        assert_eq!(normalize_claim("src/main.rs"), "file:src/main.rs");
        assert_eq!(normalize_claim("/src/main.rs"), "file:src/main.rs");
        assert_eq!(normalize_claim("dir:src/"), "dir:src");
        assert_eq!(normalize_claim("resource:db"), "resource:db");
    }

    #[test]
    fn claim_conflicts() {
        assert!(claims_conflict("file:a.rs", "file:a.rs"));
        assert!(!claims_conflict("file:a.rs", "file:b.rs"));
        assert!(claims_conflict("dir:src", "file:src/x.rs"));
        assert!(claims_conflict("file:src/x.rs", "dir:src"));
        assert!(!claims_conflict("dir:src", "file:srcx/y.rs"));
        assert!(claims_conflict("dir:src", "dir:src/sub"));
        assert!(claims_conflict("resource:db", "resource:db"));
        assert!(!claims_conflict("resource:db", "resource:cache"));
    }

    #[test]
    fn plan_validation_catches_errors() {
        let good = PlanFile {
            plan_version: Some(1),
            project: "p".into(),
            request_id: None,
            tasks: vec![
                PlanTaskSpec {
                    key: "a".into(),
                    title: "A".into(),
                    spec: String::new(),
                    capabilities: vec![],
                    deps: vec![],
                    claims: vec![],
                    agent: None,
                    priority: 0,
                    timeout_secs: None,
                },
                PlanTaskSpec {
                    key: "b".into(),
                    title: "B".into(),
                    spec: String::new(),
                    capabilities: vec![],
                    deps: vec!["a".into()],
                    claims: vec![],
                    agent: None,
                    priority: 0,
                    timeout_secs: None,
                },
            ],
        };
        assert!(validate_plan(&good).is_ok());

        let mut dup = good.clone();
        dup.tasks[1].key = "a".into();
        assert_eq!(
            validate_plan(&dup),
            Err(PlanError::DuplicateKey("a".into()))
        );

        let mut unknown = good.clone();
        unknown.tasks[1].deps = vec!["nope".into()];
        assert!(matches!(
            validate_plan(&unknown),
            Err(PlanError::UnknownDep { .. })
        ));

        let mut cycle = good.clone();
        cycle.tasks[0].deps = vec!["b".into()];
        assert!(matches!(validate_plan(&cycle), Err(PlanError::Cycle(_))));
    }
}
