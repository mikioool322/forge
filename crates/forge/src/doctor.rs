//! `forge doctor`: honest environment diagnostics.
//!
//! Every check reports what it actually observed; nothing is hardcoded to
//! "PASS". Exit code is 1 when any check fails.

use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use forge_core::model::{now_ms, TaskState};
use forge_protocol::messages::{Request, Response};

use crate::cli::DoctorArgs;
use crate::ipc;
use crate::util::human_bytes;
use crate::CliCtx;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    Warn,
    Fail,
    Skip,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Fail => "fail",
            Status::Skip => "skip",
        }
    }
}

struct Check {
    name: String,
    status: Status,
    detail: String,
}

#[derive(Default)]
struct Report {
    checks: Vec<Check>,
}

impl Report {
    fn push(&mut self, name: &str, status: Status, detail: impl Into<String>) {
        self.checks.push(Check {
            name: name.to_string(),
            status,
            detail: detail.into(),
        });
    }

    fn failed(&self) -> bool {
        self.checks.iter().any(|c| c.status == Status::Fail)
    }
}

pub async fn run(ctx: &CliCtx, args: &DoctorArgs) -> anyhow::Result<ExitCode> {
    let mut report = Report::default();

    // --- platform ---------------------------------------------------------
    report.push(
        "platform",
        Status::Ok,
        format!(
            "{} {} / {}",
            std::env::consts::OS,
            std::env::consts::ARCH,
            std::env::consts::FAMILY
        ),
    );

    // --- configuration ----------------------------------------------------
    match &ctx.cfg_check {
        Ok(()) => report.push(
            "config",
            Status::Ok,
            ctx.paths.config_path.display().to_string(),
        ),
        Err(e) => report.push("config", Status::Fail, e.clone()),
    }

    // --- data directory ---------------------------------------------------
    match writable_probe(&ctx.paths.data_dir) {
        Ok(()) => report.push(
            "data dir",
            Status::Ok,
            format!("{} (writable)", ctx.paths.data_dir.display()),
        ),
        Err(e) => report.push("data dir", Status::Fail, e),
    }

    // --- database ---------------------------------------------------------
    let probe = forge_store::probe_database(&ctx.paths.db_path);
    if !probe.exists {
        report.push(
            "database",
            Status::Warn,
            format!("not initialized yet ({})", ctx.paths.db_path.display()),
        );
    } else if probe.integrity != "ok" {
        report.push(
            "database",
            Status::Fail,
            format!("integrity: {}", probe.integrity),
        );
    } else {
        let version = probe.version.unwrap_or(-1);
        if version > probe.latest {
            report.push(
                "database",
                Status::Fail,
                format!(
                    "schema v{version} is newer than this binary (v{}); upgrade forge",
                    probe.latest
                ),
            );
        } else if version < probe.latest {
            report.push(
                "database",
                Status::Warn,
                format!(
                    "schema v{version} < v{}; migrations will run on daemon start ({}, {})",
                    probe.latest,
                    human_bytes(probe.size_bytes),
                    probe.journal_mode.as_deref().unwrap_or("?")
                ),
            );
        } else {
            report.push(
                "database",
                Status::Ok,
                format!(
                    "schema v{version}, {}, {}, integrity ok",
                    probe.journal_mode.as_deref().unwrap_or("?"),
                    human_bytes(probe.size_bytes)
                ),
            );
        }
    }

    // --- daemon -----------------------------------------------------------
    match ipc::connect(&ctx.paths, "forge-doctor").await {
        Ok(client) => match client.call(Request::Ping).await {
            Ok(Response::Pong {
                protocol,
                daemon_version,
                uptime_ms,
            }) => {
                let endpoint = std::fs::read_to_string(ctx.paths.endpoint_file())
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                report.push(
                    "daemon",
                    Status::Ok,
                    format!(
                        "running: v{daemon_version} protocol {protocol}, up {}s, {}",
                        uptime_ms / 1000,
                        endpoint
                    ),
                );
                if let Ok(Response::State(snapshot)) = client.call(Request::GetState).await {
                    report.push(
                        "tasks",
                        if snapshot.daemon.running_tasks > 0 {
                            Status::Ok
                        } else {
                            Status::Skip
                        },
                        format!(
                            "{} running, {} total; agents online: {}",
                            snapshot.daemon.running_tasks,
                            snapshot.tasks.len(),
                            snapshot
                                .agents
                                .iter()
                                .filter(|a| a.probe.is_ok() && a.enabled)
                                .count()
                        ),
                    );
                }
            }
            Ok(other) => report.push(
                "daemon",
                Status::Warn,
                format!("unexpected response {other:?}"),
            ),
            Err(e) => report.push(
                "daemon",
                Status::Warn,
                format!("endpoint present but broken: {e}"),
            ),
        },
        Err(_) => report.push(
            "daemon",
            Status::Warn,
            "not running (start: forge daemon start --background)".to_string(),
        ),
    }

    // --- git --------------------------------------------------------------
    match forge_runtime::platform::resolve_executable("git") {
        Some(git) => {
            let (code, stdout, stderr) =
                run_cmd(&git, &["--version"], None, Duration::from_secs(5));
            let version = stdout.trim().to_string();
            if code == Some(0) {
                report.push("git", Status::Ok, version);
            } else {
                report.push(
                    "git",
                    Status::Fail,
                    format!("`git --version` failed: {}", stderr.trim()),
                );
            }
        }
        None => report.push("git", Status::Fail, "git not found on PATH".to_string()),
    }

    // --- git worktree support (real probe in a scratch repository) ---------
    match worktree_probe() {
        Ok(detail) => report.push("git worktrees", Status::Ok, detail),
        Err(e) => report.push("git worktrees", Status::Fail, e),
    }

    // --- worktree root ----------------------------------------------------
    match writable_probe(&ctx.paths.worktrees_dir) {
        Ok(()) => report.push(
            "worktree root",
            Status::Ok,
            ctx.paths.worktrees_dir.display().to_string(),
        ),
        Err(e) => report.push("worktree root", Status::Fail, e),
    }

    // --- current directory / project registration -------------------------
    let cwd = std::env::current_dir().unwrap_or_default();
    match forge_git::GitRepo::detect(&cwd) {
        Ok(repo) => {
            let root = repo.root().to_string_lossy().to_string();
            let registered = project_registered(&ctx.paths, &root);
            match registered {
                Ok(Some(name)) => report.push("project", Status::Ok, format!("{name} at {root}")),
                Ok(None) => report.push(
                    "project",
                    Status::Warn,
                    format!("{root} is not registered (run `forge init`)"),
                ),
                Err(e) => report.push("project", Status::Warn, e.to_string()),
            }
        }
        Err(_) => report.push(
            "project",
            Status::Skip,
            format!("{} is not inside a Git repository", cwd.display()),
        ),
    }

    // --- agents -----------------------------------------------------------
    if ctx.cfg.agents.is_empty() {
        report.push(
            "agents",
            Status::Warn,
            "no agents configured (add [[agents]] entries to config.toml)".to_string(),
        );
    } else {
        for spec in &ctx.cfg.agents {
            let name = format!("agent:{}", spec.id);
            if !spec.enabled {
                report.push(&name, Status::Skip, format!("{} (disabled)", spec.kind));
                continue;
            }
            let s = spec.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                forge_runtime::adapter::adapter_for(s.kind).probe(&s)
            })
            .await;
            match outcome {
                Ok(outcome) => {
                    let status = match &outcome.status {
                        forge_core::ProbeStatus::Ok { .. } => Status::Ok,
                        forge_core::ProbeStatus::Missing { .. } => Status::Warn,
                        _ => Status::Warn,
                    };
                    report.push(
                        &name,
                        status,
                        format!(
                            "{} {} | {} | auth is verified at run time",
                            spec.kind,
                            spec.command,
                            outcome.status.label()
                        ) + &outcome
                            .status
                            .detail()
                            .map(|d| format!(" ({d})"))
                            .or_else(|| outcome.version.clone().map(|v| format!(" ({v})")))
                            .unwrap_or_default(),
                    );
                }
                Err(e) => report.push(&name, Status::Warn, format!("probe panicked: {e}")),
            }
        }
    }

    // --- runtime files ----------------------------------------------------
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for (label, path) in [
            ("runtime dir", ctx.paths.run_dir.clone()),
            ("endpoint file", ctx.paths.endpoint_file()),
            ("token file", ctx.paths.token_file()),
        ] {
            if path.exists() {
                let mode = std::fs::metadata(&path)
                    .map(|m| m.permissions().mode() & 0o777)
                    .unwrap_or(0);
                let ok = mode & 0o077 == 0;
                report.push(
                    &format!("perms:{label}"),
                    if ok { Status::Ok } else { Status::Warn },
                    format!("{} mode {:03o}", path.display(), mode),
                );
            }
        }
    }
    #[cfg(not(unix))]
    {
        let endpoint = std::fs::read_to_string(ctx.paths.endpoint_file())
            .unwrap_or_default()
            .trim()
            .to_string();
        if endpoint.starts_with("tcp:") && !ctx.paths.token_file().exists() {
            report.push(
                "ipc token",
                Status::Warn,
                "TCP endpoint without token file; is the daemon mid-start?".to_string(),
            );
        } else if !endpoint.is_empty() {
            report.push("ipc", Status::Ok, format!("{endpoint} (loopback + token)"));
        }
    }

    // --- logs -------------------------------------------------------------
    let logs = dir_stats(&ctx.paths.logs_dir);
    report.push(
        "logs",
        Status::Ok,
        format!(
            "{} files, {} in {}",
            logs.0,
            human_bytes(logs.1),
            ctx.paths.logs_dir.display()
        ),
    );

    // --- tasks needing attention -----------------------------------------
    if let Ok(StateInfo {
        interrupted,
        failed,
    }) = task_attention(&ctx.paths)
    {
        if interrupted + failed > 0 {
            report.push(
                "recovery",
                Status::Warn,
                format!(
                    "{interrupted} interrupted, {failed} failed task(s); inspect with `forge status`, retry with `forge retry <task>`"
                ),
            );
        }
    }

    // --- output -----------------------------------------------------------
    if args.json {
        let items: Vec<serde_json::Value> = report
            .checks
            .iter()
            .map(|c| {
                serde_json::json!({
                    "check": c.name,
                    "status": c.status.label(),
                    "detail": c.detail,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
    } else {
        let width = report
            .checks
            .iter()
            .map(|c| c.name.len())
            .max()
            .unwrap_or(10);
        for check in &report.checks {
            println!(
                "{:<width$}  {:<5} {}",
                check.name,
                check.status.label(),
                check.detail,
                width = width
            );
        }
        let failed = report
            .checks
            .iter()
            .filter(|c| c.status == Status::Fail)
            .count();
        let warned = report
            .checks
            .iter()
            .filter(|c| c.status == Status::Warn)
            .count();
        println!("{}", "-".repeat(width + 40));
        println!(
            "{} checks, {} failed, {} warnings",
            report.checks.len(),
            failed,
            warned
        );
    }
    Ok(if report.failed() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

struct StateInfo {
    interrupted: usize,
    failed: usize,
}

fn task_attention(paths: &forge_core::ForgePaths) -> anyhow::Result<StateInfo> {
    if !paths.db_path.exists() {
        return Ok(StateInfo {
            interrupted: 0,
            failed: 0,
        });
    }
    let db = forge_store::DirectDb::open_readonly(&paths.db_path)?;
    let conn = db.get();
    let interrupted: i64 = conn.query_row(
        "SELECT COUNT(*) FROM tasks WHERE state = ?1",
        rusqlite::params![TaskState::Interrupted.as_str()],
        |r| r.get(0),
    )?;
    let failed: i64 = conn.query_row(
        "SELECT COUNT(*) FROM tasks WHERE state = ?1",
        rusqlite::params![TaskState::Failed.as_str()],
        |r| r.get(0),
    )?;
    let _ = now_ms();
    Ok(StateInfo {
        interrupted: interrupted as usize,
        failed: failed as usize,
    })
}

fn project_registered(
    paths: &forge_core::ForgePaths,
    root: &str,
) -> anyhow::Result<Option<String>> {
    if !paths.db_path.exists() {
        return Ok(None);
    }
    let db = forge_store::DirectDb::open_readonly(&paths.db_path)?;
    let conn = db.get();
    let name: Option<String> = conn
        .query_row(
            "SELECT name FROM projects WHERE root = ?1",
            rusqlite::params![root],
            |r| r.get(0),
        )
        .ok();
    Ok(name)
}

fn writable_probe(dir: &PathBuf) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let probe = dir.join(".forge-write-probe");
    std::fs::write(&probe, b"probe")
        .map_err(|e| format!("cannot write in {}: {e}", dir.display()))?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

fn dir_stats(dir: &PathBuf) -> (usize, u64) {
    let mut count = 0usize;
    let mut bytes = 0u64;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    count += 1;
                    bytes += meta.len();
                }
            }
        }
    }
    (count, bytes)
}

fn run_cmd(
    program: &PathBuf,
    args: &[&str],
    cwd: Option<&PathBuf>,
    timeout: Duration,
) -> (Option<i32>, String, String) {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return (None, String::new(), e.to_string()),
    };
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return (None, String::new(), "timed out".to_string());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return (None, String::new(), e.to_string()),
        }
    }
    match child.wait_with_output() {
        Ok(out) => (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        ),
        Err(e) => (None, String::new(), e.to_string()),
    }
}

/// Create a scratch repository and verify that `git worktree` works.
fn worktree_probe() -> Result<String, String> {
    let Some(git) = forge_runtime::platform::resolve_executable("git") else {
        return Err("git not found".to_string());
    };
    let base = std::env::temp_dir().join(format!("forge-doctor-{}", std::process::id()));
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).map_err(|e| e.to_string())?;
    let (code, _, stderr) = run_cmd(
        &git,
        &["init", "-b", "main"],
        Some(&repo),
        Duration::from_secs(10),
    );
    if code != Some(0) {
        let _ = std::fs::remove_dir_all(&base);
        return Err(format!("git init failed: {}", stderr.trim()));
    }
    std::fs::write(repo.join("README.md"), "probe\n").map_err(|e| e.to_string())?;
    for args in [
        vec!["add", "README.md"],
        vec![
            "-c",
            "user.name=doctor",
            "-c",
            "user.email=doctor@forge",
            "commit",
            "-m",
            "probe",
        ],
    ] {
        let (code, _, stderr) = run_cmd(&git, &args, Some(&repo), Duration::from_secs(10));
        if code != Some(0) {
            let _ = std::fs::remove_dir_all(&base);
            return Err(format!("git {:?} failed: {}", args, stderr.trim()));
        }
    }
    let wt = base.join("wt");
    let (code, _, stderr) = run_cmd(
        &git,
        &[
            "worktree",
            "add",
            "-b",
            "probe-branch",
            &wt.to_string_lossy(),
            "HEAD",
        ],
        Some(&repo),
        Duration::from_secs(15),
    );
    let result = if code == Some(0) && wt.join("README.md").exists() {
        Ok("worktree add/list works (scratch repo)".to_string())
    } else {
        Err(format!("git worktree add failed: {}", stderr.trim()))
    };
    let _ = std::fs::remove_dir_all(&base);
    result
}
