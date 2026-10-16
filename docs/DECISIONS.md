# Design decisions (Sprint 01)

Short ADR-style notes. Dates: 2026-10-15/16. Environment: Windows 11 x64
(development), targets macOS arm64 / Linux x86_64+arm64.

## D1 — Cargo workspace, six crates

`forge-core` (pure domain), `forge-store`, `forge-git`, `forge-runtime`,
`forge-protocol`, `forge` (binary: CLI + daemon + TUI). Dependencies flow one
way; `forge-core` has no IO, which keeps the state machine, DAG and scheduler
trivially testable. The daemon lives in the binary crate because it composes
all lower layers and shares the process model with the CLI commands.

## D2 — Daemon owns state; TUI is a client

Closing the TUI must not stop work, and CLI commands must see one consistent
view. Therefore: single-writer daemon, SQLite worker thread, IPC for all
mutations. One-shot CLI operations (`forge init`, `forge doctor`) use a
separate read-only/direct connection only when the daemon is not running.

## D3 — Unix sockets on Unix, loopback TCP + token on Windows

The spec requires Unix domain sockets. Rust std/Tokio do not expose AF_UNIX
on Windows. Rather than degrade silently, the endpoint abstraction has two
backends: UDS (0600, peer permissions) on Unix; `tcp:127.0.0.1:0` with a
random port advertised in the run directory plus a 256-bit bearer token
enforced during the handshake on Windows. `forge doctor` reports which one is
in use. **No Linux/macOS verification of the UDS path was performed in this
environment** — it is covered by `cfg(unix)` code paths and unit tests that
compile on Unix, but only Windows was actually executed (see the report).

## D4 — NDJSON framing

One JSON object per line, 8 MiB cap, `Frame::{request,response,notify}`.
Rationale: debuggable with `nc`/files, no codec dependencies, trivially
versioned in the handshake. Internally tagged enums are used for messages;
newtype variants containing sequences are forbidden by serde there, so such
payloads are struct variants (`AgentsChanged { agents }`,
`Projects { projects }`) — a protocol test serializes every variant to catch
this class of bug.

## D5 — SQLite via rusqlite (bundled), WAL, single writer thread

Bundled SQLite avoids system dependencies on all targets. The daemon runs one
worker thread owning the write connection; async callers use a bounded job
channel. WAL + busy timeout + foreign keys are set per connection; migrations
bump `PRAGMA user_version` transactionally; a newer schema is refused.

## D6 — Filesystem-backed logs, selective event journaling

Full process streams must not grow the database without bound and should be
inspectable with standard tools, so raw output goes to per-task log files
(size-limited, rotated) and only signal-bearing events are persisted. The TUI
and `forge logs` read log tails with byte caps.

## D7 — Deterministic plans instead of an LLM orchestrator

Sprint 01 ships no intelligent orchestrator by design. `forge run plan.toml`
takes a deterministic dependency-aware plan; the request id is derived from
file content, making resubmission idempotent (same tasks, no duplicates).
Task models/providers stay fully configurable; nothing hardcodes a vendor.

## D8 — Adapters: capability probing, no invented flags

Adapters only pass flags that the *installed* CLI advertises in its own
`--help` output (parsed at probe time and cached). Codex uses `codex exec`
(documented non-interactive mode) and adds `--json`/`--cd` only when
advertised. OpenCode uses `opencode run`; nothing beyond the subcommand is
auto-added (`--auto`, `--format json`, `--standalone`, `--model` are
user-opted `extra_args`). Authentication is the CLI's own official session;
Forge never reads credential stores. Auth-looking failures are classified
`AUTH_REQUIRED` instead of a generic failure.

## D9 — Windows `.cmd` shims are resolved to real executables

npm installs `.cmd` shims; `cmd.exe` re-parses batch command lines, so
metacharacters in arguments (`&`, `|`, `^`, `%VAR%`, quotes) cannot be
escaped reliably (verified by a test that documents the hazard). Therefore
`resolve_executable` parses npm shims and uses the real `.exe` they wrap;
where an arbitrary batch file must be used, the prompt is routed to stdin
instead of argv, and the supervisor validates argv against cmd
metacharacters.

## D10 — Leases and claims in one table

`resource_leases(resource UNIQUE, task_id, expires_at, …)` implements
exclusive claims with expiry + heartbeat directly in SQL (single-writer
daemon makes this safe). The scheduler treats claims as a pure conflict
predicate (`file:`/`dir:` prefix rules), so the coordination model can evolve
(file-level ownership, module-level coordination) without touching the
scheduler's interface.

## D11 — Execution contract, no fake success

"Exit code 0" alone is not proof of work. A successful task records its
artifacts (diff stat/summary, exit code, duration, log path, command); an
empty diff produces a WARN event ("completed with an EMPTY diff") rather than
silent success. Review gates belong to later sprints.

## D12 — Retries create new attempts

Retry re-queues a FAILED/INTERRUPTED task and increments `attempts`; the new
attempt gets a fresh worktree and branch (`t-XXX-rN`). The previous worktree
is retained for inspection. Nothing merges into the user's branches
automatically.

## D13 — Controlled demo process instead of fabricated agent output

E2E infrastructure tests use `forge __demo-agent`: a real subprocess that
writes real files, prints progress, can fail, can simulate auth failures and
can hang (cancellation/timeouts). It is explicitly labelled as a controlled
process (name `sim-*` in the demo config) and never presented as model
execution. Real model runs are performed with the actual CLIs and reported
separately.

## D14 — Detached daemon spawning on Windows

`forge daemon start --background` spawns the daemon with
`DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB`
(retrying without breakaway when the job forbids it) and, crucially,
temporarily clears the inheritance flag on the CLI's own std handles
(`SetHandleInformation`) so the daemon does not inherit a capturing shell's
pipes (otherwise `$out = forge daemon start --background` never returns).
The daemon writes its own log file directly (`FORGE_DAEMON_LOG`).

## D15 — TUI structure and vocabulary

Ratatui + crossterm, keyboard-first, information-dense: rectangular borders,
restrained colors, no emoji/banners/gradients/fake progress. Pure
state/render functions enable `TestBackend` tests at multiple sizes.
Shortcuts never fire while text input is active; cancellation always asks for
confirmation; `q` detaches without stopping work.
