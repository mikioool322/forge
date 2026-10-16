//! CLI surface (clap).

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "forge",
    version,
    about = "Forge - local-first multi-agent software factory",
    long_about = "Forge orchestrates multiple AI coding agents through a persistent local daemon.\n\
                  The TUI is a client: closing it never stops running tasks."
)]
pub struct Cli {
    /// Override the configuration file path.
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    /// Override the data directory (same as FORGE_DATA_DIR).
    #[arg(long, global = true)]
    pub data_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Register the current Git repository and create the config/dirs.
    Init(InitArgs),
    /// Diagnose the local environment (git, agents, database, daemon).
    Doctor(DoctorArgs),
    /// Control the background daemon.
    #[command(subcommand)]
    Daemon(DaemonCommand),
    /// Attach the terminal UI to the daemon.
    Tui(TuiArgs),
    /// Submit a deterministic task plan (TOML) to the daemon.
    Run(RunArgs),
    /// Show tasks of a project.
    Status(StatusArgs),
    /// Show configured agents and their probe status.
    Agents(AgentsArgs),
    /// Print the log tail for a task.
    Logs(LogsArgs),
    /// Cancel a task (running tasks are terminated).
    Cancel(CancelArgs),
    /// Re-queue a failed/interrupted task.
    Retry(RetryArgs),
    /// Deterministic controlled process used for infrastructure tests.
    #[command(name = "__demo-agent", hide = true)]
    DemoAgent(DemoAgentArgs),
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Project name (defaults to the directory name).
    #[arg(long)]
    pub name: Option<String>,
    /// Repository root (defaults to the Git root of the current directory).
    #[arg(long)]
    pub root: Option<PathBuf>,
    /// Overwrite an existing config file.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// Start the daemon (foreground unless --background).
    Start(DaemonStartArgs),
    /// Ask the daemon to shut down gracefully.
    Stop(DaemonStopArgs),
    /// Report whether the daemon is running.
    Status,
}

#[derive(Debug, Args)]
pub struct DaemonStartArgs {
    /// Detach from the terminal (writes logs to <data>/logs/daemon.log).
    #[arg(long)]
    pub background: bool,
    /// Let running tasks finish before exiting (used by `stop --drain`).
    #[arg(long)]
    pub drain: bool,
}

#[derive(Debug, Args)]
pub struct DaemonStopArgs {
    /// Wait for running tasks to finish instead of cancelling them.
    #[arg(long)]
    pub drain: bool,
}

#[derive(Debug, Args)]
pub struct TuiArgs {
    /// Project to focus (defaults to the only registered project).
    #[arg(long)]
    pub project: Option<String>,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Plan file (TOML).
    pub spec: PathBuf,
    /// Project name (defaults to the plan's `project` field).
    #[arg(long)]
    pub project: Option<String>,
    /// Validate the plan and print it without submitting.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct StatusArgs {
    #[arg(long)]
    pub project: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct AgentsArgs {
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct LogsArgs {
    /// Task reference: T-001, T1 or 1.
    pub task: String,
    #[arg(long)]
    pub project: Option<String>,
    /// Number of trailing lines.
    #[arg(short = 'n', long, default_value_t = 200)]
    pub lines: usize,
}

#[derive(Debug, Args)]
pub struct CancelArgs {
    pub task: String,
    #[arg(long)]
    pub project: Option<String>,
    #[arg(long)]
    pub reason: Option<String>,
}

#[derive(Debug, Args)]
pub struct RetryArgs {
    pub task: String,
    #[arg(long)]
    pub project: Option<String>,
}

#[derive(Debug, Args)]
pub struct DemoAgentArgs {
    #[arg(long, default_value = "sim")]
    pub name: String,
    /// Files to create (repeatable).
    #[arg(long = "file")]
    pub files: Vec<String>,
    /// Files to append a line to (repeatable).
    #[arg(long = "edit")]
    pub edits: Vec<String>,
    #[arg(long, default_value_t = 4)]
    pub steps: u32,
    #[arg(long, default_value_t = 250)]
    pub sleep_ms: u64,
    /// Extra sleep before exiting (cancellation tests).
    #[arg(long, default_value_t = 0)]
    pub hang_ms: u64,
    /// Exit non-zero after doing partial work.
    #[arg(long)]
    pub fail: bool,
    /// Simulate an authentication failure.
    #[arg(long)]
    pub auth_fail: bool,
    #[arg(long)]
    pub version: bool,
    /// Task prompt (ignored by the controlled process; present so that
    /// generic adapters can append the prompt as a positional argument).
    #[arg(hide = true)]
    pub prompt: Option<String>,
    /// Read up to N bytes from stdin before doing anything else
    /// (0 = never read stdin). Used by the stdin-backpressure tests.
    #[arg(long, default_value_t = 0)]
    pub read_stdin_bytes: usize,
    /// Spawn a helper process that outlives this one, for the given number of
    /// milliseconds (tree-ownership tests).
    #[arg(long, default_value_t = 0)]
    pub spawn_descendant_ms: u64,
    /// Spawn that helper with `CREATE_BREAKAWAY_FROM_JOB`, so it leaves Forge's
    /// process tree deliberately.
    #[arg(long)]
    pub breakaway: bool,
}
