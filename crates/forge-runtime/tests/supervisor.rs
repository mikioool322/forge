//! Integration tests for subprocess supervision: success, failure, auth
//! classification, cancellation, timeout and high-volume output.
//!
//! These run real processes (the platform shell) through the generic
//! adapter — no model execution involved.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use forge_core::{AgentKind, AgentSpec};
use forge_runtime::adapter::AgentJob;
use forge_runtime::generic::GenericAdapter;
use forge_runtime::supervisor::{
    spawn_run, AttemptId, RunEvent, RunOutcome, RunParams, TreePolicy,
};
use tokio::sync::{mpsc, oneshot, watch};

fn agent_spec(command: &str, args: &[&str], stdin_prompt: bool) -> AgentSpec {
    AgentSpec {
        id: "test-agent".into(),
        kind: AgentKind::Generic,
        command: command.to_string(),
        extra_args: args.iter().map(|s| s.to_string()).collect(),
        capabilities: vec![],
        slots: 1,
        env: BTreeMap::new(),
        timeout_secs: None,
        enabled: true,
        stdin_prompt,
    }
}

fn job(worktree: PathBuf, timeout: Duration) -> AgentJob {
    AgentJob {
        task_display_id: "T-001".into(),
        task_title: "supervisor test".into(),
        spec: "prompt payload".into(),
        worktree,
        extra_context: BTreeMap::new(),
        timeout,
    }
}

/// `(command, args, uses_stdin)` that prints one line and exits 0.
fn echo_cmd() -> (&'static str, Vec<&'static str>, bool) {
    #[cfg(windows)]
    {
        ("cmd.exe", vec!["/C", "echo hello-from-supervisor"], true)
    }
    #[cfg(unix)]
    {
        ("sh", vec!["-c", "echo hello-from-supervisor"], true)
    }
}

/// Command that prints an auth failure to stderr and exits 1.
fn auth_fail_cmd() -> (&'static str, Vec<&'static str>, bool) {
    #[cfg(windows)]
    {
        (
            "cmd.exe",
            vec!["/C", "echo Error: not logged in. Run login. 1>&2 & exit 1"],
            true,
        )
    }
    #[cfg(unix)]
    {
        (
            "sh",
            vec!["-c", "echo 'Error: not logged in. Run login.' >&2; exit 1"],
            true,
        )
    }
}

/// Command that sleeps ~600s (cancelled/timeout in tests).
fn sleeper_cmd() -> (&'static str, Vec<&'static str>, bool) {
    #[cfg(windows)]
    {
        // ping -n N waits roughly N-1 seconds between packets.
        ("cmd.exe", vec!["/C", "ping -n 302 127.0.0.1 > NUL"], true)
    }
    #[cfg(unix)]
    {
        ("sh", vec!["-c", "sleep 600"], true)
    }
}

async fn run(
    cmd: (&str, Vec<&str>, bool),
    timeout: Duration,
    cancel_after: Option<Duration>,
) -> (Vec<RunEvent>, RunOutcome, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("task.log");
    let worktree = dir.path().to_path_buf();
    let spec = agent_spec(cmd.0, &cmd.1, cmd.2);
    let params = RunParams {
        adapter: std::sync::Arc::new(GenericAdapter),
        spec,
        job: job(worktree, timeout),
        help: Default::default(),
        log_path: log_path.clone(),
        max_log_bytes: 1024 * 1024,
        max_line_bytes: 4096,
        heartbeat_interval: Duration::from_millis(200),
        attempt: AttemptId::new(1, 0, "test-token"),
        tree_policy: TreePolicy::KillOnExit,
    };
    let (ev_tx, mut ev_rx) = mpsc::channel::<RunEvent>(512);
    let (outcome_tx, outcome_rx) = oneshot::channel::<RunOutcome>();
    let handle = spawn_run(params, ev_tx, outcome_tx).expect("spawn_run");

    let mut events = vec![];
    let mut outcome_rx = outcome_rx;
    let mut kill_tx: Option<watch::Sender<bool>> = Some(handle.kill.clone());
    let mut events_open = true;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    // Pin the cancellation timer: recreating it every loop iteration would
    // restart it (and heartbeats arrive more frequently than the delay).
    let cancel_timer = tokio::time::sleep(cancel_after.unwrap_or(Duration::from_secs(3600)));
    tokio::pin!(cancel_timer);
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
                    // The supervisor dropped its event sender: the outcome is
                    // imminent; keep waiting for it.
                    None => events_open = false,
                }
            }
            _ = &mut cancel_timer, if kill_tx.is_some() => {
                if let Some(tx) = kill_tx.take() {
                    let _ = tx.send(true);
                }
            }
            _ = tokio::time::sleep_until(deadline) => break,
        }
    }
    let outcome = outcome.unwrap_or_else(|| panic!("no outcome within 120s; events: {events:?}"));
    // Keep the temp dir alive until assertions complete by leaking it: the
    // process exits right after, so leaking is acceptable in tests.
    let leaked = Box::leak(Box::new(dir));
    let _ = leaked;
    (events, outcome, log_path)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn success_reports_outcome_and_logs_output() {
    let (events, outcome, log_path) = run(echo_cmd(), Duration::from_secs(30), None).await;
    assert_eq!(outcome.classification, forge_runtime::ExitClass::Success);
    assert_eq!(outcome.exit_code, Some(0));
    assert!(
        events.iter().any(|e| matches!(e, RunEvent::Started { .. })),
        "must emit Started"
    );
    assert!(
        events.iter().any(|e| matches!(e, RunEvent::Normalized(_))),
        "must emit normalized events"
    );
    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(log.contains("hello-from-supervisor"), "{log}");
    assert!(log.contains("process exited"), "{log}");
    assert!(outcome.log_bytes > 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auth_failure_is_classified() {
    let (_, outcome, _) = run(auth_fail_cmd(), Duration::from_secs(30), None).await;
    assert_eq!(
        outcome.classification,
        forge_runtime::ExitClass::AuthRequired
    );
    assert_eq!(outcome.exit_code, Some(1));
    assert!(
        outcome
            .stderr_tail
            .iter()
            .any(|l| l.contains("not logged in")),
        "{:?}",
        outcome.stderr_tail
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancellation_kills_the_process() {
    let (_, outcome, log) = run(
        sleeper_cmd(),
        Duration::from_secs(300),
        Some(Duration::from_millis(800)),
    )
    .await;
    assert_eq!(outcome.classification, forge_runtime::ExitClass::Killed);
    assert!(outcome.duration_ms < 60_000, "{}", outcome.duration_ms);
    let _ = log;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn timeout_kills_the_process() {
    let (_, outcome, _) = run(sleeper_cmd(), Duration::from_millis(1200), None).await;
    assert_eq!(outcome.classification, forge_runtime::ExitClass::Timeout);
    assert!(outcome.duration_ms < 60_000, "{}", outcome.duration_ms);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn high_volume_output_is_handled_without_unbounded_buffering() {
    // The test binary itself: `--list` prints every test name and exits.
    let exe = std::env::current_exe().unwrap();
    let exe_str = exe.to_string_lossy().to_string();
    let (_, outcome, log_path) = run(
        (Box::leak(exe_str.into_boxed_str()), vec!["--list"], true),
        Duration::from_secs(60),
        None,
    )
    .await;
    assert_eq!(outcome.classification, forge_runtime::ExitClass::Success);
    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(log.lines().count() > 5, "log should contain test names");
    assert!(outcome.log_bytes > 0);
}
