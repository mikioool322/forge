# Agent adapters

Forge talks to AI coding agents through a vendor-independent adapter
interface (`forge-runtime::adapter::AgentAdapter`). Sprint 01 ships three
adapters plus a probe pipeline and a supervisor.

## Interface

```rust
pub trait AgentAdapter: Send + Sync {
    fn kind(&self) -> AgentKind;                       // codex | opencode | generic
    fn probe(&self, spec: &AgentSpec) -> ProbeOutcome; // version + advertised flags
    fn prepare(&self, spec, job, help) -> Result<PreparedCommand, RuntimeError>;
    fn classify_line(&self, stream, line) -> Option<NormalizedEvent>;
    fn classify_exit(&self, code, stderr_tail) -> ExitClass;
}
```

* `PreparedCommand` is a program path + argument array + cwd + env +
  optional stdin payload. **Arguments are never passed through a shell.**
* `ExitClass` ∈ { `success`, `failed`, `auth_required`, `timeout`, `killed` }.

## Configuration

```toml
[[agents]]
id = "codex-a"            # must match [A-Za-z0-9._-]+
kind = "codex"            # codex | opencode | generic
command = "codex"         # PATH lookup, absolute path, or "@self" (this binary)
extra_args = []           # documented flags the user opts into
capabilities = ["code"]   # tasks require subsets of these
slots = 1                 # concurrent tasks for this agent
timeout_secs = 900        # per-task wall clock (overrides daemon default)
stdin_prompt = false      # send the prompt on stdin instead of argv
env = { }
```

## Probing (how "supported" is determined)

Probing runs at daemon start (and in `forge doctor` / `forge agents`):

1. resolve the executable (`@self` → this binary; npm `.cmd` shims are
   resolved to the real `.exe` they wrap),
2. `--version` / `-V` (first version-looking line wins, 5 s timeout),
3. adapter-specific subcommand help (`codex exec --help`,
   `opencode run --help`) to learn **advertised flags**; flags are cached and
   used by `prepare`.

Empirically verified in this environment (Windows):

| CLI | Detected | Notes |
| --- | --- | --- |
| `opencode` v2.0.29 | yes | `run` subcommand; executed a real task (see report) |
| `codex` | not installed | probe reports `missing`, tasks would stay QUEUED |

## Codex adapter

* Invocation: `codex exec [--json] [--cd <worktree>] <prompt>`.
* `--json` and `--cd` are added **only** if the installed `codex exec --help`
  advertises them; otherwise the prompt is the sole positional argument and
  the worktree is the process cwd.
* Approval/sandbox flags (`--full-auto`, `--dangerously-*`, …) are never
  invented; users who want them add them explicitly via `extra_args`.
* Authentication: the Codex CLI's own session. A failure whose tail matches
  login/auth markers and exits non-zero is classified `AUTH_REQUIRED`.
* JSONL stdout (when `--json` is used) is normalized via `classify_json_line`;
  plain text lines are classified heuristically (error/warn keywords).

## OpenCode 2 adapter

* Invocation: `opencode run [extra_args…] <prompt>` (documented usage:
  `opencode run [flags] [<message...>]`).
* Nothing is auto-added beyond `run`; `--auto` (auto-approve permissions not
  explicitly denied), `--format json`, `--standalone`, `--model`, `-c/--continue`,
  `--print-logs` are documented flags users may add via `extra_args`. The
  probe records which of them the installed version advertises.
* Recommended non-interactive configuration (used in the real E2E):
  `extra_args = ["--auto"]`.
* The prompt may be sent on stdin (`stdin_prompt = true`) — verified working
  with `opencode run`, which is also the safe mode for batch-file shims.

## Generic adapter

For any CLI of your own (or the bundled controlled process
`forge __demo-agent`):

* `extra_args` support placeholders: `{prompt}`, `{worktree}`, `{task}`,
  `{title}` (unknown placeholders pass through).
* If no `{prompt}` placeholder is present and `stdin_prompt = false`, the
  prompt is appended as the final positional argument.
* If the resolved command is a Windows batch file, the implicit prompt is
  routed to stdin instead (cmd.exe cannot transport arbitrary text safely),
  while explicit `{prompt}` templates are honoured as written.

## Events and classification

* Every non-empty output line becomes a normalized event (JSON lines with
  `type/message/msg/text` are structured; the rest are keyword-heuristically
  classified with `{"heuristic": true}` in the payload).
* The engine persists only signal-bearing events (severity ≥ warn or
  non-output kinds); the raw stream lives in the task log file.
* Exit classification: `0` → success; non-zero with auth markers in the
  stderr tail → `auth_required`; killed by Forge → `killed`; wall-clock
  exceeded → `timeout`; otherwise `failed`.

## Writing a new adapter

1. Add a `kind` to `forge_core::AgentKind` (serde name = config value).
2Impl `<name>.rs` implementing `AgentAdapter`; use `probe_version` +
   `HelpFlags::from_help_text` for probing and gate every optional flag on
   `help.has(...)`.
3. Register it in `adapter_for`.
4. Add unit tests for argument shaping and exit classification, plus a
   supervisor integration test using a deterministic process.

## Boundaries

* Forge never reads or extracts subscription tokens or private auth stores.
* Forge does not pass undocumented flags and does not claim flag support
  without probing.
* Forge never claims that adapters are sandboxed: agents run with the user's
  own privileges (see docs/ARCHITECTURE.md, SECURITY NOTES).
