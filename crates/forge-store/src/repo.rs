//! Typed repositories. Every function takes an explicit `&mut Connection`
//! so that it can run inside the dedicated database worker thread and be
//! composed in transactions by the caller.

use std::path::{Path, PathBuf};

use forge_core::model::{
    now_ms, AgentKind, AgentSession, EventKind, EventRecord, Project, ResourceLease, SessionState,
    Severity, Task, TaskArtifact, TaskId, TaskState, Worktree, WorktreeState,
};
use forge_core::AgentSpec;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::Value;

use crate::error::StoreError;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn json_vec(items: &[String]) -> Result<String, StoreError> {
    serde_json::to_string(items).map_err(|e| StoreError::Corrupt {
        column: "json",
        detail: e.to_string(),
    })
}

fn parse_json_vec(raw: &str, column: &'static str) -> Result<Vec<String>, StoreError> {
    serde_json::from_str::<Vec<String>>(raw).map_err(|e| StoreError::Corrupt {
        column,
        detail: e.to_string(),
    })
}

fn parse_task_state(raw: &str) -> Result<TaskState, StoreError> {
    TaskState::parse(raw).ok_or_else(|| StoreError::Corrupt {
        column: "tasks.state",
        detail: format!("unknown task state {raw:?}"),
    })
}

fn parse_agent_kind(raw: &str) -> Result<AgentKind, StoreError> {
    raw.parse::<AgentKind>().map_err(|e| StoreError::Corrupt {
        column: "agents.kind",
        detail: e,
    })
}

fn parse_session_state(raw: &str) -> Result<SessionState, StoreError> {
    SessionState::parse(raw).ok_or_else(|| StoreError::Corrupt {
        column: "agent_sessions.state",
        detail: format!("unknown session state {raw:?}"),
    })
}

fn parse_worktree_state(raw: &str) -> Result<WorktreeState, StoreError> {
    WorktreeState::parse(raw).ok_or_else(|| StoreError::Corrupt {
        column: "worktrees.state",
        detail: format!("unknown worktree state {raw:?}"),
    })
}

fn parse_event_kind(raw: &str) -> Result<EventKind, StoreError> {
    EventKind::parse(raw).ok_or_else(|| StoreError::Corrupt {
        column: "events.kind",
        detail: format!("unknown event kind {raw:?}"),
    })
}

fn parse_severity(raw: &str) -> Result<Severity, StoreError> {
    Severity::parse(raw).ok_or_else(|| StoreError::Corrupt {
        column: "events.severity",
        detail: format!("unknown severity {raw:?}"),
    })
}

// ---------------------------------------------------------------------------
// meta
// ---------------------------------------------------------------------------

pub fn meta_get(conn: &Connection, key: &str) -> Result<Option<String>, StoreError> {
    conn.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
        r.get(0)
    })
    .optional()
    .map_err(Into::into)
}

pub fn meta_set(conn: &Connection, key: &str, value: &str) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// projects
// ---------------------------------------------------------------------------

fn project_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: row.get("id")?,
        name: row.get("name")?,
        root: PathBuf::from(row.get::<_, String>("root")?),
        default_branch: row.get("default_branch")?,
        created_at: row.get("created_at")?,
    })
}

pub fn get_project_by_name(conn: &Connection, name: &str) -> Result<Option<Project>, StoreError> {
    conn.query_row(
        "SELECT * FROM projects WHERE name = ?1",
        params![name],
        project_from_row,
    )
    .optional()
    .map_err(Into::into)
}

pub fn get_project_by_root(conn: &Connection, root: &str) -> Result<Option<Project>, StoreError> {
    conn.query_row(
        "SELECT * FROM projects WHERE root = ?1",
        params![root],
        project_from_row,
    )
    .optional()
    .map_err(Into::into)
}

pub fn get_project(conn: &Connection, id: i64) -> Result<Option<Project>, StoreError> {
    conn.query_row(
        "SELECT * FROM projects WHERE id = ?1",
        params![id],
        project_from_row,
    )
    .optional()
    .map_err(Into::into)
}

pub fn list_projects(conn: &Connection) -> Result<Vec<Project>, StoreError> {
    let mut stmt = conn.prepare("SELECT * FROM projects ORDER BY name")?;
    let rows = stmt.query_map([], project_from_row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Register a project idempotently: an existing row for the same root (or
/// the same name and root) is returned instead of creating a duplicate.
pub fn register_project(
    conn: &Connection,
    name: &str,
    root: &str,
    default_branch: Option<&str>,
) -> Result<(Project, bool), StoreError> {
    if let Some(p) = get_project_by_root(conn, root)? {
        return Ok((p, false));
    }
    if let Some(existing) = get_project_by_name(conn, name)? {
        return Err(StoreError::NotFound {
            entity: "project name conflict: name is already registered for root",
            key: format!("{name} -> {}", existing.root.display()),
        });
    }
    conn.execute(
        "INSERT INTO projects (name, root, default_branch, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![name, root, default_branch, now_ms()],
    )?;
    let p = get_project_by_root(conn, root)?.expect("inserted project");
    Ok((p, true))
}

// ---------------------------------------------------------------------------
// plans (idempotency)
// ---------------------------------------------------------------------------

/// Insert a plan request idempotently. Returns `(plan_id, created)`.
pub fn create_plan(
    conn: &Connection,
    project_id: i64,
    request_id: &str,
) -> Result<(i64, bool), StoreError> {
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM plans WHERE request_id = ?1",
            params![request_id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        return Ok((id, false));
    }
    conn.execute(
        "INSERT INTO plans (project_id, request_id, created_at) VALUES (?1, ?2, ?3)",
        params![project_id, request_id, now_ms()],
    )?;
    Ok((conn.last_insert_rowid(), true))
}

// ---------------------------------------------------------------------------
// tasks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct NewTask {
    pub project_id: i64,
    pub plan_id: Option<i64>,
    pub plan_key: Option<String>,
    pub title: String,
    pub spec: String,
    pub state: TaskState,
    pub priority: i64,
    pub required_caps: Vec<String>,
    pub claims: Vec<String>,
    pub preferred_agent: Option<String>,
    pub timeout_secs: Option<u64>,
}

fn task_from_row(row: &rusqlite::Row<'_>) -> Result<Task, StoreError> {
    let state_raw: String = row.get("state")?;
    let caps_raw: String = row.get("required_caps")?;
    let claims_raw: String = row.get("claims")?;
    Ok(Task {
        id: row.get("id")?,
        project_id: row.get("project_id")?,
        seq_no: row.get("seq_no")?,
        title: row.get("title")?,
        spec: row.get("spec")?,
        state: parse_task_state(&state_raw)?,
        priority: row.get("priority")?,
        required_caps: parse_json_vec(&caps_raw, "tasks.required_caps")?,
        claims: parse_json_vec(&claims_raw, "tasks.claims")?,
        preferred_agent: row.get("preferred_agent")?,
        agent_id: row.get("agent_id")?,
        worktree_id: row.get("worktree_id")?,
        base_commit: row.get("base_commit")?,
        timeout_secs: row.get::<_, Option<i64>>("timeout_secs")?.map(|v| v as u64),
        attempts: row.get("attempts")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        started_at: row.get("started_at")?,
        finished_at: row.get("finished_at")?,
        exit_code: row.get("exit_code")?,
        failure_reason: row.get("failure_reason")?,
        log_path: row.get("log_path")?,
        summary: row.get("summary")?,
        plan_id: row.get("plan_id")?,
        plan_key: row.get("plan_key")?,
    })
}

fn map_task_error(e: StoreError) -> rusqlite::Error {
    match e {
        StoreError::Sqlite(e) => e,
        other => rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(other),
        ),
    }
}

/// Create a task, allocating the per-project sequence number atomically.
pub fn create_task(conn: &mut Connection, new: NewTask) -> Result<Task, StoreError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let seq_no: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq_no), 0) + 1 FROM tasks WHERE project_id = ?1",
        params![new.project_id],
        |r| r.get(0),
    )?;
    let now = now_ms();
    tx.execute(
        "INSERT INTO tasks (
            project_id, plan_id, plan_key, seq_no, title, spec, state, priority,
            required_caps, claims, preferred_agent, timeout_secs, created_at, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13)",
        params![
            new.project_id,
            new.plan_id,
            new.plan_key,
            seq_no,
            new.title,
            new.spec,
            new.state.as_str(),
            new.priority,
            json_vec(&new.required_caps)?,
            json_vec(&new.claims)?,
            new.preferred_agent,
            new.timeout_secs.map(|v| v as i64),
            now,
        ],
    )?;
    let id = tx.last_insert_rowid();
    let task = tx.query_row("SELECT * FROM tasks WHERE id = ?1", params![id], |row| {
        task_from_row(row).map_err(map_task_error)
    })?;
    tx.commit()?;
    Ok(task)
}

/// Task template used when materializing a plan.
#[derive(Debug, Clone)]
pub struct PlanInsertTask {
    pub key: String,
    pub title: String,
    pub spec: String,
    pub required_caps: Vec<String>,
    pub claims: Vec<String>,
    pub deps: Vec<String>,
    pub preferred_agent: Option<String>,
    pub priority: i64,
    pub timeout_secs: Option<u64>,
}

/// Materialize a whole plan atomically: all tasks and dependency edges are
/// inserted inside a single transaction, sequence numbers are allocated in
/// plan order. State starts at `CREATED`.
pub fn create_plan_tasks(
    conn: &mut Connection,
    project_id: i64,
    plan_id: Option<i64>,
    tasks: &[PlanInsertTask],
) -> Result<Vec<Task>, StoreError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut seq: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq_no), 0) FROM tasks WHERE project_id = ?1",
        params![project_id],
        |r| r.get(0),
    )?;
    let now = now_ms();
    let mut key_to_id: std::collections::BTreeMap<String, TaskId> = Default::default();
    let mut ids: Vec<TaskId> = Vec::with_capacity(tasks.len());
    for t in tasks {
        seq += 1;
        tx.execute(
            "INSERT INTO tasks (
                project_id, plan_id, plan_key, seq_no, title, spec, state, priority,
                required_caps, claims, preferred_agent, timeout_secs, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13)",
            params![
                project_id,
                plan_id,
                t.key,
                seq,
                t.title,
                t.spec,
                TaskState::Created.as_str(),
                t.priority,
                json_vec(&t.required_caps)?,
                json_vec(&t.claims)?,
                t.preferred_agent,
                t.timeout_secs.map(|v| v as i64),
                now,
            ],
        )?;
        let id = tx.last_insert_rowid();
        key_to_id.insert(t.key.clone(), id);
        ids.push(id);
    }
    for t in tasks {
        let task_id = key_to_id[&t.key];
        for dep_key in &t.deps {
            let dep_id = *key_to_id.get(dep_key).ok_or_else(|| StoreError::NotFound {
                entity: "plan dependency",
                key: dep_key.clone(),
            })?;
            tx.execute(
                "INSERT OR IGNORE INTO task_dependencies (task_id, depends_on) VALUES (?1, ?2)",
                params![task_id, dep_id],
            )?;
        }
    }
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let task = tx.query_row("SELECT * FROM tasks WHERE id = ?1", params![id], |row| {
            task_from_row(row).map_err(map_task_error)
        })?;
        out.push(task);
    }
    tx.commit()?;
    Ok(out)
}

/// Tasks belonging to a plan, ordered by sequence.
pub fn list_tasks_by_plan(conn: &Connection, plan_id: i64) -> Result<Vec<Task>, StoreError> {
    let mut stmt = conn.prepare("SELECT * FROM tasks WHERE plan_id = ?1 ORDER BY seq_no")?;
    let rows = stmt.query_map(params![plan_id], |row| {
        task_from_row(row).map_err(map_task_error)
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn add_dependency(
    conn: &Connection,
    task_id: TaskId,
    depends_on: TaskId,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT OR IGNORE INTO task_dependencies (task_id, depends_on) VALUES (?1, ?2)",
        params![task_id, depends_on],
    )?;
    Ok(())
}

pub fn get_task(conn: &Connection, id: TaskId) -> Result<Option<Task>, StoreError> {
    conn.query_row("SELECT * FROM tasks WHERE id = ?1", params![id], |row| {
        task_from_row(row).map_err(map_task_error)
    })
    .optional()
    .map_err(Into::into)
}

pub fn get_task_by_seq(
    conn: &Connection,
    project_id: i64,
    seq_no: i64,
) -> Result<Option<Task>, StoreError> {
    conn.query_row(
        "SELECT * FROM tasks WHERE project_id = ?1 AND seq_no = ?2",
        params![project_id, seq_no],
        |row| task_from_row(row).map_err(map_task_error),
    )
    .optional()
    .map_err(Into::into)
}

/// Resolve a user-supplied task reference: `T-001`, `T1`, `001` or `1`.
pub fn find_task(
    conn: &Connection,
    project_id: i64,
    reference: &str,
) -> Result<Option<Task>, StoreError> {
    let digits = reference
        .trim()
        .trim_start_matches(['T', 't'])
        .trim_start_matches('-');
    if let Ok(seq) = digits.parse::<i64>() {
        if let Some(t) = get_task_by_seq(conn, project_id, seq)? {
            return Ok(Some(t));
        }
    }
    Ok(None)
}

pub fn list_tasks(conn: &Connection, project_id: i64) -> Result<Vec<Task>, StoreError> {
    let mut stmt = conn.prepare("SELECT * FROM tasks WHERE project_id = ?1 ORDER BY seq_no")?;
    let rows = stmt.query_map(params![project_id], |row| {
        task_from_row(row).map_err(map_task_error)
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn list_tasks_in_states(
    conn: &Connection,
    project_id: i64,
    states: &[TaskState],
) -> Result<Vec<Task>, StoreError> {
    let mut all = list_tasks(conn, project_id)?;
    all.retain(|t| states.contains(&t.state));
    Ok(all)
}

pub fn deps_of(conn: &Connection, task_id: TaskId) -> Result<Vec<TaskId>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT depends_on FROM task_dependencies WHERE task_id = ?1 ORDER BY depends_on",
    )?;
    let rows = stmt.query_map(params![task_id], |r| r.get(0))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn dependents_of(conn: &Connection, dep_id: TaskId) -> Result<Vec<TaskId>, StoreError> {
    let mut stmt = conn
        .prepare("SELECT task_id FROM task_dependencies WHERE depends_on = ?1 ORDER BY task_id")?;
    let rows = stmt.query_map(params![dep_id], |r| r.get(0))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn all_dependencies(
    conn: &Connection,
    project_id: i64,
) -> Result<Vec<(TaskId, TaskId)>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT d.task_id, d.depends_on
         FROM task_dependencies d
         JOIN tasks t ON t.id = d.task_id
         WHERE t.project_id = ?1
         ORDER BY d.task_id, d.depends_on",
    )?;
    let rows = stmt.query_map(params![project_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn count_tasks_in_states(
    conn: &Connection,
    project_id: i64,
    states: &[TaskState],
) -> Result<i64, StoreError> {
    // The list is tiny and closed; inline it to keep one prepared statement.
    let list = states
        .iter()
        .map(|s| format!("'{}'", s.as_str()))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("SELECT COUNT(*) FROM tasks WHERE project_id = ?1 AND state IN ({list})");
    let n: i64 = conn.query_row(&sql, params![project_id], |r| r.get(0))?;
    Ok(n)
}

/// Atomic state transition: fails if the task is not in `from` state or the
/// transition is not legal.
pub fn set_task_state(
    conn: &Connection,
    id: TaskId,
    from: TaskState,
    to: TaskState,
) -> Result<(), StoreError> {
    forge_core::state::check_transition(from, to)?;
    let n = conn.execute(
        "UPDATE tasks SET state = ?1, updated_at = ?2 WHERE id = ?3 AND state = ?4",
        params![to.as_str(), now_ms(), id, from.as_str()],
    )?;
    if n == 0 {
        return Err(StoreError::StateConflict {
            id,
            expected: from.as_str(),
        });
    }
    Ok(())
}

pub fn assign_task(
    conn: &Connection,
    id: TaskId,
    agent_id: &str,
    worktree_id: Option<i64>,
    base_commit: Option<&str>,
    log_path: Option<&str>,
) -> Result<(), StoreError> {
    let n = conn.execute(
        "UPDATE tasks SET agent_id = ?1, worktree_id = ?2, base_commit = ?3, log_path = ?4,
         updated_at = ?5 WHERE id = ?6",
        params![agent_id, worktree_id, base_commit, log_path, now_ms(), id],
    )?;
    if n == 0 {
        return Err(StoreError::TaskNotFound(id));
    }
    Ok(())
}

pub fn mark_task_started(conn: &Connection, id: TaskId, started_at: i64) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE tasks SET started_at = ?1, updated_at = ?1 WHERE id = ?2",
        params![started_at, id],
    )?;
    Ok(())
}

pub fn mark_task_finished(
    conn: &Connection,
    id: TaskId,
    finished_at: i64,
    exit_code: Option<i32>,
    failure_reason: Option<&str>,
    summary: Option<&str>,
) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE tasks SET finished_at = ?1, exit_code = ?2, failure_reason = ?3, summary = ?4,
         updated_at = ?1 WHERE id = ?5",
        params![finished_at, exit_code, failure_reason, summary, id],
    )?;
    Ok(())
}

pub fn bump_task_attempts(conn: &Connection, id: TaskId) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE tasks SET attempts = attempts + 1, updated_at = ?1 WHERE id = ?2",
        params![now_ms(), id],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// events
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct NewEvent {
    pub project_id: Option<i64>,
    pub task_id: Option<TaskId>,
    pub agent_id: Option<String>,
    pub kind: EventKind,
    pub severity: Severity,
    pub message: String,
    pub data: Option<Value>,
}

pub fn insert_event(conn: &Connection, ev: &NewEvent) -> Result<EventRecord, StoreError> {
    let data = match &ev.data {
        Some(v) => Some(serde_json::to_string(v).map_err(|e| StoreError::Corrupt {
            column: "events.data",
            detail: e.to_string(),
        })?),
        None => None,
    };
    let (id, ts): (i64, i64) = conn.query_row(
        "INSERT INTO events (ts, project_id, task_id, agent_id, kind, severity, message, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         RETURNING id, ts",
        params![
            now_ms(),
            ev.project_id,
            ev.task_id,
            ev.agent_id,
            ev.kind.as_str(),
            ev.severity.as_str(),
            ev.message,
            data
        ],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(EventRecord {
        id,
        ts,
        project_id: ev.project_id,
        task_id: ev.task_id,
        agent_id: ev.agent_id.clone(),
        kind: ev.kind,
        severity: ev.severity,
        message: ev.message.clone(),
        data: ev.data.clone(),
    })
}

fn event_from_row(row: &rusqlite::Row<'_>) -> Result<EventRecord, StoreError> {
    let kind_raw: String = row.get("kind")?;
    let sev_raw: String = row.get("severity")?;
    let data_raw: Option<String> = row.get("data")?;
    let data = match data_raw {
        Some(s) => Some(
            serde_json::from_str::<Value>(&s).map_err(|e| StoreError::Corrupt {
                column: "events.data",
                detail: e.to_string(),
            })?,
        ),
        None => None,
    };
    Ok(EventRecord {
        id: row.get("id")?,
        ts: row.get("ts")?,
        project_id: row.get("project_id")?,
        task_id: row.get("task_id")?,
        agent_id: row.get("agent_id")?,
        kind: parse_event_kind(&kind_raw)?,
        severity: parse_severity(&sev_raw)?,
        message: row.get("message")?,
        data,
    })
}

pub fn recent_events(
    conn: &Connection,
    limit: usize,
    since_id: Option<i64>,
    task_id: Option<TaskId>,
) -> Result<Vec<EventRecord>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT * FROM events
         WHERE (?1 IS NULL OR id > ?1) AND (?2 IS NULL OR task_id = ?2)
         ORDER BY id DESC LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![since_id, task_id, limit as i64], |row| {
        event_from_row(row).map_err(map_task_error)
    })?;
    let mut out: Vec<EventRecord> = rows.collect::<Result<Vec<_>, _>>()?;
    out.reverse(); // ascending by id
    Ok(out)
}

// ---------------------------------------------------------------------------
// agents
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AgentRow {
    pub id: String,
    pub kind: AgentKind,
    pub command: String,
    pub capabilities: Vec<String>,
    pub slots: u32,
    pub enabled: bool,
    pub version: Option<String>,
    pub probe_status: String,
    pub probe_detail: Option<String>,
    pub probed_at: Option<i64>,
}

fn agent_from_row(row: &rusqlite::Row<'_>) -> Result<AgentRow, StoreError> {
    let kind_raw: String = row.get("kind")?;
    let caps_raw: String = row.get("capabilities")?;
    Ok(AgentRow {
        id: row.get("id")?,
        kind: parse_agent_kind(&kind_raw)?,
        command: row.get("command")?,
        capabilities: parse_json_vec(&caps_raw, "agents.capabilities")?,
        slots: row.get::<_, i64>("slots")?.max(0) as u32,
        enabled: row.get::<_, i64>("enabled")? != 0,
        version: row.get("version")?,
        probe_status: row.get("probe_status")?,
        probe_detail: row.get("probe_detail")?,
        probed_at: row.get("probed_at")?,
    })
}

/// Insert or update the static part of an agent row (probe results are kept).
pub fn upsert_agent(conn: &Connection, spec: &AgentSpec) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO agents (id, kind, command, capabilities, slots, enabled)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(id) DO UPDATE SET
            kind = excluded.kind,
            command = excluded.command,
            capabilities = excluded.capabilities,
            slots = excluded.slots,
            enabled = excluded.enabled",
        params![
            spec.id,
            spec.kind.as_str(),
            spec.command,
            json_vec(&spec.capabilities)?,
            spec.slots as i64,
            spec.enabled as i64
        ],
    )?;
    Ok(())
}

pub fn set_agent_probe(
    conn: &Connection,
    id: &str,
    version: Option<&str>,
    status_label: &str,
    detail: Option<&str>,
) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE agents SET version = ?1, probe_status = ?2, probe_detail = ?3, probed_at = ?4
         WHERE id = ?5",
        params![version, status_label, detail, now_ms(), id],
    )?;
    Ok(())
}

pub fn list_agents(conn: &Connection) -> Result<Vec<AgentRow>, StoreError> {
    let mut stmt = conn.prepare("SELECT * FROM agents ORDER BY id")?;
    let rows = stmt.query_map([], |row| agent_from_row(row).map_err(map_task_error))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

// ---------------------------------------------------------------------------
// sessions
// ---------------------------------------------------------------------------

fn session_from_row(row: &rusqlite::Row<'_>) -> Result<AgentSession, StoreError> {
    let state_raw: String = row.get("state")?;
    Ok(AgentSession {
        id: row.get("id")?,
        agent_id: row.get("agent_id")?,
        task_id: row.get("task_id")?,
        pid: row.get::<_, Option<i64>>("pid")?.map(|v| v as u32),
        state: parse_session_state(&state_raw)?,
        started_at: row.get("started_at")?,
        ended_at: row.get("ended_at")?,
        exit_code: row.get("exit_code")?,
        worktree_id: row.get("worktree_id")?,
    })
}

pub fn open_session(
    conn: &Connection,
    agent_id: &str,
    task_id: TaskId,
    worktree_id: Option<i64>,
) -> Result<i64, StoreError> {
    conn.execute(
        "INSERT INTO agent_sessions (agent_id, task_id, worktree_id, state, started_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            agent_id,
            task_id,
            worktree_id,
            SessionState::Starting.as_str(),
            now_ms()
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn set_session_running(conn: &Connection, session_id: i64, pid: u32) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE agent_sessions SET state = ?1, pid = ?2 WHERE id = ?3",
        params![SessionState::Running.as_str(), pid as i64, session_id],
    )?;
    Ok(())
}

pub fn close_session(
    conn: &Connection,
    session_id: i64,
    state: SessionState,
    exit_code: Option<i32>,
) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE agent_sessions SET state = ?1, ended_at = ?2, exit_code = ?3 WHERE id = ?4",
        params![state.as_str(), now_ms(), exit_code, session_id],
    )?;
    Ok(())
}

pub fn open_sessions(conn: &Connection) -> Result<Vec<AgentSession>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT * FROM agent_sessions WHERE state IN ('STARTING', 'RUNNING') ORDER BY id",
    )?;
    let rows = stmt.query_map([], |row| session_from_row(row).map_err(map_task_error))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn sessions_for_task(
    conn: &Connection,
    task_id: TaskId,
) -> Result<Vec<AgentSession>, StoreError> {
    let mut stmt = conn.prepare("SELECT * FROM agent_sessions WHERE task_id = ?1 ORDER BY id")?;
    let rows = stmt.query_map(params![task_id], |row| {
        session_from_row(row).map_err(map_task_error)
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

// ---------------------------------------------------------------------------
// worktrees
// ---------------------------------------------------------------------------

fn worktree_from_row(row: &rusqlite::Row<'_>) -> Result<Worktree, StoreError> {
    let state_raw: String = row.get("state")?;
    Ok(Worktree {
        id: row.get("id")?,
        project_id: row.get("project_id")?,
        task_id: row.get("task_id")?,
        path: PathBuf::from(row.get::<_, String>("path")?),
        branch: row.get("branch")?,
        base_commit: row.get("base_commit")?,
        state: parse_worktree_state(&state_raw)?,
        created_at: row.get("created_at")?,
        removed_at: row.get("removed_at")?,
    })
}

pub fn insert_worktree(
    conn: &Connection,
    project_id: i64,
    task_id: TaskId,
    path: &Path,
    branch: &str,
    base_commit: Option<&str>,
    state: WorktreeState,
) -> Result<Worktree, StoreError> {
    conn.execute(
        "INSERT INTO worktrees (project_id, task_id, path, branch, base_commit, state, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            project_id,
            task_id,
            path.to_string_lossy(),
            branch,
            base_commit,
            state.as_str(),
            now_ms()
        ],
    )?;
    let id = conn.last_insert_rowid();
    get_worktree(conn, id)?.ok_or_else(|| StoreError::NotFound {
        entity: "worktree",
        key: id.to_string(),
    })
}

pub fn get_worktree(conn: &Connection, id: i64) -> Result<Option<Worktree>, StoreError> {
    conn.query_row(
        "SELECT * FROM worktrees WHERE id = ?1",
        params![id],
        |row| worktree_from_row(row).map_err(map_task_error),
    )
    .optional()
    .map_err(Into::into)
}

pub fn get_worktree_by_task(
    conn: &Connection,
    task_id: TaskId,
) -> Result<Option<Worktree>, StoreError> {
    conn.query_row(
        "SELECT * FROM worktrees WHERE task_id = ?1 ORDER BY id DESC LIMIT 1",
        params![task_id],
        |row| worktree_from_row(row).map_err(map_task_error),
    )
    .optional()
    .map_err(Into::into)
}

pub fn set_worktree_state(
    conn: &Connection,
    id: i64,
    state: WorktreeState,
) -> Result<(), StoreError> {
    let removed_at = if state == WorktreeState::Removed {
        Some(now_ms())
    } else {
        None
    };
    conn.execute(
        "UPDATE worktrees SET state = ?1, removed_at = COALESCE(?2, removed_at) WHERE id = ?3",
        params![state.as_str(), removed_at, id],
    )?;
    Ok(())
}

pub fn list_active_worktrees(conn: &Connection) -> Result<Vec<Worktree>, StoreError> {
    let mut stmt = conn.prepare("SELECT * FROM worktrees WHERE state = 'ACTIVE' ORDER BY id")?;
    let rows = stmt.query_map([], |row| worktree_from_row(row).map_err(map_task_error))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

// ---------------------------------------------------------------------------
// resource leases
// ---------------------------------------------------------------------------

fn lease_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ResourceLease> {
    Ok(ResourceLease {
        id: row.get("id")?,
        resource: row.get("resource")?,
        task_id: row.get("task_id")?,
        session_id: row.get("session_id")?,
        acquired_at: row.get("acquired_at")?,
        expires_at: row.get("expires_at")?,
        heartbeat_at: row.get("heartbeat_at")?,
    })
}

/// Try to acquire an exclusive lease. Succeeds when the resource is free,
/// expired, or already held by the same task. Returns false on conflict.
pub fn acquire_lease(
    conn: &Connection,
    resource: &str,
    task_id: TaskId,
    session_id: Option<i64>,
    ttl_ms: i64,
) -> Result<bool, StoreError> {
    let now = now_ms();
    let n = conn.execute(
        "INSERT INTO resource_leases (resource, task_id, session_id, acquired_at, heartbeat_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?4, ?5)
         ON CONFLICT(resource) DO UPDATE SET
            task_id = excluded.task_id,
            session_id = excluded.session_id,
            acquired_at = excluded.acquired_at,
            heartbeat_at = excluded.heartbeat_at,
            expires_at = excluded.expires_at
         WHERE resource_leases.expires_at <= ?4 OR resource_leases.task_id = ?2",
        params![resource, task_id, session_id, now, now + ttl_ms],
    )?;
    Ok(n == 1)
}

pub fn heartbeat_leases_for_task(
    conn: &Connection,
    task_id: TaskId,
    ttl_ms: i64,
) -> Result<usize, StoreError> {
    let now = now_ms();
    let n = conn.execute(
        "UPDATE resource_leases SET heartbeat_at = ?1, expires_at = ?2 WHERE task_id = ?3",
        params![now, now + ttl_ms, task_id],
    )?;
    Ok(n)
}

/// Delete expired leases, returning `(resource, task_id)` pairs.
pub fn expire_leases(conn: &Connection) -> Result<Vec<(String, TaskId)>, StoreError> {
    let now = now_ms();
    let mut stmt = conn.prepare(
        "DELETE FROM resource_leases WHERE expires_at <= ?1 RETURNING resource, task_id",
    )?;
    let rows = stmt.query_map(params![now], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Release every lease held by a task, returning the released resources.
pub fn release_leases_for_task(
    conn: &Connection,
    task_id: TaskId,
) -> Result<Vec<String>, StoreError> {
    let mut stmt =
        conn.prepare("DELETE FROM resource_leases WHERE task_id = ?1 RETURNING resource")?;
    let rows = stmt.query_map(params![task_id], |r| r.get(0))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn list_leases(conn: &Connection) -> Result<Vec<ResourceLease>, StoreError> {
    let mut stmt = conn.prepare("SELECT * FROM resource_leases ORDER BY resource")?;
    let rows = stmt.query_map([], lease_from_row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

// ---------------------------------------------------------------------------
// artifacts
// ---------------------------------------------------------------------------

fn artifact_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskArtifact> {
    Ok(TaskArtifact {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        kind: row.get("kind")?,
        path: row.get("path")?,
        value: row.get("value")?,
        created_at: row.get("created_at")?,
    })
}

pub fn insert_artifact(
    conn: &Connection,
    task_id: TaskId,
    kind: &str,
    path: Option<&str>,
    value: Option<&str>,
) -> Result<TaskArtifact, StoreError> {
    conn.execute(
        "INSERT INTO task_artifacts (task_id, kind, path, value, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![task_id, kind, path, value, now_ms()],
    )?;
    let id = conn.last_insert_rowid();
    let artifact = conn.query_row(
        "SELECT * FROM task_artifacts WHERE id = ?1",
        params![id],
        artifact_from_row,
    )?;
    Ok(artifact)
}

pub fn list_artifacts(conn: &Connection, task_id: TaskId) -> Result<Vec<TaskArtifact>, StoreError> {
    let mut stmt = conn.prepare("SELECT * FROM task_artifacts WHERE task_id = ?1 ORDER BY id")?;
    let rows = stmt.query_map(params![task_id], artifact_from_row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}
