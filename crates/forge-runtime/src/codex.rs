//! Codex CLI adapter.
//!
//! Uses the documented non-interactive mode `codex exec [OPTIONS] [PROMPT]`.
//! Optional flags (`--json`, `--cd`) are only added when the *installed*
//! `codex exec --help` output actually advertises them. Approval/sandbox
//! flags are never invented; users may pass documented flags through
//! `extra_args` in their config. Authentication stays entirely with the
//! official CLI session.

use std::time::Duration;

use forge_core::model::{AgentKind, NormalizedEvent, ProbeStatus, StreamKind};
use forge_core::AgentSpec;

use crate::adapter::{
    classify_json_line, classify_text_line, looks_like_auth_failure, probe_version,
    resolve_agent_command, AgentAdapter, AgentJob, ExitClass, HelpFlags, PreparedCommand,
    ProbeOutcome,
};
use crate::error::RuntimeError;

pub struct CodexAdapter;

impl CodexAdapter {
    fn discover_help(program: &std::path::Path) -> HelpFlags {
        match crate::adapter::run_capture(
            program,
            &["exec".to_string(), "--help".to_string()],
            Duration::from_secs(5),
        ) {
            Ok((_, stdout, stderr)) => HelpFlags::from_help_text(&format!("{stdout}\n{stderr}")),
            Err(_) => HelpFlags::default(),
        }
    }
}

impl AgentAdapter for CodexAdapter {
    fn kind(&self) -> AgentKind {
        AgentKind::Codex
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
        let mut args: Vec<String> = vec!["exec".into()];
        let mut display: Vec<String> = vec!["exec".into()];

        // Structured JSONL events, when the installed CLI supports them.
        if help.has("--json") {
            args.push("--json".into());
            display.push("--json".into());
        }
        // Documented working-directory flag.
        if help.has("--cd") {
            args.push("--cd".into());
            args.push(job.worktree.display().to_string());
            display.push("--cd".into());
            display.push("<worktree>".into());
        }
        for extra in &spec.extra_args {
            args.push(crate::generic::substitute_args(extra, job));
            display.push(redact_extra(extra));
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
            display: format!("codex {}", display.join(" ")),
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

fn redact_extra(arg: &str) -> String {
    crate::adapter::redact_secrets(arg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn job() -> AgentJob {
        AgentJob {
            task_display_id: "T-001".into(),
            task_title: "test".into(),
            spec: "do the thing".into(),
            worktree: PathBuf::from("/tmp/wt"),
            extra_context: BTreeMap::new(),
            timeout: Duration::from_secs(10),
        }
    }

    fn spec() -> AgentSpec {
        AgentSpec {
            id: "codex-a".into(),
            kind: AgentKind::Codex,
            command: "codex".into(),
            extra_args: vec![],
            capabilities: vec![],
            slots: 1,
            env: BTreeMap::new(),
            timeout_secs: None,
            enabled: true,
            stdin_prompt: false,
        }
    }

    #[test]
    fn prepare_uses_base_flags_only_when_absent() {
        // Simulate an installed CLI without --json/--cd support.
        let help = HelpFlags::from_help_text("Usage: codex exec [PROMPT]");
        // Command resolution is not needed for argument shape assertions when
        // the binary exists; use @self so it always resolves.
        let mut s = spec();
        s.command = "@self".into();
        let prepared = CodexAdapter.prepare(&s, &job(), &help).unwrap();
        assert_eq!(prepared.args[0], "exec");
        assert!(!prepared.args.iter().any(|a| a == "--json"));
        assert!(!prepared.args.iter().any(|a| a == "--cd"));
        assert_eq!(prepared.args.last().unwrap(), "do the thing");
    }

    #[test]
    fn prepare_adds_discovered_flags() {
        let help = HelpFlags::from_help_text("Usage: codex exec --json --cd <DIR> [PROMPT]");
        let mut s = spec();
        s.command = "@self".into();
        let prepared = CodexAdapter.prepare(&s, &job(), &help).unwrap();
        assert!(prepared.args.iter().any(|a| a == "--json"));
        let cd_pos = prepared.args.iter().position(|a| a == "--cd").unwrap();
        assert_eq!(
            prepared.args[cd_pos + 1],
            PathBuf::from("/tmp/wt").display().to_string()
        );
    }

    #[test]
    fn classify_exit_detects_auth_failures() {
        let stderr = vec!["Error: not logged in. Run codex login".to_string()];
        assert_eq!(
            CodexAdapter.classify_exit(Some(1), &stderr),
            ExitClass::AuthRequired
        );
        assert_eq!(
            CodexAdapter.classify_exit(Some(0), &stderr),
            ExitClass::Success
        );
        assert_eq!(
            CodexAdapter.classify_exit(Some(2), &["boom".into()]),
            ExitClass::Failed
        );
        assert_eq!(CodexAdapter.classify_exit(None, &[]), ExitClass::Killed);
    }

    #[test]
    fn help_discovery_parses_subcommand_help() {
        // The discovery function runs the real binary; here we test the
        // parsing path only.
        let flags = HelpFlags::from_help_text("Options:\n  --json\n  --cd <DIR>\n");
        assert!(flags.has("--json") && flags.has("--cd"));
    }
}
