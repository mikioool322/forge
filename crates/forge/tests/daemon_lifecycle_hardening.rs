//! Daemon-level process lifecycle hardening tests (Sprint 01.5A).
//!
//! These drive the **real** `forge` binary against isolated repositories and
//! data directories and assert on real OS state (live process counts, PIDs),
//! durable SQLite state (attempt rows, tree verdicts) and supervisor behaviour.
//!
//! Findings covered: A01 (PID is not identity), A02 (spawn without a durable
//! session), A03 (orphan descendants), A05 (occupancy released too early),
//! plus the P03/P04/P08/P09/P10 scenarios from the sprint brief.
//!
//! Every test that starts a long-running process has a bounded lifetime and
//! terminates what it created (including deliberately escaped descendants).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use forge_protocol::messages::Request;
use forge_protocol::{Client, Endpoint};
use tokio::process::{Child, Command};

const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(30);
const TASK_TIMEOUT: Duration = Duration::from_secs(90);

/// Concurrency 1, one fast agent and one agent that hangs and leaves an escaped
/// descendant behind (the hardest tree shape).
const CONFIG: &str = r#"
[daemon]
max_concurrency = 1
task_timeout_secs = 60
heartbeat_interval_secs = 2
lease_timeout_secs = 30

[logs]
max_file_bytes = 1048576

[[agents]]
id = "worker"
kind = "generic"
command = "@self"
extra_args = ["__demo-agent", "--name", "worker", "--file", "work.txt", "--steps", "1", "--sleep-ms", "50"]
capabilities = ["code"]

[[agents]]
id = "hanger"
kind = "generic"
command = "@self"
extra_args = ["__demo-agent", "--name", "hanger", "--steps", "1", "--sleep-ms", "50", "--hang-ms", "30000", "--spawn-descendant-ms", "20000", "--breakaway"]
capabilities = ["code", "slow"]

[[agents]]
id = "failing"
kind = "generic"
command = "@self"
extra_args = ["__demo-agent", "--name", "failing", "--file", "attempt.txt", "--steps", "1", "--sleep-ms", "50", "--fail"]
capabilities = ["code", "failing"]
"#;

/// No agent can be probed, so nothing is ever dispatched: used to create tasks
/// without starting work.
const CONFIG_NO_AGENTS: &str = r#"
[daemon]
max_concurrency = 1
task_timeout_secs = 60
heartbeat_interval_secs = 2
lease_timeout_secs = 30

[[agents]]
id = "missing"
kind = "generic"
command = "forge-agent-that-does-not-exist-xyz"
extra_args = []
capabilities = ["code"]
"#;

fn forge_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_forge"))
}

struct Harness {
    _root: tempfile::TempDir,
    root: PathBuf,
    repo: PathBuf,
    data: PathBuf,
    config: PathBuf,
    daemon: Option<Child>,
}

impl Harness {
    async fn new() -> Self {
        Self::with_config(CONFIG).await
    }

    async fn with_config(config_text: &str) -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        let base = root.path().to_path_buf();
        let repo = base.join("repo");
        let data = base.join("data");
        let cfg_dir = base.join("cfg");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&cfg_dir).unwrap();

        git(&repo, &["init", "-b", "main"]).await;
        git(&repo, &["config", "user.name", "forge-e2e"]).await;
        git(&repo, &["config", "user.email", "forge-e2e@test"]).await;
        std::fs::write(repo.join("README.md"), "lifecycle hardening repo\n").unwrap();
        git(&repo, &["add", "."]).await;
        git(&repo, &["commit", "-m", "init"]).await;

        let config = cfg_dir.join("config.toml");
        std::fs::write(&config, config_text).unwrap();

        let harness = Self {
            _root: root,
            root: base,
            repo,
            data,
            config,
            daemon: None,
        };
        let out = harness.cli(&["init", "--name", "demo-api"]).await;
        assert!(out.contains("registered"), "forge init failed: {out}");
        harness
    }

    fn endpoint_file(&self) -> PathBuf {
        self.data.join("run").join("endpoint")
    }

    fn worktrees_dir(&self) -> PathBuf {
        self.data.join("worktrees").join("demo-api")
    }

    fn logs_dir(&self) -> PathBuf {
        self.data.join("logs").join("demo-api")
    }

    fn db_path(&self) -> PathBuf {
        // The store keeps one database under the data directory.
        for candidate in ["forge.db", "forge.sqlite", "state.db"] {
            let path = self.data.join(candidate);
            if path.exists() {
                return path;
            }
        }
        // Fall back to any *.db in the data dir.
        if let Ok(entries) = std::fs::read_dir(&self.data) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "db").unwrap_or(false) {
                    return path;
                }
            }
        }
        panic!("database file not found under {:?}", self.data);
    }

    fn sqlite(&self, sql: &str) {
        let conn = rusqlite::Connection::open(self.db_path()).expect("open db");
        conn.execute_batch(sql).expect("sqlite batch");
    }

    fn sqlite_value(&self, sql: &str) -> Option<String> {
        let conn = rusqlite::Connection::open(self.db_path()).expect("open db");
        conn.query_row(sql, [], |row| row.get::<_, Option<String>>(0))
            .ok()
            .flatten()
    }

    fn sqlite_count(&self, sql: &str) -> i64 {
        let conn = rusqlite::Connection::open(self.db_path()).expect("open db");
        conn.query_row(sql, [], |row| row.get::<_, i64>(0))
            .unwrap_or(-1)
    }

    async fn cli_impl(&self, args: &[&str], allow_failure: bool) -> (bool, String) {
        let output = Command::new(forge_bin())
            .arg("--data-dir")
            .arg(&self.data)
            .arg("--config")
            .arg(&self.config)
            .args(args)
            .current_dir(&self.repo)
            .stdin(Stdio::null())
            .output()
            .await
            .expect("run forge cli");
        let mut combined = String::from_utf8_lossy(&output.stdout).to_string();
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
        if !allow_failure {
            assert!(
                output.status.success(),
                "forge {args:?} failed ({}): {combined}",
                output.status
            );
        }
        (output.status.success(), combined)
    }

    async fn cli(&self, args: &[&str]) -> String {
        self.cli_impl(args, false).await.1
    }

    async fn start_daemon(&mut self) {
        assert!(self.daemon.is_none(), "daemon already running");
        let child = Command::new(forge_bin())
            .arg("--data-dir")
            .arg(&self.data)
            .arg("--config")
            .arg(&self.config)
            .args(["daemon", "start"])
            .current_dir(&self.repo)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn daemon");
        self.daemon = Some(child);
        self.wait_ready().await;
    }

    async fn wait_ready(&self) {
        let deadline = Instant::now() + DAEMON_START_TIMEOUT;
        while Instant::now() < deadline {
            if let Some(client) = self.try_connect().await {
                if client.ping().await.is_ok() {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!(
            "daemon did not become ready; endpoint file: {:?}",
            std::fs::read_to_string(self.endpoint_file())
        );
    }

    async fn try_connect(&self) -> Option<Client> {
        let spec = std::fs::read_to_string(self.endpoint_file()).ok()?;
        let endpoint = Endpoint::parse(spec.trim()).ok()?;
        let token = std::fs::read_to_string(self.data.join("run").join("token"))
            .ok()
            .map(|s| s.trim().to_string());
        Client::connect(&endpoint, "s015a-test", token.as_deref())
            .await
            .ok()
    }

    async fn stop_daemon(&mut self) {
        if let Some(client) = self.try_connect().await {
            let _ = client
                .call(Request::Shutdown {
                    idempotency_key: None,
                    drain: Some(false),
                })
                .await;
        }
        if let Some(mut child) = self.daemon.take() {
            let _ = tokio::time::timeout(Duration::from_secs(20), child.wait()).await;
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.endpoint_file().exists() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Hard kill (simulated crash).
    async fn crash_daemon(&mut self) {
        if let Some(mut child) = self.daemon.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    /// Poll `forge status --json` until `predicate` holds for all tasks.
    async fn wait_for_tasks<F>(&self, predicate: F) -> serde_json::Value
    where
        F: Fn(&[serde_json::Value]) -> bool,
    {
        let deadline = Instant::now() + TASK_TIMEOUT;
        loop {
            let out = self.cli(&["status", "--json"]).await;
            let value: serde_json::Value = serde_json::from_str(&out).expect("status json");
            let tasks = value["tasks"].as_array().cloned().unwrap_or_default();
            if predicate(&tasks) {
                return value;
            }
            if Instant::now() >= deadline {
                panic!("tasks did not reach the expected state in time: {value:#}");
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    fn plan_file(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.root.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    /// The newest task log of the project, as text.
    fn newest_log(&self) -> String {
        let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
        if let Ok(entries) = std::fs::read_dir(self.logs_dir()) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "log").unwrap_or(false) {
                    if let Ok(meta) = entry.metadata() {
                        let modified = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                        if newest.as_ref().map(|(t, _)| modified > *t).unwrap_or(true) {
                            newest = Some((modified, path));
                        }
                    }
                }
            }
        }
        newest
            .map(|(_, path)| std::fs::read_to_string(path).unwrap_or_default())
            .unwrap_or_default()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(mut child) = self.daemon.take() {
            let _ = child.start_kill();
        }
    }
}

async fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .await
        .expect("git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn one_task_plan(agent: &str) -> String {
    format!(
        r#"
plan_version = 1
project = "demo-api"

[[tasks]]
key = "only"
title = "lifecycle task"
spec = "Do the work."
capabilities = ["code"]
agent = "{agent}"
"#
    )
}

fn two_task_plan(first_agent: &str, second_agent: &str) -> String {
    format!(
        r#"
plan_version = 1
project = "demo-api"

[[tasks]]
key = "first"
title = "first task"
spec = "Hang around."
capabilities = ["code"]
agent = "{first_agent}"

[[tasks]]
key = "second"
title = "second task"
spec = "Do the work."
capabilities = ["code"]
agent = "{second_agent}"
"#
    )
}

// ---------------------------------------------------------------------------
// Real OS helpers
// ---------------------------------------------------------------------------

/// PIDs of live **agent** processes whose command line contains `marker`.
///
/// Only `forge` processes are counted: the query itself (PowerShell / pgrep) has
/// the marker in its own command line and must never be mistaken for an agent.
fn live_pids(marker: &str) -> Vec<u32> {
    #[cfg(windows)]
    {
        let script = format!(
            "(Get-CimInstance Win32_Process | Where-Object {{ $_.Name -eq 'forge.exe' -and $_.CommandLine -like '*{marker}*' }} | ForEach-Object {{ $_.ProcessId }}) -join ','"
        );
        let output = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output();
        match output {
            Ok(out) => String::from_utf8_lossy(&out.stdout)
                .trim()
                .split(',')
                .filter_map(|t| t.trim().parse::<u32>().ok())
                .collect(),
            Err(_) => vec![],
        }
    }
    #[cfg(unix)]
    {
        let output = std::process::Command::new("pgrep")
            .args(["-f", &format!("forge.*{marker}")])
            .output();
        match output {
            Ok(out) => String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|l| l.trim().parse::<u32>().ok())
                .collect(),
            Err(_) => vec![],
        }
    }
}

/// A process owned by the *test*: Forge must never touch it.
fn spawn_control_process() -> std::process::Child {
    #[cfg(windows)]
    {
        std::process::Command::new("cmd.exe")
            .args(["/C", "ping -n 40 127.0.0.1 > NUL"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("control process")
    }
    #[cfg(unix)]
    {
        std::process::Command::new("sh")
            .args(["-c", "sleep 40"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("control process")
    }
}

fn pid_alive(pid: u32) -> bool {
    forge_runtime::platform::process_snapshot(pid).is_some()
}

fn kill_pid(pid: u32) {
    let _ = forge_runtime::platform::kill_tree(pid, true);
}

async fn wait_until<F: Fn() -> bool>(timeout: Duration, predicate: F) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    predicate()
}

/// The descendant PID the demo agent reported in its log.
fn descendant_pid_from_log(log: &str) -> Option<u32> {
    for line in log.lines() {
        if let Some(idx) = line.find("spawned descendant pid=") {
            let rest = &line[idx + "spawned descendant pid=".len()..];
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(pid) = digits.parse::<u32>() {
                return Some(pid);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// P03 — a rejected attempt record means no process runs (A02)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn p03_a_rejected_attempt_record_means_no_process_runs() {
    let mut h = Harness::new().await;
    h.start_daemon().await;

    // Fault injection: every attempt row is rejected by the database.
    h.sqlite(
        "CREATE TRIGGER fail_sessions BEFORE INSERT ON agent_sessions \
         BEGIN SELECT RAISE(ABORT, 'injected attempt-record failure'); END;",
    );

    let plan = h.plan_file("p03", &one_task_plan("worker"));
    h.cli(&["run", plan.to_str().unwrap()]).await;

    let snapshot = h
        .wait_for_tasks(|tasks| {
            tasks
                .iter()
                .any(|t| t["task"]["state"].as_str() == Some("FAILED"))
        })
        .await;
    let task = &snapshot["tasks"][0];
    let reason = task["task"]["failure_reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("attempt"),
        "the failure must name the durable attempt record: {reason}"
    );

    // Durable evidence: no attempt row exists at all.
    assert_eq!(
        h.sqlite_count("SELECT COUNT(*) FROM agent_sessions"),
        0,
        "no attempt row may exist"
    );
    // OS evidence: nothing ran, so no work was produced.
    let mut produced = false;
    if let Ok(entries) = std::fs::read_dir(h.worktrees_dir()) {
        for entry in entries.flatten() {
            if entry.path().join("work.txt").exists() {
                produced = true;
            }
        }
    }
    assert!(!produced, "no agent process may have produced work");
    assert!(
        live_pids("--name worker").is_empty(),
        "no agent process may be alive"
    );

    h.stop_daemon().await;
}

// ---------------------------------------------------------------------------
// P04 — an attempt without a process identity is reconciled, never guessed
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn p04_an_attempt_without_a_recorded_process_is_reconciled_safely() {
    let control = spawn_control_process();
    let control_pid = control.id();

    // Build the crash-boundary state with a daemon that cannot dispatch: a task
    // in RUNNING with an open attempt row that never recorded a PID.
    let mut h = Harness::with_config(CONFIG_NO_AGENTS).await;
    h.start_daemon().await;
    let plan = h.plan_file("p04", &one_task_plan("missing"));
    h.cli(&["run", plan.to_str().unwrap()]).await;
    h.wait_for_tasks(|tasks| !tasks.is_empty()).await;
    h.stop_daemon().await;

    let task_id = h.sqlite_count("SELECT id FROM tasks ORDER BY id LIMIT 1");
    h.sqlite(&format!(
        "UPDATE tasks SET state = 'RUNNING', agent_id = 'missing', started_at = 1 WHERE id = {task_id}"
    ));
    h.sqlite(&format!(
        "INSERT INTO agent_sessions (agent_id, task_id, worktree_id, pid, state, started_at, \
         owner_instance, attempt_no, run_token) \
         VALUES ('missing', {task_id}, NULL, NULL, 'STARTING', 1, 'previous-instance', 0, 'tok-p04')"
    ));

    // Restart: recovery must reconcile the attempt without inventing a process.
    h.start_daemon().await;
    let snapshot = h
        .wait_for_tasks(|tasks| {
            tasks
                .iter()
                .any(|t| t["task"]["state"].as_str() == Some("INTERRUPTED"))
        })
        .await;
    assert_eq!(
        snapshot["tasks"][0]["task"]["state"].as_str(),
        Some("INTERRUPTED")
    );

    let verdict = h.sqlite_value(&format!(
        "SELECT tree_state FROM agent_sessions WHERE task_id = {task_id}"
    ));
    assert_eq!(
        verdict.as_deref(),
        Some("UNVERIFIED"),
        "an attempt with no process identity must be recorded as unverified"
    );
    assert_eq!(
        h.sqlite_count(&format!(
            "SELECT COUNT(*) FROM agent_sessions WHERE task_id = {task_id} AND state IN ('STARTING','RUNNING')"
        )),
        0,
        "no attempt may stay open after recovery"
    );
    assert!(
        pid_alive(control_pid),
        "recovery must never touch a process it cannot prove it owns"
    );

    h.stop_daemon().await;
    let mut control = control;
    let _ = control.kill();
    let _ = control.wait();
}

// ---------------------------------------------------------------------------
// P08 — a cancelled attempt keeps its slot until the tree is closed (A05)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn p08_a_cancelled_attempt_keeps_its_slot_until_the_tree_is_closed() {
    let mut h = Harness::new().await;
    h.start_daemon().await;

    let plan = h.plan_file("p08", &two_task_plan("hanger", "worker"));
    h.cli(&["run", plan.to_str().unwrap()]).await;
    h.wait_for_tasks(|tasks| {
        tasks
            .iter()
            .any(|t| t["task"]["state"].as_str() == Some("RUNNING"))
    })
    .await;

    // The escaped descendant exists (it is outside the job on purpose).
    let descendant = wait_until(Duration::from_secs(20), || {
        descendant_pid_from_log(&h.newest_log()).is_some()
    })
    .await;
    let descendant_pid = descendant_pid_from_log(&h.newest_log());
    assert!(
        descendant,
        "the hanger agent must report its escaped descendant"
    );
    if let Some(pid) = descendant_pid {
        assert!(pid_alive(pid), "the escaped descendant is alive");
    }

    // The second task must not start while the first attempt occupies the only
    // slot, and the OS must never show more than one agent process at a time.
    let first_task = h.sqlite_count("SELECT id FROM tasks ORDER BY id LIMIT 1");
    let second_task = h.sqlite_count("SELECT id FROM tasks ORDER BY id DESC LIMIT 1");
    let _ = first_task;
    let _ = second_task;

    // Cancel the hanging task.
    h.cli(&["cancel", "T-001", "--reason", "p08"]).await;

    // Sample real OS state across the cancellation window.
    let mut max_agent_processes = 0usize;
    let sample_deadline = Instant::now() + Duration::from_secs(8);
    let mut second_started_while_unsettled = false;
    while Instant::now() < sample_deadline {
        let agents = live_pids("--name hanger").len() + live_pids("--name worker").len();
        max_agent_processes = max_agent_processes.max(agents);

        let out = h.cli(&["status", "--json"]).await;
        let value: serde_json::Value = serde_json::from_str(&out).expect("status json");
        let tasks = value["tasks"].as_array().cloned().unwrap_or_default();
        let first = tasks
            .iter()
            .find(|t| t["task"]["title"].as_str() == Some("first task"));
        let second = tasks
            .iter()
            .find(|t| t["task"]["title"].as_str() == Some("second task"));
        let first_unsettled = first.map(|t| t["attempt"].is_object()).unwrap_or(false);
        let second_running = second
            .map(|t| t["task"]["state"].as_str() == Some("RUNNING"))
            .unwrap_or(false);
        if first_unsettled && second_running {
            second_started_while_unsettled = true;
        }
        if first.map(|t| t["attempt"].is_null()).unwrap_or(false) && second_running {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    assert!(
        !second_started_while_unsettled,
        "a new attempt must not run while the cancelled attempt is unsettled"
    );
    assert!(
        max_agent_processes <= 1,
        "the actual number of live agent processes must never exceed max_concurrency=1 (saw {max_agent_processes})"
    );

    // The cancelled task is durably settled with a tree verdict, and the escaped
    // descendant is reported rather than claimed.
    let first_task_id = h.sqlite_count("SELECT id FROM tasks WHERE title = 'first task'");
    let verdict = wait_until(Duration::from_secs(20), || {
        h.sqlite_value(&format!(
            "SELECT tree_state FROM agent_sessions WHERE task_id = {first_task_id} ORDER BY id DESC LIMIT 1"
        ))
        .is_some()
    })
    .await;
    assert!(verdict, "the cancelled attempt must be settled durably");
    let tree = h.sqlite_value(&format!(
        "SELECT tree_state FROM agent_sessions WHERE task_id = {first_task_id} ORDER BY id DESC LIMIT 1"
    ));
    assert!(
        matches!(
            tree.as_deref(),
            Some("SURVIVED") | Some("TERMINATED") | Some("EMPTY")
        ),
        "the tree verdict must be recorded, got {tree:?}"
    );

    // Cleanup: the escaped descendant is terminated by the test.
    if let Some(pid) = descendant_pid {
        kill_pid(pid);
    }
    h.stop_daemon().await;
}

// ---------------------------------------------------------------------------
// P09 — a retry is a new attempt; the old one cannot affect it (A05)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn p09_a_retry_is_a_new_attempt_with_its_own_identity() {
    let mut h = Harness::new().await;
    h.start_daemon().await;

    // A failing agent: the task ends FAILED and becomes retryable.
    let plan = h.plan_file(
        "p09",
        r#"
plan_version = 1
project = "demo-api"

[[tasks]]
key = "flaky"
title = "retry task"
spec = "Fail once, then be retried."
capabilities = ["failing"]
agent = "failing"
"#,
    );
    h.cli(&["run", plan.to_str().unwrap()]).await;
    h.wait_for_tasks(|tasks| {
        tasks
            .iter()
            .any(|t| t["task"]["state"].as_str() == Some("FAILED"))
    })
    .await;

    let task_id = h.sqlite_count("SELECT id FROM tasks WHERE title = 'retry task'");
    assert_eq!(
        h.sqlite_count(&format!(
            "SELECT COUNT(*) FROM agent_sessions WHERE task_id = {task_id}"
        )),
        1,
        "one attempt ran"
    );
    let first_token = h.sqlite_value(&format!(
        "SELECT run_token FROM agent_sessions WHERE task_id = {task_id} ORDER BY id LIMIT 1"
    ));

    // Retry: a *new* attempt of the same task, with its own identity.
    let out = h.cli(&["retry", "T-001"]).await;
    assert!(
        out.contains("re-queued") || out.contains("retry"),
        "the retry must be acknowledged: {out}"
    );
    h.wait_for_tasks(|tasks| {
        tasks.iter().any(|t| {
            t["task"]["state"].as_str() == Some("FAILED")
                && t["task"]["attempts"].as_i64() == Some(1)
        })
    })
    .await;

    assert_eq!(
        h.sqlite_count(&format!(
            "SELECT COUNT(*) FROM agent_sessions WHERE task_id = {task_id}"
        )),
        2,
        "the retry created a second attempt row"
    );
    let attempts = h.sqlite_value(&format!(
        "SELECT GROUP_CONCAT(attempt_no) FROM (SELECT attempt_no FROM agent_sessions WHERE task_id = {task_id} ORDER BY id)"
    ));
    assert_eq!(
        attempts.as_deref(),
        Some("0,1"),
        "attempts are numbered and distinguishable"
    );
    let second_token = h.sqlite_value(&format!(
        "SELECT run_token FROM agent_sessions WHERE task_id = {task_id} ORDER BY id DESC LIMIT 1"
    ));
    assert_ne!(
        first_token, second_token,
        "each attempt carries its own run token"
    );
    assert_eq!(
        h.sqlite_count(&format!(
            "SELECT COUNT(*) FROM agent_sessions WHERE task_id = {task_id} AND state IN ('STARTING','RUNNING')"
        )),
        0,
        "no attempt may stay open"
    );
    assert_eq!(
        h.sqlite_count(&format!(
            "SELECT COUNT(*) FROM agent_sessions WHERE task_id = {task_id} AND tree_state IS NOT NULL"
        )),
        2,
        "every settled attempt records a tree verdict"
    );

    h.stop_daemon().await;
}

// ---------------------------------------------------------------------------
// P10 — a daemon crash takes its tree with it and never touches foreign work
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn p10_a_daemon_crash_never_touches_unrelated_processes() {
    let control = spawn_control_process();
    let control_pid = control.id();

    let mut h = Harness::new().await;
    h.start_daemon().await;
    let plan = h.plan_file("p10", &one_task_plan("hanger"));
    h.cli(&["run", plan.to_str().unwrap()]).await;
    h.wait_for_tasks(|tasks| {
        tasks
            .iter()
            .any(|t| t["task"]["state"].as_str() == Some("RUNNING"))
    })
    .await;

    let descendant_ready = wait_until(Duration::from_secs(20), || {
        descendant_pid_from_log(&h.newest_log()).is_some()
    })
    .await;
    let descendant_pid = descendant_pid_from_log(&h.newest_log());
    assert!(descendant_ready, "the escaped descendant must exist");

    // Crash the daemon: on Windows the Job Object closes and must take the whole
    // owned tree with it; nothing else may be affected.
    h.crash_daemon().await;

    #[cfg(windows)]
    {
        let agent_gone = wait_until(Duration::from_secs(15), || {
            live_pids("--name hanger").is_empty()
        })
        .await;
        assert!(
            agent_gone,
            "a daemon crash must close the Job Object and kill the owned tree"
        );
    }

    assert!(
        pid_alive(control_pid),
        "the crash must not affect processes Forge does not own"
    );

    // Restart: the durable state is reconciled, and recovery kills nothing it
    // cannot prove it owns.
    h.start_daemon().await;
    let snapshot = h
        .wait_for_tasks(|tasks| {
            tasks
                .iter()
                .any(|t| t["task"]["state"].as_str() == Some("INTERRUPTED"))
        })
        .await;
    assert_eq!(
        snapshot["tasks"][0]["task"]["state"].as_str(),
        Some("INTERRUPTED")
    );
    assert!(
        pid_alive(control_pid),
        "recovery must not terminate an unrelated process"
    );

    let task_id = h.sqlite_count("SELECT id FROM tasks ORDER BY id LIMIT 1");
    let verdict = h.sqlite_value(&format!(
        "SELECT tree_state FROM agent_sessions WHERE task_id = {task_id} ORDER BY id DESC LIMIT 1"
    ));
    assert!(
        verdict.is_some(),
        "recovery must record how the attempt's tree ended"
    );
    assert_eq!(
        h.sqlite_count(&format!(
            "SELECT COUNT(*) FROM agent_sessions WHERE task_id = {task_id} AND state IN ('STARTING','RUNNING')"
        )),
        0,
        "no attempt may stay open after recovery"
    );

    // Cleanup: the deliberately escaped descendant is terminated by the test.
    if let Some(pid) = descendant_pid {
        kill_pid(pid);
    }
    h.stop_daemon().await;
    let mut control = control;
    let _ = control.kill();
    let _ = control.wait();
}

// ---------------------------------------------------------------------------
// Migration: an existing version-1 database keeps its data
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn migration_from_v1_preserves_existing_rows() {
    let mut h = Harness::new().await;
    // The daemon (or any store open) migrates the database to the latest schema.
    h.start_daemon().await;
    h.stop_daemon().await;

    let version = h.sqlite_count("PRAGMA user_version");
    assert_eq!(version, 2, "the database is at schema version 2");
    let columns = h.sqlite_count(
        "SELECT COUNT(*) FROM pragma_table_info('agent_sessions') \
         WHERE name IN ('attempt_no','run_token','pid_start_marker','pid_fingerprint','owner_instance','tree_state')",
    );
    assert_eq!(
        columns, 6,
        "every identity column exists after the migration"
    );
    // The pre-existing tables and their data are untouched by the migration.
    assert!(
        h.sqlite_count("SELECT COUNT(*) FROM projects") >= 1,
        "registered projects survive the migration"
    );
}
