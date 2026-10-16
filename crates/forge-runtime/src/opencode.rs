//! OpenCode 2 CLI adapter.
//!
//! Uses the documented non-interactive invocation `opencode run [message]`.
//! The installed version is detected with `opencode --version`; flag support
//! is discovered from `opencode run --help` and nothing is added that the
//! CLI does not advertise. OpenCode's own authentication (its auth store /
//! sessions) is used as-is; Forge never extracts tokens.

use std::time::Duration;

use forge_core::model::{AgentKind, NormalizedEvent, ProbeStatus, StreamKind};
use forge_core::AgentSpec;

use crate::adapter::{
    classify_json_line, classify_text_line, looks_like_auth_failure, probe_version,
    resolve_agent_command, AgentAdapter, AgentJob, ExitClass, HelpFlags, PreparedCommand,
    ProbeOutcome,
};
use crate::error::RuntimeError;

pub struct OpencodeAdapter;

impl OpencodeAdapter {
    fn discover_help(program: &std::path::Path) -> HelpFlags {
        match crate::adapter::run_capture(
            program,
            &["run".to_string(), "--help".to_string()],
            Duration::from_secs(5),
        ) {
            Ok((_, stdout, stderr)) => HelpFlags::from_help_text(&format!("{stdout}\n{stderr}")),
            Err(_) => HelpFlags::default(),
        }
    }
}

impl AgentAdapter for OpencodeAdapter {
    fn kind(&self) -> AgentKind {
        AgentKind::Opencode
    }

    fn probe(&self, spec: &AgentSpec) -> ProbeOutcome {
        let Ok(program) = resolve_agent_command(spec) else {
            return ProbeOutcome {
                status: ProbeStatus::Missing {
                    detail: format!("command {:?} not found on PATH", spec.command),
                },
                version: None,
                help: HelpFlags::default(),
                resolved: None,
            };
        };
        let (status, version, _) = probe_version(&program);
        let help = Self::discover_help(&program);
        ProbeOutcome {
            status,
            version,
            help,
            resolved: Some(program),
        }
    }

    fn prepare(
        &self,
        spec: &AgentSpec,
        job: &AgentJob,
        help: &HelpFlags,
    ) -> Result<PreparedCommand, RuntimeError> {
        let program = resolve_agent_command(spec)?;
        let mut args: Vec<String> = vec!["run".into()];
        let mut display: Vec<String> = vec!["run".into()];

        // Nothing is auto-added beyond the subcommand: OpenCode's run mode
        // exposes behaviour flags (`--auto`, `--format json`, `--standalone`,
        // `--print-logs`, `--model`, …) that users opt into explicitly via
        // `extra_args`; we only verify that the installed CLI advertises them.
        for extra in &spec.extra_args {
            args.push(crate::generic::substitute_args(extra, job));
            display.push(crate::adapter::redact_secrets(extra));
        }
        if !help.flags.iter().any(|f| f == "--auto") {
            tracing::debug!(
                "opencode: --auto is not advertised by this version; non-interactive \
                 permission handling depends on the user's OpenCode configuration"
            );
        }

        let stdin_payload = if spec.stdin_prompt {
            Some(job.spec.clone())
        } else {
            args.push(job.spec.clone());
            display.push("<prompt>".into());
            None
        };

        Ok(PreparedCommand {
            program,
            args,
            cwd: job.worktree.clone(),
            env: spec
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            stdin_payload,
            display: format!("opencode {}", display.join(" ")),
        })
    }

    fn classify_line(&self, stream: StreamKind, line: &str) -> Option<NormalizedEvent> {
        classify_json_line(stream, line).or_else(|| classify_text_line(stream, line))
    }

    fn classify_exit(&self, code: Option<i32>, stderr_tail: &[String]) -> ExitClass {
        match code {
            Some(0) => ExitClass::Success,
            Some(_) => {
                let tail = stderr_tail.join("\n");
                if looks_like_auth_failure(&tail) {
                    ExitClass::AuthRequired
                } else {
                    ExitClass::Failed
                }
            }
            None => ExitClass::Killed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn job() -> AgentJob {
        AgentJob {
            task_display_id: "T-002".into(),
            task_title: "test".into(),
            spec: "fix the bug".into(),
            worktree: PathBuf::from("/tmp/wt2"),
            extra_context: BTreeMap::new(),
            timeout: Duration::from_secs(10),
        }
    }

    #[test]
    fn prepare_shapes_run_invocation() {
        let mut spec = AgentSpec {
            id: "opencode-b".into(),
            kind: AgentKind::Opencode,
            command: "@self".into(),
            extra_args: vec![],
            capabilities: vec![],
            slots: 1,
            env: BTreeMap::new(),
            timeout_secs: None,
            enabled: true,
            stdin_prompt: false,
        };
        let prepared = OpencodeAdapter
            .prepare(&spec, &job(), &HelpFlags::default())
            .unwrap();
        assert_eq!(prepared.args, vec!["run", "fix the bug"]);
        assert_eq!(prepared.cwd, PathBuf::from("/tmp/wt2"));

        // stdin mode
        spec.stdin_prompt = true;
        let prepared = OpencodeAdapter
            .prepare(&spec, &job(), &HelpFlags::default())
            .unwrap();
        assert_eq!(prepared.args, vec!["run"]);
        assert_eq!(prepared.stdin_payload.as_deref(), Some("fix the bug"));
    }

    #[test]
    fn only_extra_args_are_added() {
        // Nothing beyond the documented subcommand is auto-added; user
        // `extra_args` are passed through verbatim.
        let help = HelpFlags::from_help_text("Usage: opencode run --print-logs [message]");
        let mut spec = AgentSpec {
            id: "oc".into(),
            kind: AgentKind::Opencode,
            command: "@self".into(),
            extra_args: vec!["--auto".into()],
            capabilities: vec![],
            slots: 1,
            env: BTreeMap::new(),
            timeout_secs: None,
            enabled: true,
            stdin_prompt: false,
        };
        let prepared = OpencodeAdapter.prepare(&spec, &job(), &help).unwrap();
        assert_eq!(prepared.args, vec!["run", "--auto", "fix the bug"]);
        spec.extra_args = vec![];
        let prepared = OpencodeAdapter.prepare(&spec, &job(), &help).unwrap();
        assert_eq!(prepared.args, vec!["run", "fix the bug"]);
    }

    #[test]
    fn classify_exit_auth() {
        assert_eq!(
            OpencodeAdapter.classify_exit(Some(1), &["Please log in to continue".into()]),
            ExitClass::AuthRequired
        );
        assert_eq!(
            OpencodeAdapter.classify_exit(Some(3), &["compile error".into()]),
            ExitClass::Failed
        );
    }
}
