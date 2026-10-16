//! Forge — local-first multi-agent software factory.
//!
//! This library root is shared by the `forge` binary and by the tests
//! (TUI rendering with a `TestBackend`, CLI integration tests, …).

pub mod cli;
pub mod commands;
pub mod daemon;
pub mod demo_agent;
pub mod doctor;
pub mod ipc;
pub mod tui;
pub mod util;
#[cfg(windows)]
pub mod win;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use forge_core::{ForgeConfig, ForgePaths};

use cli::{Cli, Command, DaemonCommand};

/// Shared per-invocation context: parsed configuration, resolved paths and
/// the raw config-load result (doctor reports it instead of dying).
pub struct CliCtx {
    pub cfg: ForgeConfig,
    pub paths: ForgePaths,
    pub explicit_config: Option<PathBuf>,
    pub data_dir_override: Option<PathBuf>,
    pub cfg_check: Result<(), String>,
}

/// Entry point used by the binary.
pub fn main_entry() -> ExitCode {
    let cli = Cli::parse();
    if let Some(dir) = &cli.data_dir {
        std::env::set_var("FORGE_DATA_DIR", dir);
    }
    init_tracing(
        matches!(cli.command, Command::Tui(_)),
        matches!(cli.command, Command::Daemon(DaemonCommand::Start(_))),
    );

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("forge: cannot start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(async_main(cli)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("forge: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn async_main(cli: Cli) -> anyhow::Result<ExitCode> {
    // The controlled demo process runs without any Forge state.
    if let Command::DemoAgent(args) = &cli.command {
        return Ok(demo_agent::run(args));
    }

    let ctx = build_context(&cli)?;
    if let Err(message) = &ctx.cfg_check {
        if !matches!(cli.command, Command::Doctor(_)) {
            anyhow::bail!(
                "configuration error in {}: {message}",
                ctx.paths.config_path.display()
            );
        }
    }

    match &cli.command {
        Command::Init(args) => commands::init(&ctx, args).await,
        Command::Doctor(args) => doctor::run(&ctx, args).await,
        Command::Daemon(action) => match action {
            DaemonCommand::Start(args) => commands::daemon_start(&ctx, args).await,
            DaemonCommand::Stop(args) => commands::daemon_stop(&ctx, args).await,
            DaemonCommand::Status => commands::daemon_status(&ctx).await,
        },
        Command::Tui(args) => tui::run(&ctx, args).await,
        Command::Run(args) => commands::run_plan(&ctx, args).await,
        Command::Status(args) => commands::status(&ctx, args).await,
        Command::Agents(args) => commands::agents(&ctx, args).await,
        Command::Logs(args) => commands::logs(&ctx, args).await,
        Command::Cancel(args) => commands::cancel(&ctx, args).await,
        Command::Retry(args) => commands::retry(&ctx, args).await,
        Command::DemoAgent(_) => unreachable!("handled above"),
    }
}

fn build_context(cli: &Cli) -> anyhow::Result<CliCtx> {
    let base_paths = ForgePaths::resolve(&ForgeConfig::default())?;
    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(|| base_paths.config_path.clone());
    let (cfg, cfg_check) = if config_path.exists() {
        match ForgeConfig::load(&config_path) {
            Ok(cfg) => (cfg, Ok(())),
            Err(e) => (ForgeConfig::default(), Err(e.to_string())),
        }
    } else {
        (ForgeConfig::default(), Ok(()))
    };
    let mut paths = ForgePaths::resolve(&cfg)?;
    paths.config_path = config_path;
    Ok(CliCtx {
        cfg,
        paths,
        explicit_config: cli.config.clone(),
        data_dir_override: cli.data_dir.clone(),
        cfg_check,
    })
}

fn init_tracing(tui: bool, daemon: bool) {
    use tracing_subscriber::EnvFilter;
    let default_level = if daemon { "info" } else { "warn" };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    // A detached daemon cannot rely on inherited stderr; the spawning CLI
    // points it at a log file explicitly.
    if daemon {
        if let Some(path) = std::env::var_os("FORGE_DAEMON_LOG").map(PathBuf::from) {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                let writer = std::sync::Mutex::new(file);
                let _ = tracing_subscriber::fmt()
                    .with_env_filter(filter)
                    .with_writer(writer)
                    .try_init();
                return;
            }
        }
    }
    if tui {
        // Never write logs to the terminal while the TUI is running.
        if let Ok(paths) = ForgePaths::resolve(&ForgeConfig::default()) {
            let _ = std::fs::create_dir_all(&paths.logs_dir);
            let log_path = paths.logs_dir.join("tui.log");
            if let Ok(file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
            {
                let writer = std::sync::Mutex::new(file);
                let _ = tracing_subscriber::fmt()
                    .with_env_filter(filter)
                    .with_writer(writer)
                    .try_init();
                return;
            }
        }
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
