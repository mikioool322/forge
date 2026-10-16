//! Vendor-independent agent adapter architecture.
//!
//! Adapters translate a [`AgentJob`] into a concrete command line, probe the
//! installed CLI (version + supported flags discovered from `--help`), and
//! normalize process output into Forge events. Adapters never assume flags
//! that the installed CLI did not advertise.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use forge_core::model::{AgentKind, EventKind, NormalizedEvent, ProbeStatus, Severity, StreamKind};
use forge_core::AgentSpec;

use crate::error::RuntimeError;
use crate::platform;

/// What an adapter produced for one task.
#[derive(Debug, Clone)]
pub struct AgentJob {
    pub task_display_id: String,
    pub task_title: String,
    pub spec: String,
    pub worktree: PathBuf,
    pub extra_context: BTreeMap<String, String>,
    pub timeout: Duration,
}

/// Fully resolved command to execute.
#[derive(Debug, Clone)]
pub struct PreparedCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    /// Prompt to write to stdin (then close it), if the adapter wants that.
    pub stdin_payload: Option<String>,
    /// Human-readable command line for logs (arguments are NOT secrets, but
    /// anything matching secret patterns is redacted).
    pub display: String,
}

/// Flags discovered from `<cli> ... --help` output.
#[derive(Debug, Clone, Default)]
pub struct HelpFlags {
    pub flags: Vec<String>,
}

impl HelpFlags {
    pub fn from_help_text(text: &str) -> Self {
        let mut flags = vec![];
        for token in text.split_whitespace() {
            let token = token.trim_matches(|c: char| {
                !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '=')
            });
            if token.starts_with("--") && token.len() > 2 {
                let name = token.split('=').next().unwrap_or(token).to_string();
                if !flags.contains(&name) {
                    flags.push(name);
                }
            }
        }
        HelpFlags { flags }
    }

    pub fn has(&self, flag: &str) -> bool {
        self.flags.iter().any(|f| f == flag)
    }
}

/// Result of a probe (one agent row).
#[derive(Debug, Clone)]
pub struct ProbeOutcome {
    pub status: ProbeStatus,
    pub version: Option<String>,
    pub help: HelpFlags,
    pub resolved: Option<PathBuf>,
}

/// Classification of a finished process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitClass {
    Success,
    Failed,
    AuthRequired,
    Timeout,
    Killed,
}

impl ExitClass {
    pub fn as_str(self) -> &'static str {
        match self {
            ExitClass::Success => "success",
            ExitClass::Failed => "failed",
            ExitClass::AuthRequired => "auth_required",
            ExitClass::Timeout => "timeout",
            ExitClass::Killed => "killed",
        }
    }
}

pub trait AgentAdapter: Send + Sync {
    fn kind(&self) -> AgentKind;

    /// Probe the installed CLI. Synchronous; run inside `spawn_blocking`.
    fn probe(&self, spec: &AgentSpec) -> ProbeOutcome;

    fn prepare(
        &self,
        spec: &AgentSpec,
        job: &AgentJob,
        help: &HelpFlags,
    ) -> Result<PreparedCommand, RuntimeError>;

    /// Normalize one output line. Returns `None` when the line carries no
    /// signal worth persisting as an event.
    fn classify_line(&self, stream: StreamKind, line: &str) -> Option<NormalizedEvent>;

    fn classify_exit(&self, code: Option<i32>, stderr_tail: &[String]) -> ExitClass;
}

/// Get the adapter for a configured agent kind.
pub fn adapter_for(kind: AgentKind) -> Arc<dyn AgentAdapter> {
    match kind {
        AgentKind::Codex => Arc::new(crate::codex::CodexAdapter),
        AgentKind::Opencode => Arc::new(crate::opencode::OpencodeAdapter),
        AgentKind::Generic => Arc::new(crate::generic::GenericAdapter),
    }
}

// ---------------------------------------------------------------------------
// Shared helpers used by all adapters
// ---------------------------------------------------------------------------

/// Is this a Windows batch script (`.cmd`/`.bat`)?
pub fn is_batch_script(program: &Path) -> bool {
    program
        .extension()
        .map(|e| {
            let e = e.to_string_lossy().to_ascii_lowercase();
            e == "cmd" || e == "bat"
        })
        .unwrap_or(false)
}

/// Decide where the prompt goes: stdin when requested, and always for
/// batch-file CLIs — `cmd.exe` re-parses the command line (variable
/// expansion, `&`/`^` handling), so prompts never travel through it.
/// Returns `(stdin_payload, prompt_in_stdin)`.
pub fn place_prompt(
    program: &Path,
    prompt: String,
    stdin_requested: bool,
) -> (Option<String>, bool) {
    if stdin_requested || is_batch_script(program) {
        (Some(prompt), true)
    } else {
        (None, false)
    }
}

/// Run `<program> [args...]` with a timeout, capturing stdout/stderr.
/// Direct process execution with argument arrays — never a shell string.
///
/// Windows `.cmd`/`.bat` scripts (e.g. npm-installed CLIs) are executed
/// through `cmd.exe /C`, which is the only way `CreateProcess` can run them.
pub fn run_capture(
    program: &Path,
    args: &[String],
    timeout: Duration,
) -> Result<(Option<i32>, String, String), RuntimeError> {
    use std::process::{Command, Stdio};
    let (program, args) = crate::platform::maybe_wrap_script(program.to_path_buf(), args.to_vec());
    let mut cmd = Command::new(&program);
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| RuntimeError::Spawn(format!("{}: {e}", program.display())))?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(RuntimeError::Adapter(format!(
                        "{} timed out after {timeout:?}",
                        program.display()
                    )));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(RuntimeError::Io(e)),
        }
    }
    let output = child.wait_with_output().map_err(RuntimeError::Io)?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    Ok((output.status.code(), stdout, stderr))
}

/// Extract a version-looking token from text.
pub fn extract_version(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        for token in line.split_whitespace() {
            let token = token.trim_start_matches('v');
            let mut parts = token.split('.');
            if let (Some(a), Some(b)) = (parts.next(), parts.next()) {
                if a.chars().all(|c| c.is_ascii_digit())
                    && !a.is_empty()
                    && b.chars().next().is_some_and(|c| c.is_ascii_digit())
                {
                    return Some(line.to_string());
                }
            }
        }
    }
    None
}

/// Redact anything that looks like a secret before it reaches logs.
pub fn redact_secrets(text: &str) -> String {
    const MARKERS: [&str; 6] = [
        "token",
        "secret",
        "password",
        "api_key",
        "apikey",
        "authorization",
    ];
    let lowered = text.to_ascii_lowercase();
    if !MARKERS.iter().any(|m| lowered.contains(m)) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    for token in text.split(' ') {
        let lowered_token = token.to_ascii_lowercase();
        if MARKERS.iter().any(|m| lowered_token.contains(m)) {
            if let Some((key, _)) = token.split_once('=') {
                out.push_str(key);
                out.push_str("=***");
            } else {
                out.push_str("***");
            }
        } else {
            out.push_str(token);
        }
        out.push(' ');
    }
    out.trim_end().to_string()
}

/// Substring patterns that indicate the CLI has no usable authorization.
const AUTH_MARKERS: [&str; 9] = [
    "not logged in",
    "please log in",
    "please login",
    "login required",
    "authentication required",
    "unauthorized",
    "401",
    "invalid api key",
    "no api key",
];

pub fn looks_like_auth_failure(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    AUTH_MARKERS.iter().any(|m| lowered.contains(m))
}

/// Heuristic line classification shared by CLI adapters.
pub fn classify_text_line(stream: StreamKind, line: &str) -> Option<NormalizedEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lowered = trimmed.to_ascii_lowercase();
    let severity = if lowered.contains("error") || lowered.contains("fatal") {
        Severity::Error
    } else if lowered.contains("warn") || lowered.contains("failed") || lowered.contains("failure")
    {
        Severity::Warn
    } else {
        Severity::Info
    };
    Some(NormalizedEvent {
        kind: EventKind::OutputEvent,
        severity,
        message: trimmed.chars().take(500).collect(),
        data: Some(serde_json::json!({
            "heuristic": true,
            "stream": match stream {
                StreamKind::Stdout => "stdout",
                StreamKind::Stderr => "stderr",
                StreamKind::System => "system",
            }
        })),
        stream,
    })
}

/// If a JSON line contains useful structure, normalize it.
pub fn classify_json_line(stream: StreamKind, line: &str) -> Option<NormalizedEvent> {
    let trimmed = line.trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    let obj = value.as_object()?;
    let type_field = obj
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("json_event")
        .to_string();
    let message = obj
        .get("message")
        .or_else(|| obj.get("msg"))
        .or_else(|| obj.get("text"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            let mut t = type_field.clone();
            if let Some(sub) = obj.get("item_type").and_then(|v| v.as_str()) {
                t.push(':');
                t.push_str(sub);
            }
            t
        });
    Some(NormalizedEvent {
        kind: EventKind::OutputEvent,
        severity: Severity::Info,
        message: message.chars().take(500).collect(),
        data: Some(serde_json::json!({
            "raw_type": type_field,
            "source": "cli_json",
        })),
        stream,
    })
}

/// Find the installed version of a CLI (used by probes).
pub fn probe_version(program: &Path) -> (ProbeStatus, Option<String>, HelpFlags) {
    let mut help_flags = HelpFlags::default();
    let mut last_error: Option<String> = None;
    for args in [vec!["--version".to_string()], vec!["-V".to_string()]] {
        match run_capture(program, &args, Duration::from_secs(5)) {
            Ok((code, stdout, stderr)) => {
                let text = format!("{stdout}\n{stderr}");
                if code == Some(0) {
                    let version = extract_version(&text);
                    return (
                        ProbeStatus::Ok {
                            version: version.clone(),
                        },
                        version,
                        help_flags,
                    );
                }
                last_error = Some(format!("`{}` exited with {code:?}", args.join(" ")));
            }
            Err(e) => last_error = Some(e.to_string()),
        }
    }
    // Some CLIs only expose flags via --help; capture those regardless.
    if let Ok((_, stdout, stderr)) =
        run_capture(program, &["--help".to_string()], Duration::from_secs(5))
    {
        help_flags = HelpFlags::from_help_text(&format!("{stdout}\n{stderr}"));
    }
    match last_error {
        Some(detail) => (ProbeStatus::Error { detail }, None, help_flags),
        None => (ProbeStatus::Unknown, None, help_flags),
    }
}

/// Resolve an agent's command; `@self` -> this binary.
pub fn resolve_agent_command(spec: &AgentSpec) -> Result<PathBuf, RuntimeError> {
    platform::resolve_executable(&spec.command)
        .ok_or_else(|| RuntimeError::ExecutableNotFound(spec.command.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_flags_parsing() {
        let help = "Usage: codex exec [OPTIONS] [PROMPT]\n\
                    Options:\n  \
                    --json  Output JSONL events\n  \
                    --cd <DIR>  Working directory\n  \
                    --model <MODEL>=default\n  \
                    -h, --help\n";
        let flags = HelpFlags::from_help_text(help);
        assert!(flags.has("--json"));
        assert!(flags.has("--cd"));
        assert!(flags.has("--model"));
        assert!(!flags.has("--full-auto"));
        assert!(!flags.has("-h"));
    }

    #[test]
    fn version_extraction() {
        assert_eq!(
            extract_version("codex-cli 0.21.0"),
            Some("codex-cli 0.21.0".to_string())
        );
        assert_eq!(
            extract_version("opencode v2.0.29"),
            Some("opencode v2.0.29".into())
        );
        assert_eq!(extract_version("no version here"), None);
    }

    #[test]
    fn secret_redaction() {
        let redacted = redact_secrets("run --token=abc123 --model gpt");
        assert!(!redacted.contains("abc123"));
        assert!(redacted.contains("***"));
        assert!(redacted.contains("--model gpt"));
        assert_eq!(redact_secrets("plain text"), "plain text");
    }

    #[test]
    fn auth_failure_detection() {
        assert!(looks_like_auth_failure(
            "Error: not logged in. Run `codex login`."
        ));
        assert!(looks_like_auth_failure("HTTP 401 Unauthorized"));
        assert!(!looks_like_auth_failure("All tests passed"));
    }

    #[test]
    fn json_line_classification() {
        let ev = classify_json_line(
            StreamKind::Stdout,
            r#"{"type":"item.completed","message":"wrote src/lib.rs"}"#,
        )
        .unwrap();
        assert_eq!(ev.message, "wrote src/lib.rs");
        assert_eq!(ev.severity, Severity::Info);

        let ev =
            classify_json_line(StreamKind::Stdout, r#"{"type":"reasoning","msg":"hmm"}"#).unwrap();
        assert_eq!(ev.message, "hmm");

        assert!(classify_json_line(StreamKind::Stdout, "plain text").is_none());
        assert!(classify_json_line(StreamKind::Stdout, "{invalid").is_none());
    }

    #[test]
    fn text_line_severity() {
        let ev = classify_text_line(StreamKind::Stderr, "error: build failed").unwrap();
        assert_eq!(ev.severity, Severity::Error);
        let ev = classify_text_line(StreamKind::Stdout, "warning: unused import").unwrap();
        assert_eq!(ev.severity, Severity::Warn);
        let ev = classify_text_line(StreamKind::Stdout, "editing src/lib.rs").unwrap();
        assert_eq!(ev.severity, Severity::Info);
        assert!(classify_text_line(StreamKind::Stdout, "   ").is_none());
    }
}
