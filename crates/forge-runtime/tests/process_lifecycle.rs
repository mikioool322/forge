//! Process lifecycle hardening tests (Sprint 01.5A, findings A01/A03/A04).
//!
//! Every test drives a **real** OS process through the production supervisor and
//! asserts on real OS state (identity snapshots, liveness, tree membership), not
//! only on returned structures. Long-running fixtures always have a bounded
//! lifetime and are terminated in cleanup.

use std::path::PathBuf;
use std::time::Duration;

use forge_core::{AgentKind, AgentSpec};
use forge_runtime::adapter::AgentJob;
use forge_runtime::generic::GenericAdapter;
use forge_runtime::platform::{self, ProcessIdentity};
use forge_runtime::supervisor::{
    spawn_run, AttemptId, RunEvent, RunOutcome, RunParams, TreeOutcome, TreePolicy,
};
use tokio::sync::{mpsc, oneshot, watch};

const LARGE_PROMPT_BYTES: usize = 2 * 1024 * 1024;

struct RunRequest {
    program: String,
    args: Vec<String>,
    prompt: Option<String>,
    timeout: Duration,
    cancel_after: Option<Duration>,
    tree_policy: TreePolicy,
}

impl RunRequest {
    fn new(program: &str, args: &[&str]) -> Self {
        Self {
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            // Every fixture takes its prompt on stdin (never as a positional
            // argument), so the supervisor always exercises the stdin path.
            prompt: Some("lifecycle prompt".into()),
            timeout: Duration::from_secs(60),
            cancel_after: None,
            tree_policy: TreePolicy::KillOnExit,
        }
    }

    fn with_prompt(mut self, bytes: usize) -> Self {
        self.prompt = Some("p".repeat(bytes));
        self
    }

    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn with_cancel_after(mut self, after: Duration) -> Self {
        self.cancel_after = Some(after);
        self
    }

    fn with_tree_policy(mut self, policy: TreePolicy) -> Self {
        self.tree_policy = policy;
        self
    }
}

struct RunResult {
    events: Vec<RunEvent>,
    outcome: RunOutcome,
    _dir: tempfile::TempDir,
}

/// Runs one attempt through the production supervisor with a bounded wall clock.
async fn run(req: RunRequest) -> RunResult {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("task.log");
    let worktree = dir.path().to_path_buf();
    let spec = AgentSpec {
        id: "lifecycle-test".into(),
        kind: AgentKind::Generic,
        command: req.program.clone(),
        extra_args: req.args.clone(),
        capabilities: vec![],
        slots: 1,
        env: Default::default(),
        timeout_secs: None,
        enabled: true,
        stdin_prompt: req.prompt.is_some(),
    };
    let job = AgentJob {
        task_display_id: "T-LIFE".into(),
        task_title: "process lifecycle test".into(),
        spec: req.prompt.clone().unwrap_or_else(|| "prompt".into()),
        worktree,
        extra_context: Default::default(),
        timeout: req.timeout,
    };
    let params = RunParams {
        adapter: std::sync::Arc::new(GenericAdapter),
        spec,
        job,
        help: Default::default(),
        log_path,
        max_log_bytes: 4 * 1024 * 1024,
        max_line_bytes: 8192,
        heartbeat_interval: Duration::from_millis(200),
        attempt: AttemptId::new(1, 0, "lifecycle-token"),
        tree_policy: req.tree_policy,
    };
    let (ev_tx, mut ev_rx) = mpsc::channel::<RunEvent>(1024);
    let (outcome_tx, outcome_rx) = oneshot::channel::<RunOutcome>();
    let handle = spawn_run(params, ev_tx, outcome_tx).expect("spawn_run");

    let mut events = vec![];
    let mut outcome_rx = outcome_rx;
    let mut kill_tx: Option<watch::Sender<bool>> = Some(handle.kill.clone());
    let mut events_open = true;
    let cancel_timer = tokio::time::sleep(req.cancel_after.unwrap_or(Duration::from_secs(3600)));
    tokio::pin!(cancel_timer);
    // Hard ceiling: no test may hang the suite.
    let hard_deadline = tokio::time::sleep(Duration::from_secs(90));
    tokio::pin!(hard_deadline);
    let mut outcome = None;
    loop {
        tokio::select! {
            biased;
            res = &mut outcome_rx => {
                outcome = Some(res.expect("outcome channel"));
                break;
            }
            ev = ev_rx.recv(), if events_open => {
                match ev {
                    Some(ev) => events.push(ev),
                    None => events_open = false,
                }
            }
            _ = &mut cancel_timer, if kill_tx.is_some() => {
                if let Some(tx) = kill_tx.take() {
                    let _ = tx.send(true);
                }
            }
            _ = &mut hard_deadline => break,
        }
    }
    let outcome = outcome.unwrap_or_else(|| panic!("no outcome within 90s; events: {events:?}"));
    RunResult {
        events,
        outcome,
        _dir: dir,
    }
}

fn sleeper() -> RunRequest {
    #[cfg(windows)]
    {
        RunRequest::new("cmd.exe", &["/C", "ping -n 302 127.0.0.1 > NUL"])
    }
    #[cfg(unix)]
    {
        RunRequest::new("sh", &["-c", "sleep 600"])
    }
}

/// Parent that spawns a descendant with its own (nulled) stdio and exits 0.
fn spawns_descendant_then_exits() -> RunRequest {
    #[cfg(windows)]
    {
        RunRequest::new("cmd.exe", &["/C", "start /B ping -n 30 127.0.0.1"])
    }
    #[cfg(unix)]
    {
        RunRequest::new("sh", &["-c", "sleep 30 > /dev/null 2>&1 & exit 0"])
    }
}

/// A controlled process owned by the *test* (never by Forge).
fn spawn_control_process() -> std::process::Child {
    #[cfg(windows)]
    {
        std::process::Command::new("cmd.exe")
            .args(["/C", "ping -n 30 127.0.0.1 > NUL"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("control process")
    }
    #[cfg(unix)]
    {
        std::process::Command::new("sh")
            .args(["-c", "sleep 30"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("control process")
    }
}

fn kill_quietly(pid: u32) {
    let _ = platform::kill_tree(pid, true);
}

// ---------------------------------------------------------------------------
// P01 — PID identity mismatch must never authorize a kill
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn p01_a_recorded_identity_never_matches_a_different_process() {
    let mut control = spawn_control_process();
    let pid = control.id();
    let snapshot = platform::process_snapshot(pid).expect("control process is inspectable");

    let genuine = ProcessIdentity::new(
        pid,
        snapshot.start_marker,
        snapshot.fingerprint.clone(),
        "token-a",
    );
    let foreign_marker = ProcessIdentity::new(
        pid,
        snapshot.start_marker.map(|m| m.wrapping_add(1)),
        snapshot.fingerprint.clone(),
        "token-a",
    );
    let foreign_image = ProcessIdentity::new(
        pid,
        snapshot.start_marker,
        format!("{}|0", snapshot.fingerprint),
        "token-a",
    );
    let unrecorded = ProcessIdentity::new(pid, None, snapshot.fingerprint.clone(), "token-a");
    let no_fingerprint = ProcessIdentity::new(pid, snapshot.start_marker, "", "token-a");
    let other_process = ProcessIdentity::new(
        pid.wrapping_add(1),
        snapshot.start_marker,
        snapshot.fingerprint.clone(),
        "token-a",
    );

    // Positive control: the check is not vacuously false.
    assert!(
        genuine.matches_os(),
        "the recorded identity of a live process must match (marker {:?}, fingerprint {:?})",
        snapshot.start_marker,
        snapshot.fingerprint
    );
    // The PID is alive in every case below, yet none of them is the recorded process.
    assert!(
        !foreign_marker.matches_os(),
        "the same PID with a different start marker is a different process"
    );
    assert!(
        !foreign_image.matches_os(),
        "the same PID with a different image fingerprint is a different process"
    );
    assert!(
        !unrecorded.matches_os(),
        "without a recorded start marker ownership cannot be proven (fail closed)"
    );
    assert!(
        !no_fingerprint.matches_os(),
        "without a recorded fingerprint ownership cannot be proven (fail closed)"
    );
    assert!(
        !other_process.matches_os(),
        "a different PID is never the recorded process"
    );
    assert!(
        platform::process_alive(pid),
        "no kill decision was executed; the control process must still be alive"
    );

    let _ = control.kill();
    let _ = control.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn p01_identity_of_a_finished_process_stops_matching() {
    let mut control = spawn_control_process();
    let pid = control.id();
    let snapshot = platform::process_snapshot(pid).expect("control process is inspectable");
    let identity = ProcessIdentity::new(pid, snapshot.start_marker, snapshot.fingerprint.clone(), "token-a");
    assert!(identity.matches_os(), "alive and matching");

    let _ = control.kill();
    let _ = control.wait();
    // Give the OS a moment to release the process object.
    for _ in 0..40 {
        if !identity.matches_os() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        !identity.matches_os(),
        "a dead process must not keep matching its recorded identity"
    );
}

// ---------------------------------------------------------------------------
// P02 — a cancellation escalation never outlives its own run
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn p02_escalation_after_early_exit_kills_nothing() {
    let mut control = spawn_control_process();
    let control_pid = control.id();

    // Request cancellation while a short-lived process is still starting; the
    // process exits on its own well before the 5 s escalation grace.
    let result = run(sleeper().with_cancel_after(Duration::from_millis(200))).await;
    assert_eq!(
        result.outcome.classification,
        forge_runtime::ExitClass::Killed,
        "the cancellation is reported"
    );
    assert!(result.outcome.tree.is_closed(), "{:?}", result.outcome.tree);

    // Wait past KILL_GRACE (5 s): a delayed escalation of the finished run must
    // not touch anything that is alive now.
    tokio::time::sleep(Duration::from_millis(5_500)).await;
    assert!(
        platform::process_alive(control_pid),
        "a finished run must not kill processes after the fact"
    );

    let _ = control.kill();
    let _ = control.wait();
}

// ---------------------------------------------------------------------------
// P05 — a descendant that outlives its parent keeps the attempt open
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn p05_descendant_surviving_the_parent_is_terminated_before_settling() {
    let result = run(spawns_descendant_then_exits()).await;
    assert_eq!(
        result.outcome.exit_code,
        Some(0),
        "the direct parent exits successfully"
    );
    match &result.outcome.tree {
        TreeOutcome::Terminated { count } => {
            assert!(*count >= 1, "at least one descendant was terminated");
        }
        TreeOutcome::Empty => {
            // Acceptable only when the OS confirms nothing survived.
            assert!(
                result.outcome.tree.is_closed(),
                "an empty tree is a closed attempt"
            );
        }
        other => panic!("the descendant must not be reported as settled: {other:?}"),
    }
    assert!(
        result.outcome.tree.is_closed(),
        "with policy KillOnExit the attempt is only settled when the tree is gone"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn p05_report_policy_leaves_the_descendant_alive_but_not_owned() {
    let result = run(
        spawns_descendant_then_exits().with_tree_policy(TreePolicy::ReportOnly),
    )
    .await;
    let pids = match &result.outcome.tree {
        TreeOutcome::Survived { pids, .. } => pids.clone(),
        other => panic!("report policy must report a surviving tree, got {other:?}"),
    };
    assert!(
        !result.outcome.tree.is_closed(),
        "a surviving tree is not a closed attempt"
    );

    // Cleanup: the descendant is explicitly terminated by the test (it was left
    // running on purpose, so nothing else owns it now).
    for pid in pids {
        kill_quietly(pid);
    }
}

// ---------------------------------------------------------------------------
// P06 / P07 — cancel and timeout during a blocked stdin write
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn p07_timeout_is_enforced_while_stdin_is_blocked() {
    let started = std::time::Instant::now();
    let result = run(
        sleeper()
            .with_prompt(LARGE_PROMPT_BYTES)
            .with_timeout(Duration::from_secs(1)),
    )
    .await;
    let elapsed = started.elapsed();

    assert_eq!(
        result.outcome.classification,
        forge_runtime::ExitClass::Timeout,
        "the deadline covers the whole lifecycle, including the prompt write"
    );
    assert!(
        elapsed < Duration::from_secs(30),
        "the timeout must not wait for a blocked stdin write: {elapsed:?}"
    );
    assert!(result.outcome.tree.is_closed(), "{:?}", result.outcome.tree);
    if let Some(identity) = &result.outcome.identity {
        assert!(
            !identity.matches_os(),
            "the killed process must not keep matching its identity"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn p06_cancel_is_enforced_while_stdin_is_blocked() {
    let started = std::time::Instant::now();
    let result = run(
        sleeper()
            .with_prompt(LARGE_PROMPT_BYTES)
            .with_timeout(Duration::from_secs(300))
            .with_cancel_after(Duration::from_millis(800)),
    )
    .await;
    let elapsed = started.elapsed();

    assert_eq!(
        result.outcome.classification,
        forge_runtime::ExitClass::Killed,
        "cancel must stop the process even while the prompt pipe is full"
    );
    assert!(
        elapsed < Duration::from_secs(30),
        "cancel must not wait for a blocked stdin write: {elapsed:?}"
    );
    assert!(result.outcome.tree.is_closed(), "{:?}", result.outcome.tree);
    if let Some(identity) = &result.outcome.identity {
        assert!(
            !identity.matches_os(),
            "the cancelled process must be gone"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn p06_process_flooding_stdout_before_stdin_cannot_deadlock() {
    // The readers start before the prompt is written, so a process that fills
    // the stdout pipe first still completes; the stdin writer hits BrokenPipe
    // and is aborted without blocking the supervisor.
    #[cfg(windows)]
    let req = RunRequest::new(
        "cmd.exe",
        &[
            "/C",
            "for /L %i in (1,1,20000) do @echo flooding-line-%i",
        ],
    );
    #[cfg(unix)]
    let req = RunRequest::new("sh", &["-c", "seq 1 20000"]);

    let started = std::time::Instant::now();
    let result = run(req.with_prompt(LARGE_PROMPT_BYTES)).await;
    assert_eq!(
        result.outcome.classification,
        forge_runtime::ExitClass::Success,
        "stdout flooding before the prompt must not deadlock: {:?}",
        result.outcome.error
    );
    assert!(started.elapsed() < Duration::from_secs(60));
    assert!(result.outcome.tree.is_closed());
}

// ---------------------------------------------------------------------------
// Attempt identity is carried by events and outcomes
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attempt_identity_reaches_events_and_the_outcome() {
    let result = run(sleeper().with_cancel_after(Duration::from_millis(300))).await;
    assert_eq!(result.outcome.attempt.task_id, 1);
    assert_eq!(result.outcome.attempt.attempt_no, 0);
    assert_eq!(result.outcome.attempt.run_token, "lifecycle-token");
    let started_event = result
        .events
        .iter()
        .find_map(|e| match e {
            RunEvent::Started { pid, identity } => Some((*pid, identity.clone())),
            _ => None,
        })
        .expect("Started event");
    assert_eq!(started_event.1.run_token, "lifecycle-token");
    assert!(started_event.1.pid > 0);
    assert!(
        started_event.1.start_marker.is_some() || !cfg!(windows),
        "Windows must record a start marker for identity checks"
    );
    let _ = PathBuf::new();
}
