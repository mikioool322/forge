//! Vendor-independent generic adapter.
//!
//! Runs an arbitrary command with templated arguments. This is the adapter
//! used by the bundled deterministic `forge __demo-agent` controlled process
//! and by users who want to wire any CLI of their own.
//!
//! Argument templates understand `{prompt}`, `{worktree}`, `{task}` and
//! `{title}`. Unknown `{...}` sequences are passed through untouched. If
//! `stdin_prompt = true` the prompt is written to the process stdin; if no
//! `{prompt}` placeholder is present and stdin is not used, the prompt is
//! appended as the final positional argument.

use forge_core::model::{AgentKind, NormalizedEvent, ProbeStatus, StreamKind};
use forge_core::AgentSpec;

use crate::adapter::{
    classify_json_line, classify_text_line, looks_like_auth_failure, probe_version,
    resolve_agent_command, AgentAdapter, AgentJob, ExitClass, HelpFlags, PreparedCommand,
    ProbeOutcome,
};
use crate::error::RuntimeError;

pub struct GenericAdapter;

/// Substitute job placeholders inside a single argument.
pub fn substitute_args(template: &str, job: &AgentJob) -> String {
    template
        .replace("{prompt}", &job.spec)
        .replace("{worktree}", &job.worktree.display().to_string())
        .replace("{task}", &job.task_display_id)
        .replace("{title}", &job.task_title)
}

impl AgentAdapter for GenericAdapter {
    fn kind(&self) -> AgentKind {
        AgentKind::Generic
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
        let (status, version, help) = probe_version(&program);
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
        _help: &HelpFlags,
    ) -> Result<PreparedCommand, RuntimeError> {
        let program = resolve_agent_command(spec)?;
        let mut args: Vec<String> = Vec::with_capacity(spec.extra_args.len() + 1);
        let mut has_prompt_placeholder = false;
        for template in &spec.extra_args {
            if template.contains("{prompt}") {
                has_prompt_placeholder = true;
            }
            args.push(substitute_args(template, job));
        }
        let stdin_payload = if spec.stdin_prompt {
            Some(job.spec.clone())
        } else {
            if !has_prompt_placeholder {
                args.push(job.spec.clone());
            }
            None
        };
        let display = format!(
            "{} {}",
            program
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default(),
            args.iter()
                .map(|a| if a.contains('\n') {
                    "<prompt>".to_string()
                } else {
                    crate::adapter::redact_secrets(a)
                })
                .collect::<Vec<_>>()
                .join(" ")
        );
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
            display,
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

/// Probes with a short timeout are used by `forge doctor` as well.
pub fn probe_command(command: &str) -> (ProbeStatus, Option<String>) {
    match crate::platform::resolve_executable(command) {
        Some(path) => {
            let (status, version, _) = probe_version(&path);
            (status, version)
        }
        None => (
            ProbeStatus::Missing {
                detail: format!("{command:?} not found on PATH"),
            },
            None,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::Duration;

    fn job() -> AgentJob {
        AgentJob {
            task_display_id: "T-007".into(),
            task_title: "Write alpha".into(),
            spec: "create alpha.txt".into(),
            worktree: PathBuf::from("/tmp/wt"),
            extra_context: BTreeMap::new(),
            timeout: Duration::from_secs(10),
        }
    }

    fn spec(extra: Vec<&str>, stdin: bool) -> AgentSpec {
        AgentSpec {
            id: "generic-1".into(),
            kind: AgentKind::Generic,
            command: "@self".into(),
            extra_args: extra.into_iter().map(|s| s.to_string()).collect(),
            capabilities: vec![],
            slots: 1,
            env: BTreeMap::new(),
            timeout_secs: None,
            enabled: true,
            stdin_prompt: stdin,
        }
    }

    #[test]
    fn prompt_appended_when_no_placeholder() {
        let prepared = GenericAdapter
            .prepare(&spec(vec!["--flag"], false), &job(), &HelpFlags::default())
            .unwrap();
        assert_eq!(prepared.args, vec!["--flag", "create alpha.txt"]);
    }

    #[test]
    fn placeholder_substitution() {
        let prepared = GenericAdapter
            .prepare(
                &spec(
                    vec!["--task", "{task}", "--dir={worktree}", "-p", "{prompt}"],
                    false,
                ),
                &job(),
                &HelpFlags::default(),
            )
            .unwrap();
        assert_eq!(
            prepared.args,
            vec!["--task", "T-007", "--dir=/tmp/wt", "-p", "create alpha.txt"]
        );
    }

    #[test]
    fn stdin_mode_keeps_args_clean() {
        let prepared = GenericAdapter
            .prepare(&spec(vec!["--flag"], true), &job(), &HelpFlags::default())
            .unwrap();
        assert_eq!(prepared.args, vec!["--flag"]);
        assert_eq!(prepared.stdin_payload.as_deref(), Some("create alpha.txt"));
    }

    #[test]
    fn display_redacts_prompt_and_secrets() {
        let prepared = GenericAdapter
            .prepare(
                &spec(vec!["--token=abcd", "{prompt}"], false),
                &job(),
                &HelpFlags::default(),
            )
            .unwrap();
        assert!(!prepared.display.contains("abcd"), "{}", prepared.display);
    }
}
