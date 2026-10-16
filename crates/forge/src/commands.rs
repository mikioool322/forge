//! CLI command implementations (client side of the daemon).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::process::{ExitCode, Stdio};

use anyhow::{bail, Context};
use forge_core::model::{PlanFile, TaskState};
use forge_core::ForgePaths;
use forge_protocol::messages::{ErrorCode, Request, Response, StateSnapshot};
use forge_store::repo;
use forge_store::DirectDb;

use crate::cli::{
    AgentsArgs, CancelArgs, DaemonStartArgs, DaemonStopArgs, InitArgs, LogsArgs, RetryArgs,
    RunArgs, StatusArgs,
};
use crate::ipc;
use crate::util::{format_ago, format_hms, human_bytes};
use crate::CliCtx;

// ---------------------------------------------------------------------------
// init
// ---------------------------------------------------------------------------

pub async fn init(ctx: &CliCtx, args: &InitArgs) -> anyhow::Result<ExitCode> {
    let start = match &args.root {
        Some(root) => root.clone(),
        None => std::env::current_dir().context("cannot determine current directory")?,
    };
    let repo_handle = forge_git::GitRepo::detect(&start).with_context(|| {
        format!(
            "{} is not inside a Git repository (run `forge init` in your repo root or pass --root)",
            start.display()
        )
    })?;
    let root = repo_handle.root().to_path_buf();
    let name = args.name.clone().unwrap_or_else(|| {
        root.file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "project".to_string())
    });
    if !args
        .name
        .as_deref()
        .unwrap_or("")
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        bail!("project name must match [A-Za-z0-9._-]+");
    }

    // Runtime directories.
    for dir in [
        &ctx.paths.data_dir,
        &ctx.paths.run_dir,
        &ctx.paths.logs_dir,
        &ctx.paths.worktrees_dir,
    ] {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }

    // Config file.
    let mut config_written = false;
    if args.force || !ctx.paths.config_path.exists() {
        if let Some(parent) = ctx.paths.config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            &ctx.paths.config_path,
            forge_core::config::DEFAULT_CONFIG_TEMPLATE,
        )
        .with_context(|| format!("cannot write {}", ctx.paths.config_path.display()))?;
        config_written = true;
    }

    // Register the project: through the daemon when it is running (it is the
    // single writer), otherwise directly through a short-lived connection.
    let root_str = root.to_string_lossy().to_string();
    let default_branch = repo_handle.current_branch().ok().flatten();
    let (registered_name, created, via) = match ipc::connect(&ctx.paths, "forge-init").await {
        Ok(client) => {
            let response = client
                .call(Request::RegisterProject {
                    root: root_str.clone(),
                    name: Some(name.clone()),
                })
                .await
                .context("daemon communication failed")?;
            match response {
                Response::Project { project, created } => (project.name, created, "daemon"),
                Response::Error { message, .. } => bail!("{message}"),
                other => bail!("unexpected daemon response: {other:?}"),
            }
        }
        Err(_) => {
            let mut db = DirectDb::open(&ctx.paths.db_path)?;
            let (project, created) =
                repo::register_project(db.conn(), &name, &root_str, default_branch.as_deref())?;
            (
                project.name,
                created,
                "direct database (daemon was not running)",
            )
        }
    };

    println!(
        "project:  {registered_name} ({})",
        if created {
            "registered"
        } else {
            "already registered"
        }
    );
    println!("root:     {}", root.display());
    println!(
        "config:   {}{}",
        ctx.paths.config_path.display(),
        if config_written { " (created)" } else { "" }
    );
    println!("data:     {}", ctx.paths.data_dir.display());
    println!("via:      {via}");
    println!("next:     forge doctor && forge daemon start && forge tui");
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------

fn content_request_id(contents: &str, project: &str) -> String {
    let mut h1 = DefaultHasher::new();
    contents.hash(&mut h1);
    project.hash(&mut h1);
    let mut h2 = DefaultHasher::new();
    "forge-plan".hash(&mut h2);
    contents.len().hash(&mut h2);
    project.hash(&mut h2);
    format!("plan-{:016x}{:016x}", h1.finish(), h2.finish())
}

pub async fn run_plan(ctx: &CliCtx, args: &RunArgs) -> anyhow::Result<ExitCode> {
    let contents = std::fs::read_to_string(&args.spec)
        .with_context(|| format!("cannot read plan {}", args.spec.display()))?;
    let mut plan: PlanFile = toml::from_str(&contents)
        .with_context(|| format!("cannot parse plan {}", args.spec.display()))?;
    if let Some(project) = &args.project {
        plan.project = project.clone();
    }
    forge_core::model::validate_plan(&plan).context("invalid plan")?;

    if args.dry_run {
        println!("plan: project={} tasks={}", plan.project, plan.tasks.len());
        for t in &plan.tasks {
            println!(
                "  {}  {}{}",
                t.key,
                t.title,
                if t.deps.is_empty() {
                    String::new()
                } else {
                    format!("  (deps: {})", t.deps.join(", "))
                }
            );
        }
        return Ok(ExitCode::SUCCESS);
    }

    let request_id = plan
        .request_id
        .clone()
        .unwrap_or_else(|| content_request_id(&contents, &plan.project));
    let client = ipc::connect(&ctx.paths, "forge-run")
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let response = client
        .call(Request::SubmitPlan {
            request_id: request_id.clone(),
            plan: plan.clone(),
        })
        .await
        .context("daemon communication failed")?;
    match response {
        Response::PlanAccepted {
            plan_id,
            created,
            tasks,
        } => {
            println!(
                "plan #{plan_id} {} (request {})",
                if created {
                    "accepted"
                } else {
                    "already known; replay"
                },
                request_id
            );
            println!("{:<7} {:<15} {:<10} TITLE", "TASK", "STATE", "AGENT");
            for view in &tasks {
                println!(
                    "{:<7} {:<15} {:<10} {}",
                    view.task.display_id(),
                    view.task.state,
                    view.task.agent_id.as_deref().unwrap_or("-"),
                    view.task.title
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Response::Error { code, message } => {
            if code == ErrorCode::NotFound {
                bail!("{message}");
            }
            bail!("daemon rejected the plan: {message}");
        }
        other => bail!("unexpected daemon response: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// status / agents / logs
// ---------------------------------------------------------------------------

fn display_state(state: TaskState) -> &'static str {
    match state {
        TaskState::Created => "PENDING",
        TaskState::Ready => "QUEUED",
        TaskState::Leased => "LEASED",
        TaskState::Running => "RUNNING",
        TaskState::ReviewPending => "REVIEW",
        TaskState::Blocked => "BLOCKED",
        TaskState::Completed => "COMPLETED",
        TaskState::Failed => "FAILED",
        TaskState::Cancelled => "CANCELLED",
        TaskState::Interrupted => "INTERRUPTED",
    }
}

fn task_time(view: &forge_protocol::TaskView) -> String {
    let now = forge_core::model::now_ms();
    if let Some(started) = view.task.started_at {
        let end = view.task.finished_at.unwrap_or(now);
        format_ago(started, end)
    } else {
        format_ago(view.task.created_at, now)
    }
}

async fn fetch_state(ctx: &CliCtx) -> anyhow::Result<StateSnapshot> {
    let client = ipc::connect(&ctx.paths, "forge-cli")
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let response = client
        .call(Request::GetState)
        .await
        .context("daemon communication failed")?;
    match response {
        Response::State(snapshot) => Ok(*snapshot),
        Response::Error { message, .. } => bail!("{message}"),
        other => bail!("unexpected daemon response: {other:?}"),
    }
}

pub async fn status(ctx: &CliCtx, args: &StatusArgs) -> anyhow::Result<ExitCode> {
    let snapshot = fetch_state(ctx).await?;
    let project_filter: Option<i64> = match &args.project {
        Some(name) => snapshot
            .projects
            .iter()
            .find(|p| &p.name == name)
            .map(|p| p.id)
            .or_else(|| {
                eprintln!("project {name:?} is not registered");
                None
            }),
        None => None,
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
        return Ok(ExitCode::SUCCESS);
    }
    let daemon = &snapshot.daemon;
    println!(
        "FORGE  v{}  protocol {}  up {}  endpoint {}  running {}/{}",
        daemon.version,
        daemon.protocol,
        format_ago(daemon.started_at, forge_core::model::now_ms()),
        daemon.endpoint,
        daemon.running_tasks,
        daemon.max_concurrency
    );
    println!("{}", "-".repeat(78));
    let mut tasks: Vec<&forge_protocol::TaskView> = snapshot
        .tasks
        .iter()
        .filter(|v| {
            project_filter
                .map(|pid| pid == v.task.project_id)
                .unwrap_or(true)
        })
        .collect();
    tasks.sort_by_key(|v| (v.task.project_id, v.task.seq_no));
    if tasks.is_empty() {
        println!("no tasks (submit a plan with `forge run <plan.toml>`)");
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "{:<7} {:<12} {:<10} {:>6}  {:<10} TITLE",
        "TASK", "STATE", "AGENT", "TIME", "DEPS"
    );
    let project_name = |pid: i64| {
        snapshot
            .projects
            .iter()
            .find(|p| p.id == pid)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| pid.to_string())
    };
    let multi_project = project_filter.is_none() && snapshot.projects.len() > 1;
    for view in tasks {
        let deps = if view.deps.is_empty() {
            "-".to_string()
        } else {
            view.deps
                .iter()
                .filter_map(|d| {
                    snapshot
                        .tasks
                        .iter()
                        .find(|v| v.task.id == *d)
                        .map(|v| v.task.display_id())
                })
                .collect::<Vec<_>>()
                .join(",")
        };
        let title = if multi_project {
            format!(
                "[{}] {}",
                project_name(view.task.project_id),
                view.task.title
            )
        } else {
            view.task.title.clone()
        };
        println!(
            "{:<7} {:<12} {:<10} {:>6}  {:<10} {}",
            view.task.display_id(),
            display_state(view.task.state),
            view.task.agent_id.as_deref().unwrap_or("-"),
            task_time(view),
            deps,
            truncate(&title, 60)
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let cut: String = text.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

pub async fn agents(ctx: &CliCtx, args: &AgentsArgs) -> anyhow::Result<ExitCode> {
    // Prefer live daemon data; fall back to local probing when it is down.
    match fetch_state(ctx).await {
        Ok(snapshot) => {
            if args.json {
                println!("{}", serde_json::to_string_pretty(&snapshot.agents)?);
                return Ok(ExitCode::SUCCESS);
            }
            print_agents(
                &snapshot
                    .agents
                    .iter()
                    .map(|a| {
                        (
                            a.id.clone(),
                            a.kind,
                            a.command.clone(),
                            a.state,
                            format!("{}/{}", a.running, a.slots),
                            a.version.clone(),
                            format!(
                                "{}{}",
                                a.probe.label(),
                                a.probe
                                    .detail()
                                    .map(|d| format!(" ({})", truncate(d, 60)))
                                    .unwrap_or_default()
                            ),
                        )
                    })
                    .collect::<Vec<_>>(),
            );
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            println!("daemon not reachable ({e}); probing locally");
            let mut rows = vec![];
            for spec in &ctx.cfg.agents {
                let s = spec.clone();
                let outcome = tokio::task::spawn_blocking(move || {
                    let adapter = forge_runtime::adapter_for(s.kind);
                    adapter.probe(&s)
                })
                .await
                .context("probe task failed")?;
                rows.push((
                    spec.id.clone(),
                    spec.kind,
                    spec.command.clone(),
                    if spec.enabled {
                        forge_core::AgentState::Idle
                    } else {
                        forge_core::AgentState::Disabled
                    },
                    "-".to_string(),
                    outcome.version.clone(),
                    format!(
                        "{}{}",
                        outcome.status.label(),
                        outcome
                            .status
                            .detail()
                            .map(|d| format!(" ({})", truncate(d, 60)))
                            .unwrap_or_default()
                    ),
                ));
            }
            if args.json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
                return Ok(ExitCode::SUCCESS);
            }
            print_agents(&rows);
            Ok(ExitCode::SUCCESS)
        }
    }
}

#[allow(clippy::type_complexity)]
fn print_agents(
    rows: &[(
        String,
        forge_core::AgentKind,
        String,
        forge_core::AgentState,
        String,
        Option<String>,
        String,
    )],
) {
    println!(
        "{:<14} {:<9} {:<10} {:<8} {:<16} PROBE",
        "AGENT", "KIND", "STATE", "SLOTS", "VERSION"
    );
    for (id, kind, _command, state, slots, version, probe) in rows {
        println!(
            "{:<14} {:<9} {:<10} {:<8} {:<16} {}",
            id,
            kind,
            state.as_str(),
            slots,
            version.as_deref().unwrap_or("-"),
            probe
        );
    }
}

pub async fn logs(ctx: &CliCtx, args: &LogsArgs) -> anyhow::Result<ExitCode> {
    let client = ipc::connect(&ctx.paths, "forge-logs")
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let response = client
        .call(Request::GetTaskLog {
            project: args.project.clone(),
            task: args.task.clone(),
            lines: Some(args.lines),
        })
        .await
        .context("daemon communication failed")?;
    match response {
        Response::TaskLog {
            text,
            truncated,
            path,
        } => {
            if let Some(path) = &path {
                println!(
                    "# {} (last {} lines{})",
                    path,
                    args.lines,
                    if truncated { ", truncated" } else { "" }
                );
            }
            for line in text.lines() {
                println!("{}", format_log_line(line));
            }
            Ok(ExitCode::SUCCESS)
        }
        Response::Error { message, .. } => bail!("{message}"),
        other => bail!("unexpected daemon response: {other:?}"),
    }
}

fn format_log_line(line: &str) -> String {
    let mut parts = line.splitn(3, '\t');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(ts), Some(stream), Some(text)) => {
            if let Ok(ms) = ts.parse::<i64>() {
                return format!("{} [{}] {}", format_hms(ms), stream, text);
            }
            line.to_string()
        }
        _ => line.to_string(),
    }
}

// ---------------------------------------------------------------------------
// cancel / retry
// ---------------------------------------------------------------------------

pub async fn cancel(ctx: &CliCtx, args: &CancelArgs) -> anyhow::Result<ExitCode> {
    let client = ipc::connect(&ctx.paths, "forge-cancel")
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let response = client
        .call(Request::CancelTask {
            project: args.project.clone(),
            task: args.task.clone(),
            reason: args.reason.clone(),
        })
        .await
        .context("daemon communication failed")?;
    match response {
        Response::Ack { note } => {
            println!("{}", note.unwrap_or_else(|| "ok".into()));
            Ok(ExitCode::SUCCESS)
        }
        Response::Error { message, .. } => bail!("{message}"),
        other => bail!("unexpected daemon response: {other:?}"),
    }
}

pub async fn retry(ctx: &CliCtx, args: &RetryArgs) -> anyhow::Result<ExitCode> {
    let client = ipc::connect(&ctx.paths, "forge-retry")
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let response = client
        .call(Request::RetryTask {
            project: args.project.clone(),
            task: args.task.clone(),
        })
        .await
        .context("daemon communication failed")?;
    match response {
        Response::Ack { note } => {
            println!("{}", note.unwrap_or_else(|| "ok".into()));
            Ok(ExitCode::SUCCESS)
        }
        Response::Error { message, .. } => bail!("{message}"),
        other => bail!("unexpected daemon response: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// daemon lifecycle (client side)
// ---------------------------------------------------------------------------

async fn wait_for_daemon(paths: &ForgePaths, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if let Ok(client) = ipc::connect(paths, "forge-wait").await {
            if client.ping().await.is_ok() {
                return true;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
    false
}

pub async fn daemon_start(ctx: &CliCtx, args: &DaemonStartArgs) -> anyhow::Result<ExitCode> {
    if ipc::connect(&ctx.paths, "forge-daemon").await.is_ok() {
        println!(
            "daemon is already running ({})",
            ctx.paths.endpoint_file().display()
        );
        return Ok(ExitCode::SUCCESS);
    }
    if !args.background {
        // Foreground: run the daemon in this process.
        return match crate::daemon::run(ctx.cfg.clone(), ctx.paths.clone()).await {
            Ok(()) => Ok(ExitCode::SUCCESS),
            Err(e) => Err(e),
        };
    }

    // Background: detach a child process writing to daemon.log.
    let exe = std::env::current_exe().context("cannot resolve current executable")?;
    let log_path = ctx.paths.daemon_log();
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let stdout = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("cannot open {}", log_path.display()))?;
    let stderr = stdout.try_clone()?;

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon").arg("start");
    if let Some(cfg) = &ctx.explicit_config {
        cmd.arg("--config").arg(cfg);
    }
    if let Some(dd) = &ctx.data_dir_override {
        cmd.arg("--data-dir").arg(dd);
    }
    // The daemon writes its own tracing log to this file; relying on
    // inherited stdio handles is not enough (they are not inherited).
    cmd.env("FORGE_DAEMON_LOG", &log_path);
    cmd.stdin(Stdio::null()).stdout(stdout).stderr(stderr);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    let leak_guard = crate::win::InheritGuard::suppress_std_handles();
    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) if cfg!(windows) => {
            tracing::debug!(error = %e, "breakaway spawn failed; retrying without it");
            let mut cmd = std::process::Command::new(std::env::current_exe()?);
            cmd.arg("daemon").arg("start");
            if let Some(cfg) = &ctx.explicit_config {
                cmd.arg("--config").arg(cfg);
            }
            if let Some(dd) = &ctx.data_dir_override {
                cmd.arg("--data-dir").arg(dd);
            }
            let stdout = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)?;
            let stderr = stdout.try_clone()?;
            cmd.env("FORGE_DAEMON_LOG", &log_path);
            cmd.stdin(Stdio::null()).stdout(stdout).stderr(stderr);
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                const DETACHED_PROCESS: u32 = 0x0000_0008;
                const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
                cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
            }
            cmd.spawn().context("cannot start the daemon process")?
        }
        Err(e) => return Err(e).context("cannot start the daemon process"),
    };
    #[cfg(windows)]
    drop(leak_guard);
    println!("daemon starting (pid {})", child.id());
    if wait_for_daemon(&ctx.paths, std::time::Duration::from_secs(15)).await {
        if let Ok(client) = ipc::connect(&ctx.paths, "forge-daemon").await {
            if let Ok(Response::Pong { uptime_ms, .. }) = client.call(Request::Ping).await {
                println!(
                    "daemon ready (uptime {uptime_ms}ms); logs: {}",
                    log_path.display()
                );
            }
        }
        Ok(ExitCode::SUCCESS)
    } else {
        bail!(
            "daemon did not become ready within 15s; inspect {}",
            log_path.display()
        )
    }
}

pub async fn daemon_stop(ctx: &CliCtx, args: &DaemonStopArgs) -> anyhow::Result<ExitCode> {
    let client = match ipc::connect(&ctx.paths, "forge-daemon").await {
        Ok(c) => c,
        Err(_) => {
            println!("daemon is not running");
            return Ok(ExitCode::SUCCESS);
        }
    };
    let response = client
        .call(Request::Shutdown {
            idempotency_key: Some(format!("cli-{}", std::process::id())),
            drain: Some(args.drain),
        })
        .await
        .context("daemon communication failed")?;
    match response {
        Response::ShuttingDown => {
            println!("shutdown requested");
        }
        Response::Ack { note } => {
            println!("{}", note.unwrap_or_else(|| "shutdown requested".into()));
        }
        Response::Error { message, .. } => bail!("{message}"),
        other => bail!("unexpected daemon response: {other:?}"),
    }
    // Wait until the daemon is gone.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while std::time::Instant::now() < deadline {
        if !ctx.paths.endpoint_file().exists() {
            println!("daemon stopped");
            return Ok(ExitCode::SUCCESS);
        }
        if let Err(e) = ipc::connect(&ctx.paths, "forge-daemon").await {
            // Endpoint file may lag behind; give it a moment.
            tracing::debug!(error = %e, "daemon not reachable yet");
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    bail!("daemon did not stop within 120s")
}

pub async fn daemon_status(ctx: &CliCtx) -> anyhow::Result<ExitCode> {
    let pid_info = std::fs::read_to_string(ctx.paths.pid_file())
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
    match ipc::connect(&ctx.paths, "forge-daemon").await {
        Ok(client) => match client.call(Request::Ping).await {
            Ok(Response::Pong {
                protocol,
                daemon_version,
                uptime_ms,
            }) => {
                println!("daemon: running");
                if let Some(info) = &pid_info {
                    if let Some(pid) = info.get("pid") {
                        println!("  pid:      {pid}");
                    }
                }
                println!(
                    "  endpoint: {}",
                    ctx.paths.endpoint_file().to_string_lossy()
                );
                println!("  version:  {daemon_version} (protocol {protocol})");
                println!("  uptime:   {}ms", uptime_ms);
                if let Ok(snapshot) = fetch_state(ctx).await {
                    println!(
                        "  tasks:    {} running, {} total",
                        snapshot.daemon.running_tasks,
                        snapshot.tasks.len()
                    );
                    let size = std::fs::metadata(&ctx.paths.db_path)
                        .map(|m| m.len())
                        .unwrap_or(0);
                    println!(
                        "  database: {} ({})",
                        ctx.paths.db_path.display(),
                        human_bytes(size)
                    );
                    println!("  logs:     {}", ctx.paths.logs_dir.display());
                }
                Ok(ExitCode::SUCCESS)
            }
            Ok(other) => bail!("unexpected daemon response: {other:?}"),
            Err(e) => bail!("daemon connection failed: {e}"),
        },
        Err(_) => {
            println!("daemon: not running");
            println!("  start it with: forge daemon start --background");
            Ok(ExitCode::FAILURE)
        }
    }
}
