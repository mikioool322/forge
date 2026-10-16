//! Subprocess supervision.
//!
//! One supervisor task per running task:
//!
//! * spawns the prepared command in its own process group / Job Object,
//! * attaches the child to Forge's process-tree ownership **immediately** after
//!   spawn, before any work trigger is sent (the prompt is written later),
//! * streams stdout/stderr line-by-line into a size-limited log file on
//!   disk (the authoritative raw stream) — the readers start before stdin is
//!   written, so a process that floods stdout cannot deadlock the supervisor,
//! * writes the prompt to stdin **concurrently** with the deadline, the
//!   cancellation watch and `child.wait()`, so a blocked write can never bypass
//!   the timeout or a cancel,
//! * enforces wall-clock timeouts and graceful-then-forceful cancellation with
//!   an escalation timer that lives inside the run (no detached killer task can
//!   fire after the run ended),
//! * settles the whole process tree before the attempt is reported as finished,
//! * classifies the exit (success / failed / auth-required / timeout /
//!   killed) and reports a [`RunOutcome`].

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use forge_core::model::{now_ms, NormalizedEvent, StreamKind};
use forge_core::AgentSpec;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot, watch};

use crate::adapter::{AgentAdapter, AgentJob, ExitClass, HelpFlags, PreparedCommand};
use crate::error::RuntimeError;
use crate::platform::{self, ProcessIdentity, TreeGuard};

/// Grace period between the graceful signal and the forceful escalation.
const KILL_GRACE: Duration = Duration::from_secs(5);
/// How long we keep reading output after the child exited.
const DRAIN_GRACE: Duration = Duration::from_secs(3);
/// Number of stderr lines kept for exit classification.
const STDERR_TAIL: usize = 50;
/// How long the OS gets to reap the tree after the direct child exited.
const TREE_SETTLE_GRACE: Duration = Duration::from_secs(2);
/// How long the forceful tree termination gets to take effect.
const TREE_KILL_GRACE: Duration = Duration::from_secs(3);

/// Durable identity of one execution attempt (findings A02/A05/P09).
///
/// `task_id` alone is not enough: a retry starts a *new* attempt of the same
/// task, and events, timeouts or cancellations of the old attempt must never be
/// applied to the new one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AttemptId {
    pub task_id: i64,
    pub attempt_no: i64,
    /// Random per-attempt token; part of the durable record.
    pub run_token: String,
}

impl AttemptId {
    pub fn new(task_id: i64, attempt_no: i64, run_token: impl Into<String>) -> Self {
        Self {
            task_id,
            attempt_no,
            run_token: run_token.into(),
        }
    }

    pub fn display(&self) -> String {
        format!("{}/a{}", self.task_id, self.attempt_no)
    }
}

/// What happened to the process tree when the run settled (finding A03).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeOutcome {
    /// The OS confirmed that no process of this tree is alive.
    Empty,
    /// Descendants outlived the direct child and Forge terminated them.
    Terminated { count: usize },
    /// Descendants are still alive and were deliberately left running.
    Survived { count: usize, pids: Vec<u32> },
    /// The tree could not be inspected on this platform or the child never
    /// started: nothing is claimed about it.
    Unverified,
}

impl TreeOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            TreeOutcome::Empty => "empty",
            TreeOutcome::Terminated { .. } => "terminated",
            TreeOutcome::Survived { .. } => "survived",
            TreeOutcome::Unverified => "unverified",
        }
    }

    /// True when Forge can claim the attempt is fully closed.
    pub fn is_closed(&self) -> bool {
        matches!(self, TreeOutcome::Empty | TreeOutcome::Terminated { .. })
    }
}

/// Operator policy for descendants that outlive the direct child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreePolicy {
    /// Terminate the tree; a tree that still survives is a contract failure.
    KillOnExit,
    /// Report the surviving tree and leave it running.
    ReportOnly,
}

#[derive(Debug)]
pub enum RunEvent {
    Started {
        pid: u32,
        identity: ProcessIdentity,
    },
    Normalized(Box<NormalizedEvent>),
    Heartbeat,
    /// Number of output lines dropped because the engine was too slow.
    Dropped {
        count: u64,
    },
}

#[derive(Debug, Clone)]
pub struct RunOutcome {
    /// Attempt this outcome belongs to (stale outcomes must be ignored).
    pub attempt: AttemptId,
    pub exit_code: Option<i32>,
    pub classification: ExitClass,
    pub duration_ms: u64,
    pub stderr_tail: Vec<String>,
    pub dropped_lines: u64,
    pub log_path: PathBuf,
    pub log_bytes: u64,
    pub command_display: String,
    pub error: Option<String>,
    /// Verified identity of the process this attempt ran.
    pub identity: Option<ProcessIdentity>,
    /// Contract state of the process tree when the attempt settled.
    pub tree: TreeOutcome,
}

pub struct RunHandle {
    /// Attempt identity; the engine compares it against every arriving event.
    pub attempt: AttemptId,
    /// Filled in once the process is spawned (0 = not started yet).
    pub pid: Arc<AtomicU32>,
    /// Ownership handle for the whole process tree (A03).
    pub tree: Arc<TreeGuard>,
    /// Send `true` to request graceful termination of the process tree.
    pub kill: watch::Sender<bool>,
    /// Completes after the outcome has been sent.
    pub join: tokio::task::JoinHandle<()>,
}

pub struct RunParams {
    pub adapter: Arc<dyn AgentAdapter>,
    pub spec: AgentSpec,
    pub job: AgentJob,
    pub help: HelpFlags,
    pub log_path: PathBuf,
    pub max_log_bytes: u64,
    pub max_line_bytes: usize,
    pub heartbeat_interval: Duration,
    /// Identity of the attempt this run belongs to.
    pub attempt: AttemptId,
    /// Descendant policy applied when the run settles.
    pub tree_policy: TreePolicy,
}

/// Spawn the supervisor. Errors here mean the command could not even be
/// prepared/resolved; runtime failures come back through the outcome channel.
pub fn spawn_run(
    params: RunParams,
    events: mpsc::Sender<RunEvent>,
    outcome: oneshot::Sender<RunOutcome>,
) -> Result<RunHandle, RuntimeError> {
    let prepared = params
        .adapter
        .prepare(&params.spec, &params.job, &params.help)?;
    let (kill_tx, kill_rx) = watch::channel(false);
    let pid = Arc::new(AtomicU32::new(0));
    let pid_slot = pid.clone();
    let tree = Arc::new(TreeGuard::create());
    let tree_slot = tree.clone();
    let attempt = params.attempt.clone();
    let join = tokio::spawn(async move {
        let result = supervise(prepared, params, events, kill_rx, pid_slot, tree_slot).await;
        let _ = outcome.send(result);
    });
    Ok(RunHandle {
        attempt,
        pid,
        tree,
        kill: kill_tx,
        join,
    })
}

async fn supervise(
    prepared: PreparedCommand,
    params: RunParams,
    events: mpsc::Sender<RunEvent>,
    mut kill_rx: watch::Receiver<bool>,
    pid_slot: Arc<AtomicU32>,
    tree: Arc<TreeGuard>,
) -> RunOutcome {
    let started = Instant::now();
    let command_display = prepared.display.clone();
    let log_path = params.log_path.clone();
    let attempt = params.attempt.clone();

    let mut sink = match LogSink::open(&log_path, params.max_log_bytes).await {
        Ok(s) => s,
        Err(e) => {
            return RunOutcome {
                attempt,
                exit_code: None,
                classification: ExitClass::Failed,
                duration_ms: 0,
                stderr_tail: vec![],
                dropped_lines: 0,
                log_path,
                log_bytes: 0,
                command_display,
                error: Some(format!("cannot open log file: {e}")),
                identity: None,
                tree: TreeOutcome::Unverified,
            }
        }
    };
    let _ = sink
        .write_line(
            now_ms(),
            StreamKind::System,
            &format!("$ {}", command_display),
        )
        .await;

    // `.cmd`/`.bat` CLIs (e.g. npm-installed) can only be launched through
    // `cmd.exe /C`; the prompt is kept out of the batch argument list by the
    // adapters (stdin) so cmd.exe parsing can never mangle or inject it.
    let (program, args) =
        platform::maybe_wrap_script(prepared.program.clone(), prepared.args.clone());
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(&prepared.cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(if prepared.stdin_payload.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .kill_on_drop(true);
    for (k, v) in &prepared.env {
        command.env(k, v);
    }
    platform::configure_command(&mut command);

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("failed to spawn {}: {e}", prepared.program.display());
            let _ = sink.write_line(now_ms(), StreamKind::System, &msg).await;
            let _ = sink.flush().await;
            return RunOutcome {
                attempt,
                exit_code: None,
                classification: ExitClass::Failed,
                duration_ms: started.elapsed().as_millis() as u64,
                stderr_tail: vec![msg.clone()],
                dropped_lines: 0,
                log_path,
                log_bytes: sink.bytes(),
                command_display,
                error: Some(msg),
                identity: None,
                tree: TreeOutcome::Unverified,
            };
        }
    };

    // Take ownership of the whole tree before anything else can happen. The
    // agent cannot do real work yet: its prompt is written further below, after
    // the engine has committed the durable attempt record.
    let pid = child.id().unwrap_or(0);
    let snapshot = platform::process_snapshot(pid);
    let identity = ProcessIdentity::new(
        pid,
        snapshot.as_ref().and_then(|p| p.start_marker),
        snapshot.map(|p| p.fingerprint).unwrap_or_default(),
        attempt.run_token.clone(),
    );
    if let Err(e) = tree.attach(&child) {
        let _ = sink
            .write_line(
                now_ms(),
                StreamKind::System,
                &format!("warning: cannot attach the process tree container: {e}"),
            )
            .await;
        tracing::warn!(pid, error = %e, "process tree container unavailable");
    }
    pid_slot.store(pid, Ordering::SeqCst);
    let _ = events.try_send(RunEvent::Started {
        pid,
        identity: identity.clone(),
    });

    // Readers start immediately: a process that floods stdout before reading
    // stdin can no longer deadlock the supervisor (finding A04).
    let dropped = Arc::new(AtomicU64::new(0));
    let (line_tx, mut line_rx) = mpsc::channel::<RawLine>(512);
    let stdout_task = child.stdout.take().map(|out| {
        spawn_reader(
            out,
            StreamKind::Stdout,
            line_tx.clone(),
            dropped.clone(),
            params.max_line_bytes,
        )
    });
    let stderr_task = child.stderr.take().map(|err| {
        spawn_reader(
            err,
            StreamKind::Stderr,
            line_tx,
            dropped.clone(),
            params.max_line_bytes,
        )
    });

    // The prompt is written by a helper task *concurrently* with the loop
    // below, so the deadline, the cancel watch and `child.wait()` all stay
    // responsive while the pipe is full (findings A04/A05). The task is aborted
    // and joined before this function returns: no future is left steering a
    // live process.
    let stdin_task = spawn_stdin_writer(child.stdin.take(), prepared.stdin_payload.clone(), pid);

    let mut heartbeat = tokio::time::interval(params.heartbeat_interval);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let deadline = tokio::time::sleep(params.job.timeout);
    tokio::pin!(deadline);
    // Escalation timer of the current cancellation, owned by this loop: it is
    // cancelled by construction when the loop ends, so it can never fire
    // against a later attempt (finding A01).
    let mut escalation: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;

    let mut stderr_tail: VecDeque<String> = VecDeque::with_capacity(STDERR_TAIL);
    let mut killed = false;
    let mut timed_out = false;
    let mut lines_closed = false;
    let mut exit_status: Option<std::process::ExitStatus> = None;

    loop {
        tokio::select! {
            biased;
            changed = kill_rx.changed() => {
                if changed.is_ok() && *kill_rx.borrow() && !killed {
                    killed = true;
                    let _ = tree.terminate(true);
                    escalation = Some(Box::pin(tokio::time::sleep(KILL_GRACE)));
                }
            }
            _ = async {
                match escalation.as_mut() {
                    Some(timer) => timer.as_mut().await,
                    None => std::future::pending::<()>().await,
                }
            }, if escalation.is_some() => {
                escalation = None;
                if exit_status.is_none() {
                    let _ = tree.terminate(false);
                }
            }
            _ = &mut deadline, if !timed_out => {
                timed_out = true;
                let _ = sink
                    .write_line(now_ms(), StreamKind::System, &format!(
                        "timeout after {:?}: killing process tree", params.job.timeout))
                    .await;
                let _ = tree.terminate(false);
            }
            maybe_line = line_rx.recv(), if !lines_closed => {
                match maybe_line {
                    Some(raw) => {
                        if raw.stream == StreamKind::Stderr {
                            if stderr_tail.len() == STDERR_TAIL { stderr_tail.pop_front(); }
                            stderr_tail.push_back(raw.text.clone());
                        }
                        if sink.write_line(raw.ts, raw.stream, &raw.text).await.is_err() {
                            tracing::warn!(pid, "log write failed");
                        }
                        if let Some(ev) = params.adapter.classify_line(raw.stream, &raw.text) {
                            let _ = events.try_send(RunEvent::Normalized(Box::new(ev)));
                        }
                    }
                    // All reader tasks finished: disable this branch so it can
                    // never spin and starve `child.wait()`.
                    None => lines_closed = true,
                }
            }
            _ = heartbeat.tick() => {
                let _ = events.try_send(RunEvent::Heartbeat);
                let _ = sink.flush().await;
            }
            status = child.wait() => {
                match status {
                    Ok(s) => exit_status = Some(s),
                    Err(e) => {
                        let _ = sink.write_line(now_ms(), StreamKind::System,
                            &format!("wait failed: {e}")).await;
                    }
                }
                break;
            }
        }
    }
    let _ = lines_closed;

    // The process is gone: stop the stdin writer (a blocked write can never
    // complete now) and make sure it is really finished.
    if let Some(task) = stdin_task {
        task.abort();
        let _ = task.await;
    }

    // Drain remaining output.
    if let Some(t) = stdout_task {
        let _ = tokio::time::timeout(DRAIN_GRACE, t).await;
    }
    if let Some(t) = stderr_task {
        let _ = tokio::time::timeout(DRAIN_GRACE, t).await;
    }
    while let Ok(raw) = line_rx.try_recv() {
        if raw.stream == StreamKind::Stderr {
            if stderr_tail.len() == STDERR_TAIL {
                stderr_tail.pop_front();
            }
            stderr_tail.push_back(raw.text.clone());
        }
        let _ = sink.write_line(raw.ts, raw.stream, &raw.text).await;
        if let Some(ev) = params.adapter.classify_line(raw.stream, &raw.text) {
            let _ = events.try_send(RunEvent::Normalized(Box::new(ev)));
        }
    }
    let _ = sink.flush().await;

    let exit_code = exit_status.and_then(|s| s.code());
    let stderr_vec: Vec<String> = stderr_tail.into_iter().collect();
    let classification = if timed_out {
        ExitClass::Timeout
    } else if killed {
        ExitClass::Killed
    } else {
        params.adapter.classify_exit(exit_code, &stderr_vec)
    };

    // A run is only settled when the whole tree is accounted for (A03).
    let tree_outcome = settle_tree(&tree, params.tree_policy, &mut sink).await;

    let _ = sink
        .write_line(
            now_ms(),
            StreamKind::System,
            &format!(
                "process exited: code={exit_code:?} class={} tree={} duration_ms={}",
                classification.as_str(),
                tree_outcome.as_str(),
                started.elapsed().as_millis()
            ),
        )
        .await;
    let _ = sink.flush().await;

    RunOutcome {
        attempt,
        exit_code,
        classification,
        duration_ms: started.elapsed().as_millis() as u64,
        stderr_tail: stderr_vec,
        dropped_lines: dropped.load(Ordering::Relaxed),
        log_path,
        log_bytes: sink.bytes(),
        command_display,
        error: None,
        identity: Some(identity),
        tree: tree_outcome,
    }
}

/// Writes the prompt to the agent's stdin and closes the pipe.
///
/// Runs as a helper task so that a full pipe blocks this task only — never the
/// supervisor's deadline, cancellation or exit handling.
fn spawn_stdin_writer(
    stdin: Option<tokio::process::ChildStdin>,
    payload: Option<String>,
    pid: u32,
) -> Option<tokio::task::JoinHandle<()>> {
    let payload = payload?;
    let mut stdin = stdin?;
    Some(tokio::spawn(async move {
        match stdin.write_all(payload.as_bytes()).await {
            Ok(()) => {}
            Err(e) => tracing::warn!(pid, error = %e, "failed to write prompt to agent stdin"),
        }
        // Closing the pipe is what tells the agent the prompt is complete.
        let _ = stdin.shutdown().await;
    }))
}

/// Waits for the tree to disappear, terminating it when the policy says so.
async fn settle_tree(tree: &TreeGuard, policy: TreePolicy, sink: &mut LogSink) -> TreeOutcome {
    if tree.pid() == 0 {
        return TreeOutcome::Unverified;
    }
    let settle_deadline = Instant::now() + TREE_SETTLE_GRACE;
    while !tree.is_empty() && Instant::now() < settle_deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if tree.is_empty() {
        return TreeOutcome::Empty;
    }

    let count = tree.live_count().unwrap_or(0);
    let pids = tree.pids();
    let _ = sink
        .write_line(
            now_ms(),
            StreamKind::System,
            &format!(
                "process tree still has {} live process(es) after the child exited: {:?}",
                if count == 0 { 1 } else { count },
                pids
            ),
        )
        .await;

    match policy {
        TreePolicy::KillOnExit => {
            let _ = tree.terminate(false);
            let kill_deadline = Instant::now() + TREE_KILL_GRACE;
            while !tree.is_empty() && Instant::now() < kill_deadline {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            if tree.is_empty() {
                TreeOutcome::Terminated {
                    count: if count == 0 { 1 } else { count },
                }
            } else {
                TreeOutcome::Survived { count, pids }
            }
        }
        TreePolicy::ReportOnly => {
            // Leave the descendants running but stop claiming ownership.
            tree.detach();
            TreeOutcome::Survived { count, pids }
        }
    }
}

struct RawLine {
    stream: StreamKind,
    text: String,
    ts: i64,
}

fn spawn_reader<R: AsyncRead + Unpin + Send + 'static>(
    reader: R,
    stream: StreamKind,
    tx: mpsc::Sender<RawLine>,
    dropped: Arc<AtomicU64>,
    max_line_bytes: usize,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut reader = BufReader::new(reader);
        let mut buf: Vec<u8> = Vec::with_capacity(4096);
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf).await {
                Ok(0) => break,
                Ok(_) => {
                    let mut text = String::from_utf8_lossy(&buf)
                        .trim_end_matches(['\n', '\r'])
                        .to_string();
                    if text.len() > max_line_bytes {
                        let mut cut = max_line_bytes;
                        while cut > 0 && !text.is_char_boundary(cut) {
                            cut -= 1;
                        }
                        text.truncate(cut);
                        text.push_str(" [truncated]");
                    }
                    let ts = now_ms();
                    if tx.try_send(RawLine { stream, text, ts }).is_err() {
                        dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Err(_) => break,
            }
        }
    })
}

/// Size-limited, line-oriented log file with a single `.1` rotation.
struct LogSink {
    path: PathBuf,
    file: tokio::fs::File,
    written: u64,
    max_bytes: u64,
}

impl LogSink {
    async fn open(path: &std::path::Path, max_bytes: u64) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let written = tokio::fs::metadata(path)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            written,
            max_bytes,
        })
    }

    fn bytes(&self) -> u64 {
        self.written
    }

    async fn write_line(&mut self, ts: i64, stream: StreamKind, text: &str) -> std::io::Result<()> {
        if self.max_bytes > 0 && self.written >= self.max_bytes {
            self.rotate().await?;
        }
        let tag = match stream {
            StreamKind::Stdout => "out",
            StreamKind::Stderr => "err",
            StreamKind::System => "sys",
        };
        let line = format!("{ts}\t{tag}\t{text}\n");
        self.file.write_all(line.as_bytes()).await?;
        self.written += line.len() as u64;
        Ok(())
    }

    async fn rotate(&mut self) -> std::io::Result<()> {
        self.file.flush().await?;
        self.file.sync_all().await.ok();
        let rotated = self.path.with_extension("log.1");
        let _ = tokio::fs::remove_file(&rotated).await;
        tokio::fs::rename(&self.path, &rotated).await?;
        self.file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)
            .await?;
        self.written = 0;
        Ok(())
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn log_sink_rotates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("task.log");
        let mut sink = LogSink::open(&path, 100).await.unwrap();
        for i in 0..20 {
            sink.write_line(0, StreamKind::Stdout, &format!("line {i} padding padding"))
                .await
                .unwrap();
        }
        sink.flush().await.unwrap();
        let rotated = path.with_extension("log.1");
        assert!(rotated.exists(), "rotation must produce .log.1");
        assert!(std::fs::metadata(&path).unwrap().len() < 200);
    }

    #[tokio::test]
    async fn log_sink_appends_across_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("task.log");
        {
            let mut sink = LogSink::open(&path, 0).await.unwrap();
            sink.write_line(1, StreamKind::Stdout, "first")
                .await
                .unwrap();
            sink.flush().await.unwrap();
        }
        {
            let mut sink = LogSink::open(&path, 0).await.unwrap();
            assert!(sink.bytes() > 0);
            sink.write_line(2, StreamKind::Stderr, "second")
                .await
                .unwrap();
            sink.flush().await.unwrap();
        }
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("first") && content.contains("second"));
        assert!(content.contains("\terr\t"), "{content}");
    }

    #[test]
    fn attempt_identity_is_explicit() {
        let a = AttemptId::new(7, 0, "tok-a");
        let b = AttemptId::new(7, 1, "tok-b");
        assert_ne!(a, b, "a retry is a different attempt");
        assert_eq!(a.display(), "7/a0");
    }

    #[test]
    fn tree_outcome_contract() {
        assert!(TreeOutcome::Empty.is_closed());
        assert!(TreeOutcome::Terminated { count: 2 }.is_closed());
        assert!(!TreeOutcome::Survived {
            count: 1,
            pids: vec![42]
        }
        .is_closed());
        assert!(!TreeOutcome::Unverified.is_closed());
    }
}
