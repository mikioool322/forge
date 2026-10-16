//! The daemon engine: the single owner of scheduling, dispatch, supervision
//! and durable state transitions.
//!
//! Everything funnels through one task with a `select!` loop over:
//! * control messages from IPC connections (queries and commands),
//! * supervisor events (output normalization, heartbeats),
//! * supervisor outcomes (process exits),
//! * a 500 ms maintenance tick (lease heartbeats, timeouts, scheduling).
//!
//! All durable writes go through the store worker thread; the engine never
//! blocks an executor thread on SQLite.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use forge_core::model::{
    normalize_claim, now_ms, AgentInfo, AgentState, EventKind, NormalizedEvent, PlanFile,
    ProbeStatus, Project, SessionState, Severity, Task, TaskId, TaskState, WorktreeState,
};
use forge_core::scheduler::{
    self, AgentScheduleView, ScheduledAction, SchedulerInput, SchedulerTask,
};
use forge_core::{ForgeConfig, ForgePaths};
use forge_git::GitRepo;
use forge_protocol::messages::{
    DaemonInfo, ErrorCode, Notification, Response, StateSnapshot, TaskDetail, TaskView,
    PROTOCOL_VERSION,
};
use forge_runtime::adapter::{adapter_for, AgentJob, HelpFlags};
use forge_runtime::platform;
use forge_runtime::supervisor::{spawn_run, RunEvent, RunHandle, RunOutcome, RunParams};
use forge_store::repo::{self, NewEvent, PlanInsertTask};
use forge_store::Store;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

const TICK: Duration = Duration::from_millis(500);
const EVENT_FLUSH_INTERVAL: Duration = Duration::from_millis(250);
const EVENT_BATCH_MAX: usize = 200;
const NOTIFY_CAPACITY: usize = 1024;
const LOG_TAIL_MAX_BYTES: usize = 1024 * 1024;
const DIFF_MAX_BYTES: usize = 200 * 1024;
const SNAPSHOT_EVENTS: usize = 200;

// ---------------------------------------------------------------------------
// Control messages
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum CtrlReq {
    GetState,
    EventsSince {
        since_id: i64,
        limit: usize,
    },
    ListProjects,
    RegisterProject {
        root: String,
        name: Option<String>,
    },
    SubmitPlan {
        request_id: String,
        plan: PlanFile,
    },
    CancelTask {
        project: Option<String>,
        task: String,
        reason: Option<String>,
    },
    RetryTask {
        project: Option<String>,
        task: String,
    },
    GetTaskDetail {
        project: Option<String>,
        task: String,
    },
    GetTaskLog {
        project: Option<String>,
        task: String,
        lines: usize,
    },
    GetTaskDiff {
        project: Option<String>,
        task: String,
        max_bytes: usize,
    },
    Shutdown {
        drain: bool,
    },
}

pub struct Ctrl {
    pub req: CtrlReq,
    pub reply: oneshot::Sender<Response>,
}

pub struct EngineHandles {
    pub ctrl: mpsc::Sender<Ctrl>,
    pub notify: broadcast::Sender<Notification>,
    pub shutdown: watch::Sender<bool>,
    pub join: tokio::task::JoinHandle<()>,
}

pub struct EngineStartup {
    pub cfg: ForgeConfig,
    pub paths: ForgePaths,
    pub store: Arc<Store>,
    pub instance_id: String,
    pub endpoint_spec: String,
    pub started_at: i64,
}

pub fn spawn_engine(startup: EngineStartup) -> EngineHandles {
    let (ctrl_tx, ctrl_rx) = mpsc::channel::<Ctrl>(256);
    let (notify_tx, _) = broadcast::channel::<Notification>(NOTIFY_CAPACITY);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let engine_notify = notify_tx.clone();
    let engine_shutdown = shutdown_tx.clone();
    let join = tokio::spawn(async move {
        match Engine::bootstrap(startup, engine_notify, engine_shutdown).await {
            Ok(engine) => engine.run(ctrl_rx, shutdown_rx).await,
            Err(e) => tracing::error!(error = %e, "engine bootstrap failed"),
        }
    });
    EngineHandles {
        ctrl: ctrl_tx,
        notify: notify_tx,
        shutdown: shutdown_tx,
        join,
    }
}

// ---------------------------------------------------------------------------
// Agent runtime view
// ---------------------------------------------------------------------------

/// A supervised run plus bookkeeping for the "lost outcome" safety net.
struct RunEntry {
    handle: RunHandle,
    join_finished_at: Option<Instant>,
}

impl RunEntry {
    fn new(handle: RunHandle) -> Self {
        Self {
            handle,
            join_finished_at: None,
        }
    }
}

struct AgentRuntime {
    spec: forge_core::AgentSpec,
    probe: ProbeStatus,
    version: Option<String>,
    help: HelpFlags,
    probed_at: i64,
}

impl AgentRuntime {
    fn available(&self) -> bool {
        self.spec.enabled && self.probe.is_ok()
    }
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

pub struct Engine {
    cfg: ForgeConfig,
    paths: ForgePaths,
    store: Arc<Store>,
    instance_id: String,
    endpoint_spec: String,
    started_at: i64,

    projects: BTreeMap<String, Project>,
    tasks: BTreeMap<TaskId, Task>,
    deps: BTreeMap<TaskId, Vec<TaskId>>,
    agents: BTreeMap<String, AgentRuntime>,
    leases: BTreeMap<String, TaskId>,

    runs: HashMap<TaskId, RunEntry>,
    sessions: HashMap<TaskId, i64>,
    pending_cancel: HashSet<TaskId>,

    draining: Option<Instant>,
    recover_notes: usize,

    run_events_tx: mpsc::Sender<(TaskId, RunEvent)>,
    run_events_rx: mpsc::Receiver<(TaskId, RunEvent)>,
    run_outcomes_tx: mpsc::Sender<(TaskId, RunOutcome)>,
    run_outcomes_rx: mpsc::Receiver<(TaskId, RunOutcome)>,

    notify: broadcast::Sender<Notification>,
    shutdown_tx: watch::Sender<bool>,

    event_batch: Vec<NewEvent>,
    last_flush: Instant,
}

impl Engine {
    async fn bootstrap(
        startup: EngineStartup,
        notify: broadcast::Sender<Notification>,
        shutdown_tx: watch::Sender<bool>,
    ) -> anyhow::Result<Self> {
        let (run_events_tx, run_events_rx) = mpsc::channel(2048);
        let (run_outcomes_tx, run_outcomes_rx) = mpsc::channel(64);
        let mut engine = Self {
            cfg: startup.cfg,
            paths: startup.paths,
            store: startup.store,
            instance_id: startup.instance_id,
            endpoint_spec: startup.endpoint_spec,
            started_at: startup.started_at,
            projects: BTreeMap::new(),
            tasks: BTreeMap::new(),
            deps: BTreeMap::new(),
            agents: BTreeMap::new(),
            leases: BTreeMap::new(),
            runs: HashMap::new(),
            sessions: HashMap::new(),
            pending_cancel: HashSet::new(),
            draining: None,
            recover_notes: 0,
            run_events_tx,
            run_events_rx,
            run_outcomes_tx,
            run_outcomes_rx,
            notify,
            shutdown_tx,
            event_batch: Vec::new(),
            last_flush: Instant::now(),
        };
        engine.load_state().await?;
        engine.probe_agents().await;
        engine.recover().await?;
        engine.record(NewEvent {
            project_id: None,
            task_id: None,
            agent_id: None,
            kind: EventKind::DaemonNote,
            severity: Severity::Info,
            message: format!(
                "daemon started (instance {}, endpoint {}); recovery notes: {}",
                engine.instance_id, engine.endpoint_spec, engine.recover_notes
            ),
            data: None,
        });
        engine.flush_events().await;
        engine.schedule().await;
        Ok(engine)
    }

    async fn load_state(&mut self) -> anyhow::Result<()> {
        let projects = self.store.call(|conn| repo::list_projects(conn)).await?;
        for p in projects {
            self.projects.insert(p.name.clone(), p);
        }
        let project_ids: Vec<i64> = self.projects.values().map(|p| p.id).collect();
        for pid in project_ids {
            let tasks = self
                .store
                .call(move |conn| repo::list_tasks(conn, pid))
                .await?;
            let edges = self
                .store
                .call(move |conn| repo::all_dependencies(conn, pid))
                .await?;
            for t in tasks {
                self.tasks.insert(t.id, t);
            }
            for (task_id, dep_id) in edges {
                self.deps.entry(task_id).or_default().push(dep_id);
            }
        }
        for deps in self.deps.values_mut() {
            deps.sort_unstable();
        }
        let leases = self.store.call(|conn| repo::list_leases(conn)).await?;
        for l in leases {
            self.leases.insert(l.resource, l.task_id);
        }
        Ok(())
    }

    async fn probe_agents(&mut self) {
        let specs = self.cfg.agents.clone();
        let mut handles = vec![];
        for spec in specs {
            let s = spec.clone();
            handles.push(tokio::spawn(async move {
                let adapter = adapter_for(s.kind);
                let s2 = s.clone();
                let outcome = tokio::task::spawn_blocking(move || adapter.probe(&s2))
                    .await
                    .ok();
                (s, outcome)
            }));
        }
        for handle in handles {
            let Ok((spec, Some(outcome))) = handle.await else {
                continue;
            };
            if let Err(e) = self
                .store
                .call({
                    let spec = spec.clone();
                    move |conn| repo::upsert_agent(conn, &spec)
                })
                .await
            {
                tracing::warn!(agent = %spec.id, error = %e, "failed to upsert agent");
            }
            let status_label = outcome.status.label().to_string();
            let detail = outcome.status.detail().map(|s| s.to_string());
            let version = outcome.version.clone();
            if let Err(e) = self
                .store
                .call({
                    let id = spec.id.clone();
                    let status_label = status_label.clone();
                    let detail = detail.clone();
                    let version = version.clone();
                    move |conn| {
                        repo::set_agent_probe(
                            conn,
                            &id,
                            version.as_deref(),
                            &status_label,
                            detail.as_deref(),
                        )
                    }
                })
                .await
            {
                tracing::warn!(agent = %spec.id, error = %e, "failed to persist probe");
            }
            self.record(NewEvent {
                project_id: None,
                task_id: None,
                agent_id: Some(spec.id.clone()),
                kind: EventKind::AgentProbe,
                severity: if outcome.status.is_ok() {
                    Severity::Info
                } else {
                    Severity::Warn
                },
                message: format!(
                    "agent {} probe: {} {}",
                    spec.id,
                    status_label,
                    outcome
                        .version
                        .clone()
                        .or_else(|| detail.clone())
                        .unwrap_or_default()
                ),
                data: None,
            });
            self.agents.insert(
                spec.id.clone(),
                AgentRuntime {
                    spec,
                    probe: outcome.status,
                    version: outcome.version,
                    help: outcome.help,
                    probed_at: now_ms(),
                },
            );
        }
        self.flush_events().await;
    }

    /// Reconcile state after a restart: orphan processes, stale leases,
    /// unclosed sessions and worktrees of tasks that were active.
    async fn recover(&mut self) -> anyhow::Result<()> {
        let active: Vec<Task> = self
            .tasks
            .values()
            .filter(|t| t.state.is_active() || t.state == TaskState::ReviewPending)
            .cloned()
            .collect();

        // Close sessions that belong to tasks which are not active any more.
        let open_sessions = self.store.call(|conn| repo::open_sessions(conn)).await?;
        for s in &open_sessions {
            let task_active = active.iter().any(|t| t.id == s.task_id);
            if !task_active {
                let sid = s.id;
                let _ = self
                    .store
                    .call(move |conn| repo::close_session(conn, sid, SessionState::Crashed, None))
                    .await;
            }
        }

        for task in active {
            let mut note = String::new();
            // 1. orphan process?
            if let Some(session) = open_sessions.iter().rev().find(|s| s.task_id == task.id) {
                if let Some(pid) = session.pid {
                    if platform::process_alive(pid) {
                        match self.cfg.recovery.orphan_action {
                            forge_core::config::OrphanAction::Kill => {
                                let _ = platform::kill_tree(pid, true);
                                note = format!("orphan process {pid} killed");
                            }
                            forge_core::config::OrphanAction::Report => {
                                note = format!("orphan process {pid} left running (recovery.orphan_action=report)");
                            }
                        }
                    }
                }
                let sid = session.id;
                let _ = self
                    .store
                    .call(move |conn| repo::close_session(conn, sid, SessionState::Crashed, None))
                    .await;
            }
            // 2. release leases
            let tid = task.id;
            let released = self
                .store
                .call(move |conn| repo::release_leases_for_task(conn, tid))
                .await
                .unwrap_or_default();
            for resource in released {
                self.leases.remove(&resource);
            }
            // 3. worktree -> retained (never delete dirty work)
            if let Some(wt_id) = task.worktree_id {
                let _ = self
                    .store
                    .call(move |conn| {
                        repo::set_worktree_state(conn, wt_id, WorktreeState::Retained)
                    })
                    .await;
            }
            // 4. mark interrupted
            let from = task.state;
            let reason = if note.is_empty() {
                "daemon restarted while the task was active".to_string()
            } else {
                format!("daemon restarted; {note}")
            };
            let tid = task.id;
            if self
                .store
                .call(move |conn| repo::set_task_state(conn, tid, from, TaskState::Interrupted))
                .await
                .is_ok()
            {
                if let Some(t) = self.tasks.get_mut(&task.id) {
                    t.state = TaskState::Interrupted;
                    t.failure_reason = Some(reason.clone());
                    t.updated_at = now_ms();
                }
            }
            self.recover_notes += 1;
            self.record(NewEvent {
                project_id: Some(task.project_id),
                task_id: Some(task.id),
                agent_id: task.agent_id.clone(),
                kind: EventKind::Recovery,
                severity: Severity::Warn,
                message: format!("{} marked INTERRUPTED: {reason}", task.display_id()),
                data: None,
            });
        }

        // Drop leases whose tasks are gone or terminal.
        let stale: Vec<String> = self
            .leases
            .iter()
            .filter(|(_, task_id)| {
                self.tasks
                    .get(task_id)
                    .map(|t| t.state.is_terminal() || t.state == TaskState::Interrupted)
                    .unwrap_or(true)
            })
            .map(|(r, _)| r.clone())
            .collect();
        for resource in stale {
            let task_id = self.leases.remove(&resource);
            if let Some(tid) = task_id {
                let _ = self
                    .store
                    .call(move |conn| repo::release_leases_for_task(conn, tid))
                    .await;
            }
        }
        self.flush_events().await;
        Ok(())
    }

    async fn run(
        mut self,
        mut ctrl_rx: mpsc::Receiver<Ctrl>,
        mut shutdown_rx: watch::Receiver<bool>,
    ) {
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                changed = shutdown_rx.changed() => {
                    if changed.is_ok() && *shutdown_rx.borrow() {
                        break;
                    }
                }
                Some(ctrl) = ctrl_rx.recv() => {
                    self.handle_ctrl(ctrl).await;
                    self.schedule().await;
                }
                Some((task_id, ev)) = self.run_events_rx.recv() => {
                    self.handle_run_event(task_id, ev).await;
                }
                Some((task_id, outcome)) = self.run_outcomes_rx.recv() => {
                    self.handle_run_outcome(task_id, outcome).await;
                    self.schedule().await;
                }
                _ = tick.tick() => {
                    self.on_tick().await;
                }
            }
            if self.event_batch.len() >= EVENT_BATCH_MAX
                || self.last_flush.elapsed() >= EVENT_FLUSH_INTERVAL
            {
                self.flush_events().await;
            }
            // Draining: shut down once everything finished or the budget is up.
            if let Some(deadline) = self.draining {
                if self.runs.is_empty() || Instant::now() >= deadline {
                    let _ = self.shutdown_tx.send(true);
                }
            }
        }
        self.shutdown().await;
    }

    // -----------------------------------------------------------------------
    // Events & notifications
    // -----------------------------------------------------------------------

    fn record(&mut self, ev: NewEvent) {
        self.event_batch.push(ev);
    }

    async fn flush_events(&mut self) {
        if self.event_batch.is_empty() {
            self.last_flush = Instant::now();
            return;
        }
        let batch = std::mem::take(&mut self.event_batch);
        self.last_flush = Instant::now();
        match self
            .store
            .call(move |conn| {
                let tx = conn.unchecked_transaction()?;
                let mut records = Vec::with_capacity(batch.len());
                for ev in &batch {
                    records.push(repo::insert_event(&tx, ev)?);
                }
                tx.commit()?;
                Ok(records)
            })
            .await
        {
            Ok(records) => {
                for record in records {
                    let _ = self.notify.send(Notification::Event(Box::new(record)));
                }
            }
            Err(e) => tracing::warn!(error = %e, "failed to persist events"),
        }
    }

    fn broadcast_task(&self, task_id: TaskId) {
        if let Some(task) = self.tasks.get(&task_id) {
            let view = self.task_view(task);
            let _ = self.notify.send(Notification::TaskChanged(Box::new(view)));
        }
    }

    fn broadcast_agents(&self) {
        let _ = self.notify.send(Notification::AgentsChanged {
            agents: self.agent_infos(),
        });
    }

    // -----------------------------------------------------------------------
    // Views
    // -----------------------------------------------------------------------

    fn task_view(&self, task: &Task) -> TaskView {
        TaskView {
            task: task.clone(),
            deps: self.deps.get(&task.id).cloned().unwrap_or_default(),
        }
    }

    fn running_for_agent(&self, agent_id: &str) -> u32 {
        self.tasks
            .values()
            .filter(|t| t.agent_id.as_deref() == Some(agent_id) && t.state == TaskState::Running)
            .count() as u32
    }

    fn agent_infos(&self) -> Vec<AgentInfo> {
        self.agents
            .iter()
            .map(|(id, a)| {
                let running = self.running_for_agent(id);
                let state = if !a.spec.enabled {
                    AgentState::Disabled
                } else if !a.probe.is_ok() {
                    AgentState::Offline
                } else if running > 0 {
                    AgentState::Running
                } else {
                    AgentState::Idle
                };
                AgentInfo {
                    id: id.clone(),
                    kind: a.spec.kind,
                    command: a.spec.command.clone(),
                    capabilities: a.spec.capabilities.clone(),
                    slots: a.spec.slots,
                    enabled: a.spec.enabled,
                    state,
                    running,
                    version: a.version.clone(),
                    probe: a.probe.clone(),
                    probed_at: Some(a.probed_at),
                }
            })
            .collect()
    }

    async fn snapshot(&self) -> Response {
        let events = self
            .store
            .call(|conn| repo::recent_events(conn, SNAPSHOT_EVENTS, None, None))
            .await
            .unwrap_or_default();
        let tasks = self.tasks.values().map(|t| self.task_view(t)).collect();
        let info = DaemonInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol: PROTOCOL_VERSION,
            instance_id: self.instance_id.clone(),
            started_at: self.started_at,
            uptime_ms: (now_ms() - self.started_at).max(0) as u64,
            endpoint: self.endpoint_spec.clone(),
            max_concurrency: self.cfg.daemon.max_concurrency,
            running_tasks: self.runs.len(),
        };
        Response::State(Box::new(StateSnapshot {
            daemon: info,
            projects: self.projects.values().cloned().collect(),
            tasks,
            agents: self.agent_infos(),
            events,
        }))
    }

    // -----------------------------------------------------------------------
    // Control messages
    // -----------------------------------------------------------------------

    async fn handle_ctrl(&mut self, ctrl: Ctrl) {
        let Ctrl { req, reply } = ctrl;
        let response = match req {
            CtrlReq::GetState => self.snapshot().await,
            CtrlReq::EventsSince { since_id, limit } => {
                let events = self
                    .store
                    .call(move |conn| repo::recent_events(conn, limit, Some(since_id), None))
                    .await
                    .unwrap_or_default();
                Response::Events { events }
            }
            CtrlReq::ListProjects => {
                match self.store.call(|conn| repo::list_projects(conn)).await {
                    Ok(projects) => Response::Projects { projects },
                    Err(e) => Response::error(ErrorCode::Internal, e.to_string()),
                }
            }
            CtrlReq::RegisterProject { root, name } => self.register_project(root, name).await,
            CtrlReq::SubmitPlan { request_id, plan } => self.submit_plan(request_id, plan).await,
            CtrlReq::CancelTask {
                project,
                task,
                reason,
            } => self.cancel_task(project, task, reason).await,
            CtrlReq::RetryTask { project, task } => self.retry_task(project, task).await,
            CtrlReq::GetTaskDetail { project, task } => self.task_detail(project, task).await,
            CtrlReq::GetTaskLog {
                project,
                task,
                lines,
            } => self.task_log(project, task, lines).await,
            CtrlReq::GetTaskDiff {
                project,
                task,
                max_bytes,
            } => self.task_diff(project, task, max_bytes).await,
            CtrlReq::Shutdown { drain } => {
                if drain {
                    let budget = Duration::from_secs(self.cfg.daemon.shutdown_drain_secs.max(300));
                    self.draining = Some(Instant::now() + budget);
                    let note = format!(
                        "draining: {} running task(s); daemon exits when they finish or after {:?}",
                        self.runs.len(),
                        budget
                    );
                    Response::Ack { note: Some(note) }
                } else {
                    let _ = self.shutdown_tx.send(true);
                    Response::ShuttingDown
                }
            }
        };
        let _ = reply.send(response);
    }

    async fn register_project(&mut self, root: String, name: Option<String>) -> Response {
        let root_path = PathBuf::from(&root);
        if !root_path.is_dir() {
            return Response::error(ErrorCode::BadRequest, format!("{root} is not a directory"));
        }
        let repo = match GitRepo::detect(&root_path) {
            Ok(r) => r,
            Err(e) => {
                return Response::error(
                    ErrorCode::BadRequest,
                    format!("{root} is not inside a Git repository: {e}"),
                )
            }
        };
        let canonical = repo.root().to_path_buf();
        let project_name = name.unwrap_or_else(|| {
            canonical
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "project".to_string())
        });
        let default_branch = repo.current_branch().ok().flatten();
        let root_str = canonical.to_string_lossy().to_string();
        let name_clone = project_name.clone();
        let result = self
            .store
            .call(move |conn| {
                repo::register_project(conn, &name_clone, &root_str, default_branch.as_deref())
            })
            .await;
        match result {
            Ok((project, created)) => {
                self.record(NewEvent {
                    project_id: Some(project.id),
                    task_id: None,
                    agent_id: None,
                    kind: EventKind::DaemonNote,
                    severity: Severity::Info,
                    message: format!(
                        "project {:?} {} ({})",
                        project.name,
                        if created {
                            "registered"
                        } else {
                            "already registered"
                        },
                        project.root.display()
                    ),
                    data: None,
                });
                self.projects.insert(project.name.clone(), project.clone());
                Response::Project { project, created }
            }
            Err(e) => Response::error(ErrorCode::Conflict, e.to_string()),
        }
    }

    async fn submit_plan(&mut self, request_id: String, plan: PlanFile) -> Response {
        if let Err(e) = forge_core::model::validate_plan(&plan) {
            return Response::error(ErrorCode::BadRequest, format!("invalid plan: {e}"));
        }
        let Some(project) = self.projects.get(&plan.project).cloned() else {
            return Response::error(
                ErrorCode::NotFound,
                format!(
                    "project {:?} is not registered; run `forge init` in the repository root",
                    plan.project
                ),
            );
        };
        let plan_id = {
            let pid = project.id;
            let rid = request_id.clone();
            match self
                .store
                .call(move |conn| repo::create_plan(conn, pid, &rid))
                .await
            {
                Ok((id, created)) => {
                    if !created {
                        // Idempotent replay: return the existing tasks.
                        let tasks = match self
                            .store
                            .call(move |conn| repo::list_tasks_by_plan(conn, id))
                            .await
                        {
                            Ok(t) => t,
                            Err(e) => return Response::error(ErrorCode::Internal, e.to_string()),
                        };
                        // Make sure they are present in memory (e.g. plan
                        // submitted by another client before we started).
                        for t in &tasks {
                            self.tasks.entry(t.id).or_insert_with(|| t.clone());
                        }
                        let views = tasks.iter().map(|t| self.task_view(t)).collect();
                        return Response::PlanAccepted {
                            plan_id: id,
                            created: false,
                            tasks: views,
                        };
                    }
                    id
                }
                Err(e) => return Response::error(ErrorCode::Internal, e.to_string()),
            }
        };

        let inserts: Vec<PlanInsertTask> = plan
            .tasks
            .iter()
            .map(|t| PlanInsertTask {
                key: t.key.clone(),
                title: t.title.clone(),
                spec: t.spec.clone(),
                required_caps: t.capabilities.clone(),
                claims: t.claims.iter().map(|c| normalize_claim(c)).collect(),
                deps: t.deps.clone(),
                preferred_agent: t.agent.clone(),
                priority: t.priority,
                timeout_secs: t.timeout_secs,
            })
            .collect();
        let project_id = project.id;
        let created = match self
            .store
            .call(move |conn| repo::create_plan_tasks(conn, project_id, Some(plan_id), &inserts))
            .await
        {
            Ok(tasks) => tasks,
            Err(e) => return Response::error(ErrorCode::Internal, e.to_string()),
        };

        for task in &created {
            self.tasks.insert(task.id, task.clone());
            let keys: Vec<String> = plan
                .tasks
                .iter()
                .filter(|t| t.key == task.plan_key.clone().unwrap_or_default())
                .flat_map(|t| t.deps.clone())
                .collect();
            // Resolve dep keys to ids via the sibling tasks just created.
            let mut dep_ids = vec![];
            for key in keys {
                if let Some(dep) = created
                    .iter()
                    .find(|c| c.plan_key.as_deref() == Some(key.as_str()))
                {
                    dep_ids.push(dep.id);
                }
            }
            if !dep_ids.is_empty() {
                self.deps.insert(task.id, dep_ids);
            }
        }
        for task in &created {
            self.record(NewEvent {
                project_id: Some(task.project_id),
                task_id: Some(task.id),
                agent_id: None,
                kind: EventKind::TaskCreated,
                severity: Severity::Info,
                message: format!("{} created: {}", task.display_id(), task.title),
                data: Some(serde_json::json!({ "plan_key": task.plan_key })),
            });
            self.broadcast_task(task.id);
        }
        let views = created.iter().map(|t| self.task_view(t)).collect();
        Response::PlanAccepted {
            plan_id,
            created: true,
            tasks: views,
        }
    }

    fn resolve_task(&self, project: &Option<String>, reference: &str) -> Result<Task, Response> {
        let digits = reference
            .trim()
            .trim_start_matches(['T', 't'])
            .trim_start_matches('-');
        let Ok(seq) = digits.parse::<i64>() else {
            return Err(Response::error(
                ErrorCode::BadRequest,
                format!("invalid task reference {reference:?} (expected e.g. T-001)"),
            ));
        };
        let project_filter: Option<i64> = match project {
            Some(name) => match self.projects.get(name) {
                Some(p) => Some(p.id),
                None => {
                    return Err(Response::error(
                        ErrorCode::NotFound,
                        format!("project {name:?} not found"),
                    ))
                }
            },
            None => None,
        };
        let mut matches: Vec<&Task> = self
            .tasks
            .values()
            .filter(|t| t.seq_no == seq)
            .filter(|t| {
                project_filter
                    .map(|pid| pid == t.project_id)
                    .unwrap_or(true)
            })
            .collect();
        matches.sort_by_key(|t| t.id);
        match matches.len() {
            0 => Err(Response::error(
                ErrorCode::NotFound,
                format!("task {reference} not found"),
            )),
            1 => Ok(matches[0].clone()),
            _ => Err(Response::error(
                ErrorCode::BadRequest,
                format!("task reference {reference} is ambiguous across projects; pass --project"),
            )),
        }
    }

    async fn cancel_task(
        &mut self,
        project: Option<String>,
        reference: String,
        reason: Option<String>,
    ) -> Response {
        let task = match self.resolve_task(&project, &reference) {
            Ok(t) => t,
            Err(resp) => return resp,
        };
        let reason = reason.unwrap_or_else(|| "cancelled by user".into());
        if task.state.is_terminal() {
            return Response::Ack {
                note: Some(format!(
                    "{} is already {} (no-op)",
                    task.display_id(),
                    task.state
                )),
            };
        }
        if task.state.is_active() {
            self.pending_cancel.insert(task.id);
            if let Some(entry) = self.runs.get(&task.id) {
                let _ = entry.handle.kill.send(true);
            }
        }
        let display = task.display_id();
        if self.transition(task.id, TaskState::Cancelled).await {
            self.record(NewEvent {
                project_id: Some(task.project_id),
                task_id: Some(task.id),
                agent_id: task.agent_id.clone(),
                kind: EventKind::TaskStateChanged,
                severity: Severity::Warn,
                message: format!("{display} cancelled: {reason}"),
                data: None,
            });
            Response::Ack {
                note: Some(format!("{display} cancellation requested")),
            }
        } else {
            Response::error(
                ErrorCode::Conflict,
                format!("{display} could not be cancelled from state {}", task.state),
            )
        }
    }

    async fn retry_task(&mut self, project: Option<String>, reference: String) -> Response {
        let task = match self.resolve_task(&project, &reference) {
            Ok(t) => t,
            Err(resp) => return resp,
        };
        if !matches!(task.state, TaskState::Failed | TaskState::Interrupted) {
            return Response::Ack {
                note: Some(format!(
                    "{} is {}; only FAILED or INTERRUPTED tasks can be retried",
                    task.display_id(),
                    task.state
                )),
            };
        }
        let from = task.state;
        let tid = task.id;
        let ok = self
            .store
            .call(move |conn| {
                let tx = conn.unchecked_transaction()?;
                repo::set_task_state(&tx, tid, from, TaskState::Ready)?;
                repo::bump_task_attempts(&tx, tid)?;
                tx.execute(
                    "UPDATE tasks SET failure_reason = NULL, exit_code = NULL,
                     finished_at = NULL, summary = NULL, started_at = NULL WHERE id = ?1",
                    rusqlite::params![tid],
                )?;
                tx.commit()?;
                Ok(())
            })
            .await
            .is_ok();
        if !ok {
            return Response::error(ErrorCode::Conflict, "could not retry the task");
        }
        if let Some(t) = self.tasks.get_mut(&task.id) {
            t.state = TaskState::Ready;
            t.failure_reason = None;
            t.exit_code = None;
            t.finished_at = None;
            t.summary = None;
            t.started_at = None;
            t.attempts += 1;
            t.updated_at = now_ms();
        }
        self.record(NewEvent {
            project_id: Some(task.project_id),
            task_id: Some(task.id),
            agent_id: None,
            kind: EventKind::TaskStateChanged,
            severity: Severity::Info,
            message: format!(
                "{} re-queued for retry (attempt {})",
                task.display_id(),
                task.attempts + 1
            ),
            data: None,
        });
        self.broadcast_task(task.id);
        Response::Ack {
            note: Some(format!("{} re-queued", task.display_id())),
        }
    }

    async fn task_detail(&self, project: Option<String>, reference: String) -> Response {
        let task = match self.resolve_task(&project, &reference) {
            Ok(t) => t,
            Err(resp) => return resp,
        };
        let tid = task.id;
        let worktree = if let Some(wt_id) = task.worktree_id {
            self.store
                .call(move |conn| repo::get_worktree(conn, wt_id))
                .await
                .unwrap_or(None)
        } else {
            None
        };
        let artifacts = self
            .store
            .call(move |conn| repo::list_artifacts(conn, tid))
            .await
            .unwrap_or_default();
        let sessions = self
            .store
            .call(move |conn| repo::sessions_for_task(conn, tid))
            .await
            .unwrap_or_default();
        let events = self
            .store
            .call(move |conn| repo::recent_events(conn, 100, None, Some(tid)))
            .await
            .unwrap_or_default();
        Response::TaskDetail(Box::new(TaskDetail {
            deps: self.deps.get(&task.id).cloned().unwrap_or_default(),
            task,
            worktree,
            artifacts,
            sessions,
            events,
        }))
    }

    async fn task_log(&self, project: Option<String>, reference: String, lines: usize) -> Response {
        let task = match self.resolve_task(&project, &reference) {
            Ok(t) => t,
            Err(resp) => return resp,
        };
        let Some(path) = task.log_path.clone() else {
            return Response::error(
                ErrorCode::NotFound,
                format!("{} has no log file yet", task.display_id()),
            );
        };
        let lines = if lines == 0 {
            self.cfg.logs.tail_default_lines
        } else {
            lines.min(10_000)
        };
        let path_buf = PathBuf::from(&path);
        let result = tokio::task::spawn_blocking(move || {
            crate::util::read_log_tail(&path_buf, lines, LOG_TAIL_MAX_BYTES)
        })
        .await;
        match result {
            Ok(Ok((text, truncated))) => Response::TaskLog {
                text,
                truncated,
                path: Some(path),
            },
            Ok(Err(e)) => Response::error(
                ErrorCode::NotFound,
                format!("cannot read log {}: {e}", path),
            ),
            Err(e) => Response::error(ErrorCode::Internal, e.to_string()),
        }
    }

    async fn task_diff(
        &self,
        project: Option<String>,
        reference: String,
        max_bytes: usize,
    ) -> Response {
        let task = match self.resolve_task(&project, &reference) {
            Ok(t) => t,
            Err(resp) => return resp,
        };
        let Some(wt_id) = task.worktree_id else {
            return Response::error(
                ErrorCode::NotFound,
                format!("{} has no worktree", task.display_id()),
            );
        };
        let worktree = match self
            .store
            .call(move |conn| repo::get_worktree(conn, wt_id))
            .await
        {
            Ok(Some(w)) => w,
            Ok(None) => return Response::error(ErrorCode::NotFound, "worktree row missing"),
            Err(e) => return Response::error(ErrorCode::Internal, e.to_string()),
        };
        let wt_path = worktree.path.clone();
        let max_bytes = if max_bytes == 0 {
            DIFF_MAX_BYTES
        } else {
            max_bytes.min(4 * 1024 * 1024)
        };
        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let repo = GitRepo::detect(&wt_path).map_err(|e| e.to_string())?;
            repo.mark_intent_to_add(&wt_path)
                .map_err(|e| e.to_string())?;
            let stat = repo.diff_stat(&wt_path).map_err(|e| e.to_string())?;
            let (diff, truncated) = repo.diff(&wt_path, max_bytes).map_err(|e| e.to_string())?;
            let mut text = String::new();
            text.push_str(&format!(
                "# branch {} @ {}\n# worktree {}\n\n",
                worktree.branch,
                worktree.base_commit.as_deref().unwrap_or("?"),
                wt_path.display()
            ));
            text.push_str(&stat);
            if !stat.ends_with('\n') {
                text.push('\n');
            }
            text.push('\n');
            text.push_str(&diff);
            if truncated {
                text.push_str("\n[diff truncated]\n");
            }
            Ok(text)
        })
        .await;
        match result {
            Ok(Ok(text)) => Response::TaskDiff {
                truncated: text.contains("[diff truncated]"),
                text,
            },
            Ok(Err(e)) => Response::error(ErrorCode::Internal, e),
            Err(e) => Response::error(ErrorCode::Internal, e.to_string()),
        }
    }

    // -----------------------------------------------------------------------
    // Scheduling & dispatch
    // -----------------------------------------------------------------------

    fn scheduler_input(&self) -> SchedulerInput {
        let mut tasks = Vec::with_capacity(self.tasks.len());
        for t in self.tasks.values() {
            let dep_states = self
                .deps
                .get(&t.id)
                .map(|deps| {
                    deps.iter()
                        .filter_map(|dep| self.tasks.get(dep).map(|dt| (*dep, dt.state)))
                        .collect()
                })
                .unwrap_or_default();
            tasks.push(SchedulerTask {
                id: t.id,
                seq_no: t.seq_no,
                priority: t.priority,
                state: t.state,
                required_caps: t.required_caps.clone(),
                preferred_agent: t.preferred_agent.clone(),
                claims: t.claims.clone(),
                dep_states,
            });
        }
        let agents = self
            .agents
            .iter()
            .map(|(id, a)| AgentScheduleView {
                id: id.clone(),
                capabilities: a.spec.capabilities.clone(),
                slots: a.spec.slots,
                running: self.running_for_agent(id),
                available: a.available(),
            })
            .collect();
        let running_claims = self
            .leases
            .iter()
            .map(|(resource, task_id)| (*task_id, resource.clone()))
            .collect();
        let running_count = self.tasks.values().filter(|t| t.state.is_active()).count();
        SchedulerInput {
            tasks,
            agents,
            running_count,
            running_claims,
            max_concurrency: self.cfg.daemon.max_concurrency as usize,
        }
    }

    async fn schedule(&mut self) {
        if self.draining.is_some() {
            return; // stop dispatching while draining
        }
        let decisions = scheduler::schedule(&self.scheduler_input());
        for decision in decisions {
            match decision {
                ScheduledAction::Promote { task } => {
                    self.transition(task, TaskState::Ready).await;
                }
                ScheduledAction::Block { task, reason } => {
                    if self.transition(task, TaskState::Blocked).await {
                        let display = self
                            .tasks
                            .get(&task)
                            .map(|t| t.display_id())
                            .unwrap_or_default();
                        self.record(NewEvent {
                            project_id: self.tasks.get(&task).map(|t| t.project_id),
                            task_id: Some(task),
                            agent_id: None,
                            kind: EventKind::TaskStateChanged,
                            severity: Severity::Warn,
                            message: format!("{display} blocked: {reason}"),
                            data: None,
                        });
                    }
                }
                ScheduledAction::Dispatch { task, agent } => {
                    self.start_dispatch(task, agent).await;
                }
            }
        }
        self.flush_events().await;
        self.broadcast_agents();
    }

    async fn transition(&mut self, task_id: TaskId, to: TaskState) -> bool {
        let Some(task) = self.tasks.get(&task_id).cloned() else {
            return false;
        };
        let from = task.state;
        if from == to {
            return true;
        }
        if !forge_core::state::is_legal_transition(from, to) {
            tracing::warn!(task = task_id, %from, %to, "illegal transition ignored");
            return false;
        }
        match self
            .store
            .call(move |conn| repo::set_task_state(conn, task_id, from, to))
            .await
        {
            Ok(()) => {
                if let Some(t) = self.tasks.get_mut(&task_id) {
                    t.state = to;
                    t.updated_at = now_ms();
                }
                self.broadcast_task(task_id);
                true
            }
            Err(e) => {
                tracing::warn!(task = task_id, error = %e, "state transition failed");
                false
            }
        }
    }

    async fn start_dispatch(&mut self, task_id: TaskId, agent_id: String) {
        let Some(task) = self.tasks.get(&task_id).cloned() else {
            return;
        };
        if task.state != TaskState::Ready {
            return;
        }
        let Some(agent) = self.agents.get(&agent_id) else {
            return;
        };
        if !agent.available() {
            return;
        }
        let agent_spec = agent.spec.clone();
        let help = agent.help.clone();
        let Some(project) = self
            .projects
            .values()
            .find(|p| p.id == task.project_id)
            .cloned()
        else {
            self.fail_task(task_id, "project is no longer registered", None)
                .await;
            return;
        };

        // READY -> LEASED
        if !self.transition(task_id, TaskState::Leased).await {
            return;
        }
        let lease_ttl_ms = (self.cfg.daemon.lease_timeout_secs * 1000) as i64;

        // Acquire declared resource claims.
        for claim in &task.claims {
            let acquired = self
                .store
                .call({
                    let claim = claim.clone();
                    move |conn| repo::acquire_lease(conn, &claim, task_id, None, lease_ttl_ms)
                })
                .await
                .unwrap_or(false);
            if !acquired {
                // Lost the race: revert and try later.
                self.transition(task_id, TaskState::Ready).await;
                return;
            }
            self.leases.insert(claim.clone(), task_id);
            self.record(NewEvent {
                project_id: Some(project.id),
                task_id: Some(task_id),
                agent_id: Some(agent_id.clone()),
                kind: EventKind::LeaseAcquired,
                severity: Severity::Info,
                message: format!("{} acquired resource {claim}", task.display_id()),
                data: None,
            });
        }

        let attempt = task.attempts.max(0);
        let task_slug = task.display_id().to_lowercase();
        let dir_name = format!("{task_slug}-r{attempt}");
        let branch = format!(
            "{}{}/{}-r{}",
            self.cfg.git.branch_prefix,
            forge_git::sanitize_ref_component(&project.name),
            task_slug,
            attempt
        );
        let worktree_dir = self
            .paths
            .project_worktrees_dir(&project.name)
            .join(&dir_name);
        let log_dir = self.paths.project_logs_dir(&project.name);
        let log_path = log_dir.join(format!("{dir_name}.log"));

        // Create the worktree (blocking git work).
        let create = {
            let root = project.root.clone();
            let dir = worktree_dir.clone();
            let branch = branch.clone();
            tokio::task::spawn_blocking(move || -> Result<String, String> {
                let repo = GitRepo::detect(&root).map_err(|e| e.to_string())?;
                let base = repo.head_commit().map_err(|e| e.to_string())?;
                let branch = if repo.branch_exists(&branch).map_err(|e| e.to_string())? {
                    format!("{branch}-{}", forge_core::model::now_ms())
                } else {
                    branch
                };
                repo.worktree_add(&dir, &branch, &base)
                    .map_err(|e| e.to_string())?;
                let canonical = dir
                    .canonicalize()
                    .map_err(|e| format!("cannot canonicalize worktree {}: {e}", dir.display()))?;
                Ok(format!("{}\u{1}{}", base, canonical.display()))
            })
            .await
        };
        let (base_commit, worktree_path, branch) = match create {
            Ok(Ok(payload)) => {
                let mut it = payload.split('\u{1}');
                let base = it.next().unwrap_or_default().to_string();
                let path = PathBuf::from(it.next().unwrap_or_default());
                (base, path, branch)
            }
            Ok(Err(e)) => {
                self.fail_task(
                    task_id,
                    &format!("worktree creation failed: {e}"),
                    Some(agent_id),
                )
                .await;
                return;
            }
            Err(e) => {
                self.fail_task(
                    task_id,
                    &format!("worktree task panicked: {e}"),
                    Some(agent_id),
                )
                .await;
                return;
            }
        };

        // Guard: a cancel may have arrived while git ran.
        if let Some(t) = self.tasks.get(&task_id) {
            if t.state != TaskState::Leased {
                // Keep the worktree for inspection but do not run.
                let _ = self
                    .store
                    .call({
                        let path = worktree_path.clone();
                        let project_id = project.id;
                        let branch = branch.clone();
                        let base = base_commit.clone();
                        move |conn| {
                            repo::insert_worktree(
                                conn,
                                project_id,
                                task_id,
                                &path,
                                &branch,
                                Some(&base),
                                WorktreeState::Retained,
                            )
                        }
                    })
                    .await;
                return;
            }
        }

        // Persist worktree row.
        let worktree_row = match self
            .store
            .call({
                let path = worktree_path.clone();
                let branch = branch.clone();
                let base = base_commit.clone();
                let project_id = project.id;
                move |conn| {
                    repo::insert_worktree(
                        conn,
                        project_id,
                        task_id,
                        &path,
                        &branch,
                        Some(&base),
                        WorktreeState::Active,
                    )
                }
            })
            .await
        {
            Ok(w) => w,
            Err(e) => {
                self.fail_task(
                    task_id,
                    &format!("cannot persist worktree row: {e}"),
                    Some(agent_id),
                )
                .await;
                return;
            }
        };

        // Assign + LEASED -> RUNNING + session.
        let log_path_str = log_path.to_string_lossy().to_string();
        let assigned = self
            .store
            .call({
                let agent_id = agent_id.clone();
                let log_path_str = log_path_str.clone();
                let base = base_commit.clone();
                move |conn| {
                    repo::assign_task(
                        conn,
                        task_id,
                        &agent_id,
                        Some(worktree_row.id),
                        Some(&base),
                        Some(&log_path_str),
                    )
                }
            })
            .await;
        if let Err(e) = assigned {
            self.fail_task(task_id, &format!("cannot assign task: {e}"), Some(agent_id))
                .await;
            return;
        }
        if let Some(t) = self.tasks.get_mut(&task_id) {
            t.agent_id = Some(agent_id.clone());
            t.worktree_id = Some(worktree_row.id);
            t.base_commit = Some(base_commit.clone());
            t.log_path = Some(log_path_str.clone());
        }
        if !self.transition(task_id, TaskState::Running).await {
            self.fail_task(task_id, "could not enter RUNNING state", Some(agent_id))
                .await;
            return;
        }
        let session_id = self
            .store
            .call({
                let agent_id = agent_id.clone();
                move |conn| repo::open_session(conn, &agent_id, task_id, Some(worktree_row.id))
            })
            .await
            .unwrap_or(0);
        self.sessions.insert(task_id, session_id);
        let _ = self
            .store
            .call(move |conn| repo::mark_task_started(conn, task_id, now_ms()))
            .await;
        if let Some(t) = self.tasks.get_mut(&task_id) {
            t.started_at = Some(now_ms());
        }

        self.record(NewEvent {
            project_id: Some(project.id),
            task_id: Some(task_id),
            agent_id: Some(agent_id.clone()),
            kind: EventKind::TaskDispatched,
            severity: Severity::Info,
            message: format!(
                "{} dispatched to {} (worktree {}, branch {}, base {})",
                task.display_id(),
                agent_id,
                worktree_path.display(),
                branch,
                &base_commit[..base_commit.len().min(8)]
            ),
            data: None,
        });
        self.record(NewEvent {
            project_id: Some(project.id),
            task_id: Some(task_id),
            agent_id: Some(agent_id.clone()),
            kind: EventKind::WorktreeCreated,
            severity: Severity::Info,
            message: format!("worktree {} on {}", worktree_path.display(), branch),
            data: None,
        });
        self.broadcast_task(task_id);

        // Spawn the supervisor.
        let timeout = Duration::from_secs(
            task.timeout_secs
                .unwrap_or(self.cfg.daemon.task_timeout_secs),
        );
        let job = AgentJob {
            task_display_id: task.display_id(),
            task_title: task.title.clone(),
            spec: build_prompt(&task, &project, &worktree_path, &branch),
            worktree: worktree_path.clone(),
            extra_context: Default::default(),
            timeout,
        };
        let params = RunParams {
            adapter: adapter_for(agent_spec.kind),
            spec: agent_spec,
            job,
            help,
            log_path: log_path.clone(),
            max_log_bytes: self.cfg.logs.max_file_bytes,
            max_line_bytes: self.cfg.logs.max_line_bytes,
            heartbeat_interval: Duration::from_secs(self.cfg.daemon.heartbeat_interval_secs.max(1)),
        };
        let (outcome_tx, outcome_rx) = oneshot::channel();
        let (ev_tx, mut ev_rx) = mpsc::channel::<RunEvent>(1024);
        let event_forward = self.run_events_tx.clone();
        tokio::spawn(async move {
            while let Some(event) = ev_rx.recv().await {
                if event_forward.send((task_id, event)).await.is_err() {
                    break;
                }
            }
        });
        match spawn_run(params, ev_tx, outcome_tx) {
            Ok(handle) => {
                self.runs.insert(task_id, RunEntry::new(handle));
                let forward_tx = self.run_outcomes_tx.clone();
                tokio::spawn(async move {
                    if let Ok(outcome) = outcome_rx.await {
                        let _ = forward_tx.send((task_id, outcome)).await;
                    }
                });
            }
            Err(e) => {
                self.fail_task(
                    task_id,
                    &format!("failed to start agent process: {e}"),
                    Some(agent_id),
                )
                .await;
            }
        }
    }

    async fn fail_task(&mut self, task_id: TaskId, reason: &str, agent_id: Option<String>) {
        let Some(task) = self.tasks.get(&task_id).cloned() else {
            return;
        };
        // Release leases held by this task.
        let released = self
            .store
            .call(move |conn| repo::release_leases_for_task(conn, task_id))
            .await
            .unwrap_or_default();
        for resource in released {
            self.leases.remove(&resource);
        }
        self.sessions.remove(&task_id);
        let _ = self
            .store
            .call({
                let reason = reason.to_string();
                move |conn| {
                    let tx = conn.unchecked_transaction()?;
                    repo::mark_task_finished(&tx, task_id, now_ms(), None, Some(&reason), None)?;
                    tx.commit()?;
                    Ok(())
                }
            })
            .await;
        let from = task.state;
        if from != TaskState::Failed {
            let _ = self
                .store
                .call(move |conn| repo::set_task_state(conn, task_id, from, TaskState::Failed))
                .await;
        }
        if let Some(t) = self.tasks.get_mut(&task_id) {
            t.state = TaskState::Failed;
            t.failure_reason = Some(reason.to_string());
            t.finished_at = Some(now_ms());
            t.updated_at = now_ms();
        }
        self.record(NewEvent {
            project_id: Some(task.project_id),
            task_id: Some(task_id),
            agent_id,
            kind: EventKind::TaskStateChanged,
            severity: Severity::Error,
            message: format!("{} FAILED: {reason}", task.display_id()),
            data: None,
        });
        self.broadcast_task(task_id);
    }

    // -----------------------------------------------------------------------
    // Run events / outcomes
    // -----------------------------------------------------------------------

    async fn handle_run_event(&mut self, task_id: TaskId, event: RunEvent) {
        match event {
            RunEvent::Started { pid } => {
                if let Some(session_id) = self.sessions.get(&task_id).copied() {
                    let _ = self
                        .store
                        .call(move |conn| repo::set_session_running(conn, session_id, pid))
                        .await;
                }
                let task = self.tasks.get(&task_id);
                self.record(NewEvent {
                    project_id: task.map(|t| t.project_id),
                    task_id: Some(task_id),
                    agent_id: task.and_then(|t| t.agent_id.clone()),
                    kind: EventKind::ProcessStarted,
                    severity: Severity::Info,
                    message: format!(
                        "{} agent process started (pid {pid})",
                        task.map(|t| t.display_id()).unwrap_or_default()
                    ),
                    data: None,
                });
            }
            RunEvent::Normalized(ev) => {
                self.persist_normalized(task_id, *ev);
            }
            RunEvent::Heartbeat => {
                // Heartbeats are cheap; they renew leases via the tick loop.
            }
            RunEvent::Dropped { count } => {
                let task = self.tasks.get(&task_id);
                self.record(NewEvent {
                    project_id: task.map(|t| t.project_id),
                    task_id: Some(task_id),
                    agent_id: task.and_then(|t| t.agent_id.clone()),
                    kind: EventKind::LogDropped,
                    severity: Severity::Warn,
                    message: format!("{count} output lines were dropped from the live stream (kept in the log file)"),
                    data: None,
                });
            }
        }
    }

    /// Persist only signal-bearing output events; the raw stream lives in
    /// the size-limited log file (keeps the events table bounded).
    fn persist_normalized(&mut self, task_id: TaskId, ev: NormalizedEvent) {
        let interesting = ev.severity != Severity::Info || ev.kind != EventKind::OutputEvent;
        if !interesting {
            return;
        }
        let task = self.tasks.get(&task_id);
        self.record(NewEvent {
            project_id: task.map(|t| t.project_id),
            task_id: Some(task_id),
            agent_id: task.and_then(|t| t.agent_id.clone()),
            kind: ev.kind,
            severity: ev.severity,
            message: ev.message,
            data: ev.data,
        });
    }

    async fn handle_run_outcome(&mut self, task_id: TaskId, outcome: RunOutcome) {
        self.runs.remove(&task_id);
        let was_cancelled = self.pending_cancel.remove(&task_id);
        let Some(task) = self.tasks.get(&task_id).cloned() else {
            return;
        };
        let agent_id = task.agent_id.clone();

        // Close the session.
        if let Some(session_id) = self.sessions.remove(&task_id) {
            let session_state = match outcome.classification {
                forge_runtime::ExitClass::Success => SessionState::Exited,
                forge_runtime::ExitClass::Killed => SessionState::Killed,
                forge_runtime::ExitClass::AuthRequired => SessionState::Exited,
                forge_runtime::ExitClass::Timeout => SessionState::Killed,
                forge_runtime::ExitClass::Failed => {
                    if outcome.exit_code.is_none() {
                        SessionState::Crashed
                    } else {
                        SessionState::Exited
                    }
                }
            };
            let exit_code = outcome.exit_code;
            let _ = self
                .store
                .call(move |conn| repo::close_session(conn, session_id, session_state, exit_code))
                .await;
        }

        // Capture artifacts from the worktree.
        let mut diff_stat = String::new();
        let mut files = 0usize;
        let mut added = 0u64;
        let mut deleted = 0u64;
        let mut file_list: Vec<String> = vec![];
        if let Some(wt_id) = task.worktree_id {
            if let Ok(Some(wt)) = self
                .store
                .call(move |conn| repo::get_worktree(conn, wt_id))
                .await
            {
                let wt_path = wt.path.clone();
                let result = tokio::task::spawn_blocking(
                    move || -> Result<(String, usize, u64, u64, Vec<String>), String> {
                        let repo = GitRepo::detect(&wt_path).map_err(|e| e.to_string())?;
                        repo.mark_intent_to_add(&wt_path)
                            .map_err(|e| e.to_string())?;
                        let stat = repo.diff_stat(&wt_path).map_err(|e| e.to_string())?;
                        let (added, deleted) =
                            repo.diff_numstat(&wt_path).map_err(|e| e.to_string())?;
                        let files = repo.changed_files(&wt_path).map_err(|e| e.to_string())?;
                        Ok((stat, files.len(), added, deleted, files))
                    },
                )
                .await;
                if let Ok(Ok((stat, count, a, d, list))) = result {
                    diff_stat = stat;
                    files = count;
                    added = a;
                    deleted = d;
                    file_list = list;
                }
                // Keep the worktree for inspection (never delete work).
                let _ = self
                    .store
                    .call(move |conn| {
                        repo::set_worktree_state(conn, wt_id, WorktreeState::Retained)
                    })
                    .await;
                let _ = self
                    .store
                    .call({
                        let path = wt.path.to_string_lossy().to_string();
                        move |conn| {
                            repo::insert_artifact(conn, task_id, "worktree", Some(&path), None)
                                .map(|_| ())
                        }
                    })
                    .await;
            }
        }

        // Persist execution-contract artifacts.
        let exit_code = outcome.exit_code;
        let duration = outcome.duration_ms;
        let log_path = outcome.log_path.to_string_lossy().to_string();
        let log_bytes = outcome.log_bytes.to_string();
        let log_bytes_display = outcome.log_bytes;
        let command = outcome.command_display.clone();
        let stat_for_db: String = diff_stat.chars().take(4000).collect();
        let summary_json = serde_json::json!({
            "files": files,
            "added": added,
            "deleted": deleted,
            "paths": file_list.iter().take(50).collect::<Vec<_>>(),
            "dropped_lines": outcome.dropped_lines,
        })
        .to_string();
        let _ = self
            .store
            .call(move |conn| {
                let tx = conn.unchecked_transaction()?;
                repo::insert_artifact(&tx, task_id, "git_diff_stat", None, Some(&stat_for_db))?;
                repo::insert_artifact(&tx, task_id, "git_diff_summary", None, Some(&summary_json))?;
                repo::insert_artifact(
                    &tx,
                    task_id,
                    "exit_code",
                    None,
                    Some(&format!("{exit_code:?}")),
                )?;
                repo::insert_artifact(
                    &tx,
                    task_id,
                    "duration_ms",
                    None,
                    Some(&duration.to_string()),
                )?;
                repo::insert_artifact(&tx, task_id, "log_file", Some(&log_path), Some(&log_bytes))?;
                repo::insert_artifact(&tx, task_id, "command", None, Some(&command))?;
                tx.commit()?;
                Ok(())
            })
            .await;
        self.record(NewEvent {
            project_id: Some(task.project_id),
            task_id: Some(task_id),
            agent_id: agent_id.clone(),
            kind: EventKind::ArtifactRecorded,
            severity: Severity::Info,
            message: format!(
                "{} artifacts: files={files} +{added} -{deleted} log={log_bytes_display}B",
                task.display_id()
            ),
            data: None,
        });

        // Release leases.
        let released = self
            .store
            .call(move |conn| repo::release_leases_for_task(conn, task_id))
            .await
            .unwrap_or_default();
        for resource in released {
            self.leases.remove(&resource);
            self.record(NewEvent {
                project_id: Some(task.project_id),
                task_id: Some(task_id),
                agent_id: agent_id.clone(),
                kind: EventKind::LeaseReleased,
                severity: Severity::Info,
                message: format!("released resource {resource}"),
                data: None,
            });
        }

        let summary = format!(
            "exit={exit_code:?} class={} files={files} +{added} -{deleted} dropped={}",
            outcome.classification.as_str(),
            outcome.dropped_lines
        );
        let finished_at = now_ms();
        let reason_line = outcome
            .stderr_tail
            .last()
            .cloned()
            .or_else(|| outcome.error.clone());

        if was_cancelled || task.state == TaskState::Cancelled {
            let _ = self
                .store
                .call({
                    let summary = summary.clone();
                    move |conn| {
                        repo::mark_task_finished(
                            conn,
                            task_id,
                            finished_at,
                            exit_code,
                            Some("cancelled"),
                            Some(&summary),
                        )
                    }
                })
                .await;
            if let Some(t) = self.tasks.get_mut(&task_id) {
                t.finished_at = Some(finished_at);
                t.exit_code = exit_code;
                t.summary = Some(summary);
                t.failure_reason = Some("cancelled".into());
                t.updated_at = finished_at;
            }
            self.broadcast_task(task_id);
            self.flush_events().await;
            return;
        }

        match outcome.classification {
            forge_runtime::ExitClass::Success => {
                self.transition(task_id, TaskState::ReviewPending).await;
                let _ = self
                    .store
                    .call({
                        let summary = summary.clone();
                        move |conn| {
                            repo::mark_task_finished(
                                conn,
                                task_id,
                                finished_at,
                                exit_code,
                                None,
                                Some(&summary),
                            )
                        }
                    })
                    .await;
                if !self.transition(task_id, TaskState::Completed).await {
                    // If REVIEW_PENDING -> COMPLETED could not be applied the
                    // task stays visible for inspection; surface loudly.
                    self.record(NewEvent {
                        project_id: Some(task.project_id),
                        task_id: Some(task_id),
                        agent_id: agent_id.clone(),
                        kind: EventKind::DaemonNote,
                        severity: Severity::Warn,
                        message: format!(
                            "{} finished successfully but the completion transition failed",
                            task.display_id()
                        ),
                        data: None,
                    });
                }
                let severity = if files == 0 {
                    Severity::Warn
                } else {
                    Severity::Info
                };
                self.record(NewEvent {
                    project_id: Some(task.project_id),
                    task_id: Some(task_id),
                    agent_id: agent_id.clone(),
                    kind: EventKind::ProcessExited,
                    severity,
                    message: if files == 0 {
                        format!(
                            "{} completed with an EMPTY diff (exit 0) - verify the worktree {}",
                            task.display_id(),
                            task.worktree_id.map(|_| "(see detail)").unwrap_or("-")
                        )
                    } else {
                        format!("{} completed: {summary}", task.display_id())
                    },
                    data: None,
                });
                if let Some(t) = self.tasks.get_mut(&task_id) {
                    t.finished_at = Some(finished_at);
                    t.exit_code = exit_code;
                    t.summary = Some(summary);
                    t.updated_at = finished_at;
                }
            }
            class => {
                let (reason, kind) = match class {
                    forge_runtime::ExitClass::AuthRequired => (
                        format!(
                            "authentication required (exit {exit_code:?}): {}",
                            reason_line.clone().unwrap_or_default()
                        ),
                        EventKind::AuthRequired,
                    ),
                    forge_runtime::ExitClass::Timeout => (
                        format!("timeout after {} ms", outcome.duration_ms),
                        EventKind::ProcessExited,
                    ),
                    forge_runtime::ExitClass::Killed => (
                        format!(
                            "process killed (exit {exit_code:?}){}",
                            reason_line
                                .clone()
                                .map(|l| format!(": {l}"))
                                .unwrap_or_default()
                        ),
                        EventKind::ProcessExited,
                    ),
                    _ => (
                        format!(
                            "process failed (exit {exit_code:?}){}",
                            reason_line
                                .clone()
                                .map(|l| format!(": {l}"))
                                .unwrap_or_default()
                        ),
                        EventKind::ProcessExited,
                    ),
                };
                let _ = self
                    .store
                    .call({
                        let reason = reason.clone();
                        let summary = summary.clone();
                        move |conn| {
                            repo::mark_task_finished(
                                conn,
                                task_id,
                                finished_at,
                                exit_code,
                                Some(&reason),
                                Some(&summary),
                            )
                        }
                    })
                    .await;
                if let Some(t) = self.tasks.get_mut(&task_id) {
                    t.failure_reason = Some(reason.clone());
                    t.finished_at = Some(finished_at);
                    t.exit_code = exit_code;
                    t.summary = Some(summary);
                    t.updated_at = finished_at;
                }
                if self.transition(task_id, TaskState::Failed).await {
                    self.record(NewEvent {
                        project_id: Some(task.project_id),
                        task_id: Some(task_id),
                        agent_id: agent_id.clone(),
                        kind,
                        severity: Severity::Error,
                        message: format!("{} failed: {reason}", task.display_id()),
                        data: None,
                    });
                }
            }
        }
        self.broadcast_task(task_id);
        self.flush_events().await;
        self.broadcast_agents();
    }

    // -----------------------------------------------------------------------
    // Tick & shutdown
    // -----------------------------------------------------------------------

    async fn on_tick(&mut self) {
        // Lease heartbeat for running tasks.
        let running: Vec<TaskId> = self.runs.keys().copied().collect();
        if !running.is_empty() {
            let ttl = (self.cfg.daemon.lease_timeout_secs * 1000) as i64;
            let _ = self
                .store
                .call(move |conn| {
                    for id in &running {
                        repo::heartbeat_leases_for_task(conn, *id, ttl)?;
                    }
                    Ok(())
                })
                .await;
        }
        // Lease hygiene.
        if let Ok(expired) = self.store.call(|conn| repo::expire_leases(conn)).await {
            for (resource, task_id) in expired {
                if self.leases.get(&resource) == Some(&task_id) {
                    self.leases.remove(&resource);
                    self.record(NewEvent {
                        project_id: None,
                        task_id: Some(task_id),
                        agent_id: None,
                        kind: EventKind::LeaseReleased,
                        severity: Severity::Warn,
                        message: format!("lease {resource} expired"),
                        data: None,
                    });
                }
            }
        }
        // Engine-level timeout safety net (the supervisor enforces the real
        // deadline; this catches pathological cases such as a panicked
        // supervisor task).
        let now = now_ms();
        let mut to_kill: Vec<(TaskId, u32)> = vec![];
        let mut lost: Vec<TaskId> = vec![];
        for (task_id, entry) in &self.runs {
            if let Some(task) = self.tasks.get(task_id) {
                let timeout_ms = ((task
                    .timeout_secs
                    .unwrap_or(self.cfg.daemon.task_timeout_secs)
                    as i64)
                    + 30)
                    * 1000;
                if let Some(started) = task.started_at {
                    if now - started > timeout_ms {
                        let pid = entry.handle.pid.load(std::sync::atomic::Ordering::SeqCst);
                        to_kill.push((*task_id, pid));
                    }
                }
            }
        }
        // A supervisor task that finished without its outcome being processed
        // (panic) is treated as lost after a grace period.
        for (task_id, entry) in self.runs.iter_mut() {
            if entry.handle.join.is_finished() {
                let since = *entry.join_finished_at.get_or_insert_with(Instant::now);
                if since.elapsed() > Duration::from_secs(5) {
                    lost.push(*task_id);
                }
            }
        }
        for (task_id, pid) in to_kill {
            tracing::warn!(task = task_id, pid, "safety-net timeout: killing process");
            if pid != 0 {
                let _ = platform::kill_tree(pid, true);
            }
        }
        for task_id in lost {
            if self.runs.remove(&task_id).is_some() {
                tracing::error!(task = task_id, "supervisor finished without an outcome");
                self.fail_task(
                    task_id,
                    "supervisor exited without reporting a result",
                    None,
                )
                .await;
            }
        }
        self.schedule().await;
    }

    async fn shutdown(&mut self) {
        let mut remaining: HashSet<TaskId> = self.runs.keys().copied().collect();
        for (_, entry) in self.runs.drain() {
            let _ = entry.handle.kill.send(true);
        }
        // Give supervisors a moment to report outcomes.
        let deadline = Instant::now() + Duration::from_secs(6);
        while !remaining.is_empty() && Instant::now() < deadline {
            match tokio::time::timeout_at(deadline.into(), self.run_outcomes_rx.recv()).await {
                Ok(Some((task_id, outcome))) => {
                    remaining.remove(&task_id);
                    self.handle_run_outcome(task_id, outcome).await;
                }
                _ => break,
            }
        }
        // Force-mark anything still active.
        let orphan_ids: Vec<TaskId> = self
            .tasks
            .values()
            .filter(|t| t.state.is_active())
            .map(|t| t.id)
            .collect();
        for task_id in orphan_ids {
            let task = self.tasks.get(&task_id).cloned();
            if let Some(task) = task {
                if let Some(pid) = self
                    .store
                    .call(move |conn| {
                        Ok(repo::sessions_for_task(conn, task_id)?
                            .into_iter()
                            .rev()
                            .find_map(|s| s.pid))
                    })
                    .await
                    .unwrap_or(None)
                {
                    let _ = platform::kill_tree(pid, true);
                }
                let reason = "daemon shutdown while the task was running";
                let _ = self
                    .store
                    .call({
                        let reason = reason.to_string();
                        move |conn| {
                            let tx = conn.unchecked_transaction()?;
                            repo::mark_task_finished(
                                &tx,
                                task_id,
                                now_ms(),
                                None,
                                Some(&reason),
                                None,
                            )?;
                            tx.commit()?;
                            Ok(())
                        }
                    })
                    .await;
                let from = task.state;
                let _ = self
                    .store
                    .call(move |conn| {
                        repo::set_task_state(conn, task_id, from, TaskState::Interrupted)
                    })
                    .await;
                if let Some(t) = self.tasks.get_mut(&task_id) {
                    t.state = TaskState::Interrupted;
                    t.failure_reason = Some(reason.into());
                    t.finished_at = Some(now_ms());
                }
                self.record(NewEvent {
                    project_id: Some(task.project_id),
                    task_id: Some(task_id),
                    agent_id: task.agent_id.clone(),
                    kind: EventKind::Recovery,
                    severity: Severity::Warn,
                    message: format!("{} marked INTERRUPTED: {reason}", task.display_id()),
                    data: None,
                });
            }
        }
        // Close leftover sessions and leases.
        if let Ok(sessions) = self.store.call(|conn| repo::open_sessions(conn)).await {
            for s in sessions {
                let sid = s.id;
                let _ = self
                    .store
                    .call(move |conn| repo::close_session(conn, sid, SessionState::Killed, None))
                    .await;
            }
        }
        let leases = self
            .store
            .call(|conn| repo::list_leases(conn))
            .await
            .unwrap_or_default();
        for lease in leases {
            let tid = lease.task_id;
            let _ = self
                .store
                .call(move |conn| repo::release_leases_for_task(conn, tid))
                .await;
        }
        self.record(NewEvent {
            project_id: None,
            task_id: None,
            agent_id: None,
            kind: EventKind::DaemonNote,
            severity: Severity::Info,
            message: "daemon stopping".to_string(),
            data: None,
        });
        self.flush_events().await;
        tracing::info!("engine stopped");
    }
}

fn build_prompt(
    task: &Task,
    project: &Project,
    worktree: &std::path::Path,
    branch: &str,
) -> String {
    let spec = if task.spec.trim().is_empty() {
        task.title.as_str()
    } else {
        task.spec.as_str()
    };
    format!(
        "Forge task {}: {}\n\n{}\n\n---\nProject: {}\nGit worktree: {}\nBranch: {}\nYour working directory is the worktree root. Implement the task by editing files in the working tree. Do not commit, do not push, do not touch files outside the worktree.",
        task.display_id(),
        task.title,
        spec,
        project.name,
        worktree.display(),
        branch
    )
}
