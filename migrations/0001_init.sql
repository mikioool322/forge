-- Forge schema, version 1.
--
-- Conventions:
--   * timestamps are milliseconds since the Unix epoch (UTC),
--   * enum-like columns store the SCREAMING_SNAKE_CASE names used by
--     forge-core (`tasks.state`, `worktrees.state`, `agent_sessions.state`),
--   * list-like values are JSON arrays in TEXT columns,
--   * `PRAGMA user_version` tracks the schema version (managed by
--     forge-store::migrations).

CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE projects (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    name           TEXT NOT NULL UNIQUE,
    root           TEXT NOT NULL UNIQUE,
    default_branch TEXT,
    created_at     INTEGER NOT NULL
);

CREATE TABLE plans (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    request_id TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL
);

CREATE TABLE tasks (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id      INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    plan_id         INTEGER REFERENCES plans(id) ON DELETE SET NULL,
    plan_key        TEXT,
    seq_no          INTEGER NOT NULL,
    title           TEXT NOT NULL,
    spec            TEXT NOT NULL DEFAULT '',
    state           TEXT NOT NULL,
    priority        INTEGER NOT NULL DEFAULT 0,
    required_caps   TEXT NOT NULL DEFAULT '[]',
    claims          TEXT NOT NULL DEFAULT '[]',
    preferred_agent TEXT,
    agent_id        TEXT,
    worktree_id     INTEGER REFERENCES worktrees(id),
    base_commit     TEXT,
    timeout_secs    INTEGER,
    attempts        INTEGER NOT NULL DEFAULT 0,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    started_at      INTEGER,
    finished_at     INTEGER,
    exit_code       INTEGER,
    failure_reason  TEXT,
    log_path        TEXT,
    summary         TEXT,
    UNIQUE (project_id, seq_no)
);

CREATE INDEX idx_tasks_state   ON tasks(state);
CREATE INDEX idx_tasks_project ON tasks(project_id, seq_no);
CREATE INDEX idx_tasks_plan    ON tasks(plan_id);

CREATE TABLE task_dependencies (
    task_id    INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    depends_on INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    PRIMARY KEY (task_id, depends_on),
    CHECK (task_id <> depends_on)
);

CREATE INDEX idx_deps_depends_on ON task_dependencies(depends_on);

CREATE TABLE agents (
    id           TEXT PRIMARY KEY,
    kind         TEXT NOT NULL,
    command      TEXT NOT NULL,
    capabilities TEXT NOT NULL DEFAULT '[]',
    slots        INTEGER NOT NULL DEFAULT 1,
    enabled      INTEGER NOT NULL DEFAULT 1,
    version      TEXT,
    probe_status TEXT NOT NULL DEFAULT 'unknown',
    probe_detail TEXT,
    probed_at    INTEGER
);

CREATE TABLE agent_sessions (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id    TEXT NOT NULL,
    task_id     INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    worktree_id INTEGER REFERENCES worktrees(id),
    pid         INTEGER,
    state       TEXT NOT NULL,
    started_at  INTEGER NOT NULL,
    ended_at    INTEGER,
    exit_code   INTEGER
);

CREATE INDEX idx_sessions_task   ON agent_sessions(task_id);
CREATE INDEX idx_sessions_active ON agent_sessions(state);

CREATE TABLE worktrees (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id  INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    task_id     INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    path        TEXT NOT NULL UNIQUE,
    branch      TEXT NOT NULL UNIQUE,
    base_commit TEXT,
    state       TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    removed_at  INTEGER
);

CREATE INDEX idx_worktrees_state ON worktrees(state);
CREATE INDEX idx_worktrees_task  ON worktrees(task_id);

CREATE TABLE events (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    ts         INTEGER NOT NULL,
    project_id INTEGER,
    task_id    INTEGER,
    agent_id   TEXT,
    kind       TEXT NOT NULL,
    severity   TEXT NOT NULL,
    message    TEXT NOT NULL,
    data       TEXT
);

CREATE INDEX idx_events_task ON events(task_id, id);
CREATE INDEX idx_events_ts   ON events(ts);

CREATE TABLE resource_leases (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    resource     TEXT NOT NULL UNIQUE,
    task_id      INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    session_id   INTEGER,
    acquired_at  INTEGER NOT NULL,
    heartbeat_at INTEGER NOT NULL,
    expires_at   INTEGER NOT NULL
);

CREATE INDEX idx_leases_task ON resource_leases(task_id);
CREATE INDEX idx_leases_expires ON resource_leases(expires_at);

CREATE TABLE task_artifacts (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id    INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    kind       TEXT NOT NULL,
    path       TEXT,
    value      TEXT,
    created_at INTEGER NOT NULL
);

CREATE INDEX idx_artifacts_task ON task_artifacts(task_id);
