//! Configuration schema, validation and filesystem layout.
//!
//! The config file lives at `$FORGE_CONFIG_DIR/config.toml`
//! (defaults to `~/.config/forge/config.toml`, `%APPDATA%\forge\config.toml`
//! on Windows). Runtime state lives under `$FORGE_DATA_DIR`
//! (defaults to the platform data dir, e.g. `~/.local/share/forge`).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::{AgentKind, AgentSpec};

// ---------------------------------------------------------------------------
// Config structures
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ForgeConfig {
    pub daemon: DaemonConfig,
    pub git: GitConfig,
    pub logs: LogsConfig,
    pub recovery: RecoveryConfig,
    pub agents: Vec<AgentSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    /// Explicit IPC endpoint: `unix:/path/to.sock` or `tcp:127.0.0.1:PORT`
    /// (port 0 = pick a free port and advertise it in the endpoint file).
    pub endpoint: Option<String>,
    pub max_concurrency: u32,
    pub task_timeout_secs: u64,
    pub lease_timeout_secs: u64,
    pub heartbeat_interval_secs: u64,
    pub connection_limit: usize,
    /// Seconds to let running tasks finish during `daemon stop` before they
    /// are cancelled. 0 = cancel immediately.
    pub shutdown_drain_secs: u64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            endpoint: None,
            max_concurrency: 2,
            task_timeout_secs: 900,
            lease_timeout_secs: 120,
            heartbeat_interval_secs: 5,
            connection_limit: 32,
            shutdown_drain_secs: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GitConfig {
    pub worktree_root: Option<PathBuf>,
    pub branch_prefix: String,
    pub keep_failed_worktrees: bool,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            worktree_root: None,
            branch_prefix: "forge/".into(),
            keep_failed_worktrees: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogsConfig {
    pub dir: Option<PathBuf>,
    /// Log files are rotated (new suffix `.1`) once they exceed this size.
    pub max_file_bytes: u64,
    /// Individual lines longer than this are truncated before writing.
    pub max_line_bytes: usize,
    pub tail_default_lines: usize,
}

impl Default for LogsConfig {
    fn default() -> Self {
        Self {
            dir: None,
            max_file_bytes: 64 * 1024 * 1024,
            max_line_bytes: 16 * 1024,
            tail_default_lines: 200,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrphanAction {
    /// Kill orphaned agent process trees found during recovery.
    Kill,
    /// Only report orphaned processes and leave them running.
    Report,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecoveryConfig {
    pub orphan_action: OrphanAction,
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        Self {
            orphan_action: OrphanAction::Kill,
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot parse config file: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

// ---------------------------------------------------------------------------
// Loading / validation
// ---------------------------------------------------------------------------

impl ForgeConfig {
    pub fn parse(contents: &str) -> Result<Self, ConfigError> {
        let cfg: ForgeConfig = toml::from_str(contents)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let contents = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&contents)
    }

    /// Load the config if the file exists, otherwise return defaults.
    pub fn load_or_default(path: &Path) -> Result<Self, ConfigError> {
        if path.exists() {
            Self::load(path)
        } else {
            Ok(Self::default())
        }
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.daemon.max_concurrency == 0 {
            return Err(ConfigError::Invalid(
                "daemon.max_concurrency must be >= 1".into(),
            ));
        }
        if self.daemon.heartbeat_interval_secs == 0 {
            return Err(ConfigError::Invalid(
                "daemon.heartbeat_interval_secs must be >= 1".into(),
            ));
        }
        if self.daemon.lease_timeout_secs < self.daemon.heartbeat_interval_secs * 2 {
            return Err(ConfigError::Invalid(
                "daemon.lease_timeout_secs must be at least 2x heartbeat_interval_secs".into(),
            ));
        }
        if self.logs.max_line_bytes < 256 {
            return Err(ConfigError::Invalid(
                "logs.max_line_bytes must be >= 256".into(),
            ));
        }
        if self.git.branch_prefix.is_empty()
            || self
                .git
                .branch_prefix
                .chars()
                .any(|c| c.is_whitespace() || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
        {
            return Err(ConfigError::Invalid(format!(
                "git.branch_prefix {:?} is not a valid git ref prefix",
                self.git.branch_prefix
            )));
        }
        if let Some(ep) = &self.daemon.endpoint {
            if !(ep.starts_with("unix:") || ep.starts_with("tcp:")) {
                return Err(ConfigError::Invalid(format!(
                    "daemon.endpoint {ep:?} must start with `unix:` or `tcp:`"
                )));
            }
        }
        let mut ids = BTreeSet::new();
        for agent in &self.agents {
            agent.validate().map_err(ConfigError::Invalid)?;
            if !ids.insert(agent.id.clone()) {
                return Err(ConfigError::Invalid(format!(
                    "duplicate agent id {:?}",
                    agent.id
                )));
            }
            if agent.kind != AgentKind::Generic && agent.command.trim().is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "agent {:?}: command must not be empty",
                    agent.id
                )));
            }
        }
        Ok(())
    }
}

/// Template written by `forge init` when no config file exists.
pub const DEFAULT_CONFIG_TEMPLATE: &str = r#"# Forge configuration.
#
# Locations can be overridden with environment variables:
#   FORGE_DATA_DIR    runtime state, logs, worktrees, SQLite database
#   FORGE_CONFIG_DIR  this configuration directory

[daemon]
# Maximum number of concurrently running tasks across all agents.
max_concurrency = 2
# Hard per-task wall-clock timeout (seconds).
task_timeout_secs = 900
# A lease whose heartbeat is older than this is considered expired.
lease_timeout_secs = 120
heartbeat_interval_secs = 5
# Maximum number of simultaneous IPC client connections.
connection_limit = 32
# Seconds to let running tasks finish on `forge daemon stop` (0 = cancel now).
shutdown_drain_secs = 0
# endpoint = "unix:/custom/path/forge.sock"   # or "tcp:127.0.0.1:0"

[git]
branch_prefix = "forge/"
keep_failed_worktrees = true
# worktree_root = "/custom/worktree/root"

[logs]
max_file_bytes = 67108864
max_line_bytes = 16384
tail_default_lines = 200
# dir = "/custom/log/dir"

[recovery]
# What to do with agent processes still alive after a daemon crash.
orphan_action = "kill"   # or "report"

# ---------------------------------------------------------------------------
# Agents
# ---------------------------------------------------------------------------
#
# `kind` selects the adapter: "codex", "opencode" or "generic".
# `command` is the executable (PATH lookup or absolute path). `@self` means
# the forge binary itself (used by the bundled deterministic test agent).
#
# Example real adapters (enable only what is installed and authorized):
#
# [[agents]]
# id = "codex-a"
# kind = "codex"
# command = "codex"
# capabilities = ["code"]
# slots = 1
#
# [[agents]]
# id = "opencode-b"
# kind = "opencode"
# command = "opencode"
# capabilities = ["code", "tests"]
# slots = 1
#
# Deterministic controlled process (used by scripts/e2e-demo.ps1):
#
# [[agents]]
# id = "sim-a"
# kind = "generic"
# command = "@self"
# extra_args = ["__demo-agent", "--name", "sim-a", "--file", "alpha.txt"]
# capabilities = ["code"]
"#;

// ---------------------------------------------------------------------------
// Filesystem layout
// ---------------------------------------------------------------------------

/// Resolved filesystem locations for one Forge installation.
#[derive(Debug, Clone)]
pub struct ForgePaths {
    pub data_dir: PathBuf,
    pub config_dir: PathBuf,
    pub run_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub worktrees_dir: PathBuf,
    pub db_path: PathBuf,
    pub config_path: PathBuf,
}

impl ForgePaths {
    pub fn resolve(cfg: &ForgeConfig) -> Result<Self, ConfigError> {
        let data_dir = std::env::var_os("FORGE_DATA_DIR")
            .map(PathBuf::from)
            .or_else(|| dirs::data_dir().map(|d| d.join("forge")))
            .ok_or_else(|| {
                ConfigError::Invalid("cannot determine data directory; set FORGE_DATA_DIR".into())
            })?;
        let config_dir = std::env::var_os("FORGE_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| dirs::config_dir().map(|d| d.join("forge")))
            .ok_or_else(|| {
                ConfigError::Invalid(
                    "cannot determine config directory; set FORGE_CONFIG_DIR".into(),
                )
            })?;
        Ok(Self::from_roots(data_dir, config_dir, cfg))
    }

    pub fn from_roots(data_dir: PathBuf, config_dir: PathBuf, cfg: &ForgeConfig) -> Self {
        let run_dir = data_dir.join("run");
        let logs_dir = cfg
            .logs
            .dir
            .clone()
            .unwrap_or_else(|| data_dir.join("logs"));
        let worktrees_dir = cfg
            .git
            .worktree_root
            .clone()
            .unwrap_or_else(|| data_dir.join("worktrees"));
        let db_path = data_dir.join("state.db");
        let config_path = config_dir.join("config.toml");
        Self {
            data_dir,
            config_dir,
            run_dir,
            logs_dir,
            worktrees_dir,
            db_path,
            config_path,
        }
    }

    pub fn socket_path(&self) -> PathBuf {
        self.run_dir.join("forge.sock")
    }

    pub fn endpoint_file(&self) -> PathBuf {
        self.run_dir.join("endpoint")
    }

    pub fn token_file(&self) -> PathBuf {
        self.run_dir.join("token")
    }

    pub fn lock_file(&self) -> PathBuf {
        self.run_dir.join("daemon.lock")
    }

    pub fn pid_file(&self) -> PathBuf {
        self.run_dir.join("daemon.pid")
    }

    pub fn daemon_log(&self) -> PathBuf {
        self.logs_dir.join("daemon.log")
    }

    pub fn project_logs_dir(&self, project: &str) -> PathBuf {
        self.logs_dir.join(sanitize_component(project))
    }

    pub fn project_worktrees_dir(&self, project: &str) -> PathBuf {
        self.worktrees_dir.join(sanitize_component(project))
    }

    /// Default IPC endpoint for this installation.
    pub fn default_endpoint(&self) -> String {
        if cfg!(unix) {
            format!("unix:{}", self.socket_path().display())
        } else {
            "tcp:127.0.0.1:0".to_string()
        }
    }

    pub fn endpoint_spec(&self, cfg: &ForgeConfig) -> String {
        cfg.daemon
            .endpoint
            .clone()
            .unwrap_or_else(|| self.default_endpoint())
    }
}

/// Restrict a string to a safe single path component.
///
/// Every character outside `[A-Za-z0-9._-]` becomes `_`; consecutive dots are
/// collapsed so the result can never be `..` or contain a `..` segment.
pub fn sanitize_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dot = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
            last_dot = false;
            out.push(c);
        } else if c == '.' {
            if last_dot {
                continue;
            }
            last_dot = true;
            out.push('.');
        } else {
            last_dot = false;
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('.').to_string();
    if trimmed.is_empty() {
        "unnamed".into()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[daemon]
max_concurrency = 3
task_timeout_secs = 60

[git]
branch_prefix = "forge/"

[[agents]]
id = "codex-a"
kind = "codex"
command = "codex"
capabilities = ["code"]
"#;

    #[test]
    fn parses_sample_config_with_defaults() {
        let cfg = ForgeConfig::parse(SAMPLE).unwrap();
        assert_eq!(cfg.daemon.max_concurrency, 3);
        assert_eq!(cfg.daemon.task_timeout_secs, 60);
        assert_eq!(cfg.daemon.heartbeat_interval_secs, 5); // default
        assert_eq!(cfg.agents.len(), 1);
        assert_eq!(cfg.agents[0].kind, AgentKind::Codex);
        assert_eq!(cfg.agents[0].slots, 1); // default
    }

    #[test]
    fn empty_config_is_valid() {
        let cfg = ForgeConfig::parse("").unwrap();
        assert_eq!(cfg.daemon.max_concurrency, 2);
        assert!(cfg.agents.is_empty());
    }

    #[test]
    fn rejects_unknown_fields() {
        let err = ForgeConfig::parse("[daemon]\nmax_concurreny = 2\n").unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)), "got {err:?}");
    }

    #[test]
    fn rejects_duplicate_agent_ids() {
        let src = r#"
[[agents]]
id = "a"
kind = "codex"
command = "codex"

[[agents]]
id = "a"
kind = "opencode"
command = "opencode"
"#;
        let err = ForgeConfig::parse(src).unwrap_err();
        assert!(err.to_string().contains("duplicate agent id"), "{err}");
    }

    #[test]
    fn rejects_bad_agent_id() {
        let src = r#"
[[agents]]
id = "bad id!"
kind = "generic"
command = "x"
"#;
        assert!(ForgeConfig::parse(src).is_err());
    }

    #[test]
    fn rejects_zero_concurrency() {
        assert!(ForgeConfig::parse("[daemon]\nmax_concurrency = 0\n").is_err());
    }

    #[test]
    fn rejects_invalid_endpoint() {
        assert!(ForgeConfig::parse("[daemon]\nendpoint = \"http://x\"\n").is_err());
        assert!(ForgeConfig::parse("[daemon]\nendpoint = \"unix:/tmp/x.sock\"\n").is_ok());
        assert!(ForgeConfig::parse("[daemon]\nendpoint = \"tcp:127.0.0.1:0\"\n").is_ok());
    }

    #[test]
    fn paths_layout() {
        let cfg = ForgeConfig::default();
        let p = ForgePaths::from_roots(PathBuf::from("/data"), PathBuf::from("/cfg"), &cfg);
        assert_eq!(p.db_path, PathBuf::from("/data/state.db"));
        assert_eq!(p.config_path, PathBuf::from("/cfg/config.toml"));
        assert_eq!(p.run_dir, PathBuf::from("/data/run"));
        assert_eq!(p.socket_path(), PathBuf::from("/data/run/forge.sock"));
        assert_eq!(
            p.project_logs_dir("demo-api"),
            PathBuf::from("/data/logs/demo-api")
        );
    }

    #[test]
    fn sanitize_component_blocks_traversal() {
        let evil = sanitize_component("../../etc/passwd");
        assert!(!evil.contains('/'), "{evil}");
        assert!(!evil.contains(".."), "{evil}");
        assert_eq!(sanitize_component("demo-api"), "demo-api");
        assert_eq!(sanitize_component(""), "unnamed");
        assert_eq!(sanitize_component(".."), "unnamed");
        assert_eq!(sanitize_component("a.b-c_d"), "a.b-c_d");
        // Windows-style separators as well.
        let win = sanitize_component("..\\..\\windows\\system32");
        assert!(!win.contains('\\') && !win.contains(".."), "{win}");
    }
}
