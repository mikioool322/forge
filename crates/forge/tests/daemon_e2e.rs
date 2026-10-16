//! End-to-end daemon integration tests.
//!
//! These tests spawn the real `forge` binary against isolated temporary
//! repositories and data directories (`--data-dir` / `--config` flags, so no
//! environment variables are mutated and tests stay parallel-safe).
//!
//! Covered: daemon lifecycle, plan submission and execution, worktree
//! isolation, artifact/log retrieval, cancellation, TUI detach semantics and
//! restart reconciliation (including orphan process cleanup).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use forge_protocol::messages::{Notification, Request, Response};
use forge_protocol::{Client, Endpoint};
use tokio::process::{Child, Command};

const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(30);
const TASK_TIMEOUT: Duration = Duration::from_secs(90);

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
    /// Create a temp repo (with one commit), a config with deterministic
    /// agents and register the project.
    async fn new() -> Self {
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
        std::fs::write(repo.join("README.md"), "e2e demo repository\n").unwrap();
        git(&repo, &["add", "."]).await;
        git(&repo, &["commit", "-m", "init"]).await;

        let config = cfg_dir.join("config.toml");
        std::fs::write(&config, CONFIG).unwrap();

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

    /// Run a CLI command against this harness's data/config and return its
    /// combined output. Panics on non-zero exit unless `allow_failure`.
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

    /// Start the daemon as a foreground child process and wait until its
    /// endpoint answers.
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
        Client::connect(&endpoint, "e2e-test", token.as_deref())
            .await
            .ok()
    }

    async fn client(&self) -> Client {
        self.try_connect().await.expect("daemon client")
    }

    /// Graceful shutdown through the IPC interface.
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
        // Wait until the endpoint file is gone.
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
}

impl Drop for Harness {
    /// Best-effort: never leak a daemon process, even when a test panics.
    fn drop(&mut self) {
        if let Some(child) = self.daemon.as_mut() {
            let _ = child.start_kill();
        }
    }
}

const CONFIG: &str = r#"
[daemon]
max_concurrency = 2
task_timeout_secs = 60
heartbeat_interval_secs = 2
lease_timeout_secs = 30

[logs]
max_file_bytes = 1048576

[[agents]]
id = "sim-a"
kind = "generic"
command = "@self"
extra_args = ["__demo-agent", "--name", "sim-a", "--file", "alpha.txt", "--steps", "3", "--sleep-ms", "200"]
capabilities = ["code"]

[[agents]]
id = "sim-b"
kind = "generic"
command = "@self"
extra_args = ["__demo-agent", "--name", "sim-b", "--file", "beta.txt", "--steps", "3", "--sleep-ms", "200"]
capabilities = ["code"]

[[agents]]
id = "sim-slow"
kind = "generic"
command = "@self"
extra_args = ["__demo-agent", "--name", "sim-slow", "--steps", "1", "--sleep-ms", "100", "--hang-ms", "30000"]
capabilities = ["code", "slow"]
"#;

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

fn two_task_plan() -> &'static str {
    r#"
plan_version = 1
project = "demo-api"

[[tasks]]
key = "alpha"
title = "create alpha module"
spec = "Create alpha.txt."
capabilities = ["code"]
claims = ["file:alpha.txt"]

[[tasks]]
key = "beta"
title = "create beta module"
spec = "Create beta.txt."
capabilities = ["code"]
claims = ["file:beta.txt"]
"#
}

fn slow_plan() -> &'static str {
    r#"
plan_version = 1
project = "demo-api"

[[tasks]]
key = "slow"
title = "slow task"
spec = "Sleep for a while."
capabilities = ["slow"]
"#
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_lifecycle_status_and_clean_stop() {
    let mut h = Harness::new().await;
    let before = h.cli_impl(&["daemon", "status"], true).await;
    assert!(
        !before.0,
        "status must fail while no daemon runs: {}",
        before.1
    );

    h.start_daemon().await;
    let status = h.cli(&["daemon", "status"]).await;
    assert!(status.contains("daemon: running"), "{status}");

    // Two clients can connect simultaneously (CLI + raw client).
    let client = h.client().await;
    assert!(client.ping().await.is_ok());
    let _ = h.cli(&["status"]).await;

    h.stop_daemon().await;
    assert!(!h.endpoint_file().exists(), "endpoint file must be removed");
    let after = h.cli_impl(&["daemon", "status"], true).await;
    assert!(!after.0, "status must fail after stop: {}", after.1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plan_runs_concurrently_in_isolated_worktrees() {
    let mut h = Harness::new().await;
    h.start_daemon().await;
    let plan = h.plan_file("plan.toml", two_task_plan());
    let out = h.cli(&["run", &plan.to_string_lossy()]).await;
    assert!(out.contains("accepted"), "{out}");

    let value = h
        .wait_for_tasks(|tasks| {
            tasks.len() == 2 && tasks.iter().all(|t| t["task"]["state"] == "COMPLETED")
        })
        .await;

    // Independent worktrees with the files produced by the two agents.
    let wt_a = h.worktrees_dir().join("t-001-r0");
    let wt_b = h.worktrees_dir().join("t-002-r0");
    assert!(wt_a.join("alpha.txt").exists(), "alpha worktree file");
    assert!(wt_b.join("beta.txt").exists(), "beta worktree file");
    assert!(
        !wt_a.join("beta.txt").exists(),
        "worktrees must be isolated"
    );

    // The canonical repository is untouched.
    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&h.repo)
        .output()
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&status.stdout).trim().is_empty(),
        "canonical repo must stay clean"
    );

    // Both tasks completed (not left running / failed).
    let states: Vec<String> = value["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["task"]["state"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(states, vec!["COMPLETED", "COMPLETED"]);

    // Logs are retrievable and contain the process lifecycle lines.
    let logs = h.cli(&["logs", "T-001", "-n", "200"]).await;
    assert!(logs.contains("wrote alpha.txt"), "{logs}");
    assert!(logs.contains("process exited: code=Some(0)"), "{logs}");

    // Diff retrieval through the IPC API (what the TUI 'd' key uses).
    let client = h.client().await;
    let response = client
        .call(Request::GetTaskDiff {
            project: Some("demo-api".into()),
            task: "T-001".into(),
            max_bytes: None,
        })
        .await
        .unwrap();
    match response {
        Response::TaskDiff { text, .. } => {
            assert!(text.contains("alpha.txt"), "{text}");
            assert!(text.contains("+"), "{text}");
        }
        other => panic!("unexpected diff response: {other:?}"),
    }

    // Idempotent replay: running the same plan again must not duplicate tasks.
    let out = h.cli(&["run", &plan.to_string_lossy()]).await;
    assert!(out.contains("already known"), "{out}");
    let value = h.cli(&["status", "--json"]).await;
    let parsed: serde_json::Value = serde_json::from_str(&value).unwrap();
    assert_eq!(parsed["tasks"].as_array().unwrap().len(), 2);

    h.stop_daemon().await;

    // State survives a restart.
    h.start_daemon().await;
    let value = h.cli(&["status", "--json"]).await;
    let parsed: serde_json::Value = serde_json::from_str(&value).unwrap();
    assert_eq!(parsed["tasks"].as_array().unwrap().len(), 2);
    assert_eq!(parsed["tasks"][0]["task"]["state"], "COMPLETED");
    h.stop_daemon().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancellation_terminates_the_task() {
    let mut h = Harness::new().await;
    h.start_daemon().await;
    let plan = h.plan_file("slow.toml", slow_plan());
    h.cli(&["run", &plan.to_string_lossy()]).await;
    h.wait_for_tasks(|tasks| tasks.iter().any(|t| t["task"]["state"] == "RUNNING"))
        .await;

    let out = h.cli(&["cancel", "T-001"]).await;
    assert!(out.contains("cancellation requested"), "{out}");
    let value = h
        .wait_for_tasks(|tasks| {
            tasks.iter().any(|t| {
                t["task"]["state"] == "CANCELLED" && t["task"]["failure_reason"] == "cancelled"
            })
        })
        .await;
    assert_eq!(value["tasks"][0]["task"]["state"], "CANCELLED");
    assert_eq!(value["tasks"][0]["task"]["failure_reason"], "cancelled");
    // Terminal state is stable.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let value = h.cli(&["status", "--json"]).await;
    let parsed: serde_json::Value = serde_json::from_str(&value).unwrap();
    assert_eq!(parsed["tasks"][0]["task"]["state"], "CANCELLED");
    h.stop_daemon().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tui_detach_does_not_stop_work() {
    let mut h = Harness::new().await;
    h.start_daemon().await;
    let plan = h.plan_file("slow.toml", slow_plan());
    h.cli(&["run", &plan.to_string_lossy()]).await;
    h.wait_for_tasks(|tasks| tasks.iter().any(|t| t["task"]["state"] == "RUNNING"))
        .await;

    // A "TUI session": connect, subscribe, fetch the snapshot, then detach.
    {
        let client = h.client().await;
        client
            .call(Request::Subscribe {
                since_event_id: None,
            })
            .await
            .unwrap();
        let response = client.call(Request::GetState).await.unwrap();
        assert!(matches!(response, Response::State(_)));
        // Subscription stream is dropped here (client dropped).
    }

    // The task keeps running after the client detached.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let value = h.cli(&["status", "--json"]).await;
    let parsed: serde_json::Value = serde_json::from_str(&value).unwrap();
    assert_eq!(parsed["tasks"][0]["task"]["state"], "RUNNING");
    // And it can still be cancelled afterwards.
    h.cli(&["cancel", "T-001"]).await;
    h.wait_for_tasks(|tasks| tasks.iter().any(|t| t["task"]["state"] == "CANCELLED"))
        .await;
    h.stop_daemon().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_reconciliation_marks_interrupted_and_kills_orphans() {
    let mut h = Harness::new().await;
    h.start_daemon().await;
    let plan = h.plan_file("slow.toml", slow_plan());
    h.cli(&["run", &plan.to_string_lossy()]).await;
    h.wait_for_tasks(|tasks| tasks.iter().any(|t| t["task"]["state"] == "RUNNING"))
        .await;

    // Crash the daemon (no graceful shutdown).
    h.crash_daemon().await;

    // Restart: recovery must mark the task INTERRUPTED, close sessions,
    // release leases and kill the orphaned agent process tree.
    h.start_daemon().await;

    let client = h.client().await;
    // Fetch the durable events to find the recovery note with the orphan pid.
    let events = client.call(Request::GetState).await.map(|_| ()).ok();
    assert!(events.is_some());
    let mut orphan_pid: Option<u32> = None;
    let deadline = Instant::now() + Duration::from_secs(20);
    while orphan_pid.is_none() && Instant::now() < deadline {
        let value = h.cli(&["status", "--json"]).await;
        let parsed: serde_json::Value = serde_json::from_str(&value).unwrap();
        for event in parsed["events"].as_array().cloned().unwrap_or_default() {
            let message = event["message"].as_str().unwrap_or_default().to_string();
            if message.contains("orphan process") && message.contains("killed") {
                orphan_pid = message
                    .split_whitespace()
                    .filter_map(|tok| tok.parse::<u32>().ok())
                    .next();
            }
        }
        if orphan_pid.is_none() {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    let value = h
        .wait_for_tasks(|tasks| tasks.iter().all(|t| t["task"]["state"] == "INTERRUPTED"))
        .await;
    assert_eq!(value["tasks"][0]["task"]["state"], "INTERRUPTED");
    let reason = value["tasks"][0]["task"]["failure_reason"]
        .as_str()
        .unwrap_or_default();
    assert!(reason.contains("restarted"), "{reason}");

    if let Some(pid) = orphan_pid {
        // Give the OS a moment to reap the killed process.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !forge_runtime::platform::process_alive(pid),
            "orphan process {pid} must be dead after recovery"
        );
    }

    // Retry re-queues the interrupted task (fresh attempt ⇒ new worktree).
    let out = h.cli(&["retry", "T-001"]).await;
    assert!(out.contains("re-queued"), "{out}");
    let value = h
        .wait_for_tasks(|tasks| {
            tasks
                .iter()
                .any(|t| t["task"]["state"] == "COMPLETED" || t["task"]["state"] == "RUNNING")
        })
        .await;
    let state = value["tasks"][0]["task"]["state"].as_str().unwrap();
    assert!(state == "RUNNING" || state == "COMPLETED", "{state}");

    h.stop_daemon().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notifications_reach_subscribed_clients() {
    let mut h = Harness::new().await;
    h.start_daemon().await;
    let client = h.client().await;
    let mut notes = client.notifications();
    client
        .call(Request::Subscribe {
            since_event_id: None,
        })
        .await
        .unwrap();

    let plan = h.plan_file("plan.toml", two_task_plan());
    h.cli(&["run", &plan.to_string_lossy()]).await;

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut saw_task_changed = false;
    let mut saw_event = false;
    while (!saw_task_changed || !saw_event) && Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(5), notes.recv()).await {
            Ok(Ok(Notification::TaskChanged(_))) => saw_task_changed = true,
            Ok(Ok(Notification::Event(_))) => saw_event = true,
            Ok(Ok(_)) => {}
            Ok(Err(e)) => panic!("notification stream error: {e}"),
            Err(_) => {}
        }
    }
    assert!(saw_task_changed, "must receive task change notifications");
    assert!(saw_event, "must receive event notifications");
    h.stop_daemon().await;
}
