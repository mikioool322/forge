# Forge — Sprint 01 progress

Updated: 2026-10-16 (Sprint 01 complete).

Legend: `[x]` done and verified, `[~]` partial / environment-limited, `[ ]` not started.

## Phase A — Bootstrap — DONE

- Cargo workspace `forge` with crates `forge`, `forge-core`, `forge-store`,
  `forge-git`, `forge-runtime`, `forge-protocol`; dependencies flow one way.
- `forge-core` complete: domain model, explicit task state machine, DAG
  (Kahn + cycle detection), pure scheduler, TOML config + path resolution,
  plan validation, resource-claim conflict rules.
- Windows toolchain without Visual Studio: rustup
  `stable-x86_64-pc-windows-gnu` + portable LLVM-MinGW; linker pinned to
  `rust-lld` (`.cargo/config.toml`), `dlltool` for raw-dylib import libs
  (`scripts/dev-env.ps1`).

Verification: `cargo test -p forge-core` → 38 passed; clippy clean.

## Phase B — Persistence — DONE

- `migrations/0001_init.sql`: projects, plans, tasks, task_dependencies,
  agents, agent_sessions, worktrees, events, resource_leases,
  task_artifacts, meta (indexes + constraints).
- Dedicated SQLite worker thread (bounded job channel), transactional
  migrations with `PRAGMA user_version`, WAL + foreign keys + busy timeout,
  read-only `DirectDb` for CLI/doctor, `probe_database` diagnostics, typed
  repositories.

Verification: `cargo test -p forge-store` → 15 integration tests
(migrations idempotence, WAL, reopen durability, atomic transitions, lease
expiry/steal, events, worktrees, sessions, artifacts, plan idempotency, 16
concurrent writers, read-only peer while writer open, newer-schema refusal).

## Phase C — Daemon & IPC — DONE

- `forge-protocol`: typed messages, NDJSON framing (8 MiB cap), UDS
  transport with 0600 perms on Unix, loopback-TCP + mandatory session token
  on Windows, multiplexing client with notification stream.
- Daemon lifecycle: runtime dirs + owner-only permissions (Unix), single
  instance lock with stale-pid reclaim, endpoint/token/pid files, bounded
  accept loop, graceful shutdown on ctrl-c/SIGTERM/IPC, file cleanup.

Verification: `cargo test -p forge-protocol` → 14 unit + 4 integration
(handshake, version mismatch, out-of-order multiplexed responses,
notifications, disconnect wakes pending calls, and a test that serializes
every message variant — this caught a real serde internally-tagged-enum bug
with sequence payloads). Live: background daemon ready in ~150 ms, survives
shell exit, `daemon status` reports pid/endpoint/uptime.

## Phase D — Agent runtime — DONE

- Capability-probing adapters: Codex (`codex exec`; `--json`/`--cd` only when
  advertised), OpenCode 2 (`opencode run`; nothing auto-added), generic
  adapter with `{prompt}`/`{worktree}`/`{task}` templating.
- Supervisor: process groups, line streaming to size-limited rotating logs,
  bounded event channel with counted drops, wall-clock timeout,
  graceful→force escalation, exit classification
  (success/failed/auth_required/timeout/killed), stderr tail ring.
- Windows hardening: npm `.cmd` shims resolved to the real wrapped `.exe`
  (verified: cmd.exe does not preserve metacharacters — test documents the
  hazard), prompts never travel through batch argument lists.
- Controlled deterministic process `forge __demo-agent` for infrastructure
  tests (never presented as model execution).

Verification: `cargo test -p forge-runtime` → 26 unit + 5 supervisor
integration (success, auth classification, cancellation, timeout,
high-volume output). Fixed during the phase: a `select!` starvation bug
where a closed line channel spun hot and starved `child.wait()` (processes
exited but outcomes never arrived), and a seconds-vs-milliseconds bug in the
timeout safety net.

## Phase E — Scheduler & Git — DONE

- Per-attempt worktrees `<data>/worktrees/<project>/t-XXX-rN`, unique
  branches, recorded base commit, self-healing creation (prune stale
  registrations, attach existing branch), path-traversal guards, dirty
  worktrees never removed, retries use fresh attempts.
- Engine: dependency gating, capability matching, preferred agents,
  global + per-agent concurrency, leases with conflicts/expiry/heartbeat,
  idempotent cancel, retry, restart recovery (orphan kill/report,
  INTERRUPTED marking, session/lease cleanup), lost-outcome safety net,
  shutdown semantics, execution-contract artifacts.

Verification (live E2E + Rust integration tests):
- `cargo test -p forge --test daemon_e2e` → 6 tests (lifecycle + clean stop,
  concurrent plan in isolated worktrees with artifacts/logs/diff over IPC,
  cancellation, TUI detach keeps work running, crash restart reconciliation
  incl. orphan kill verification, subscribed notification delivery).

## Phase F — Ratatui TUI — DONE

- Compact lazygit/k9s-style layout: header, tasks/agents panes, output tail,
  events, key bar; rectangular borders, restrained colors, no emoji/
  banners/gradients/fake statistics.
- Keys: `j/k`, arrows, `tab`/`shift-tab` focus, `enter` inspect, `esc` back,
  `n` new task (input mode swallows shortcuts), `a` agents, `l` logs,
  `d` diff, `x` cancel with confirmation, `r` refresh, `?` help, `q` detach.
- Screens: overview, help, task detail, logs, diff, new-task dialog, confirm
  dialog. Degrades gracefully at small sizes (agents pane hidden < 90 cols,
  events hidden < 18 rows, key bar adapts).

Verification: 12 input/state unit tests + 13 `TestBackend` render tests
(80×24, 100×30, 140×40, tiny screens, empty states, disconnected banner,
long logs with scrolling, dialogs) + 3 helper tests. Live smoke run against
a running daemon: attached, rendered real task data, zero disconnects.

## Phase G — Integration — DONE

- `scripts/e2e-demo.ps1` on the release binary: **30 checks, 0 failed**
  (doctor → init → background daemon → plan with 2 concurrent + 1 dependent
  task → worktrees/diffs/canonical-repo-clean → logs → cancellation → TUI
  attach/detach/reconnect → kill -9 recovery → retry → stop/restart
  persistence).
- Real CLI validation: **OpenCode 2 (v2.0.29)** probed (`ok`) and executed a
  real task end-to-end (`hello-forge.txt` written by the model, exit 0,
  artifacts recorded, `t-001-r1` worktree diff `1 file changed, 1
  insertion`). **Codex CLI is not installed** on this machine — its adapter
  reports `missing` honestly (probe + doctor row), real Codex execution is
  NOT RUN.
- Final gates: `cargo fmt --all -- --check` clean; `cargo clippy --workspace
  --all-targets -- -D warnings` clean; `cargo test --workspace` → **145
  passed, 0 failed**; `cargo build --release` OK (`forge.exe` 10.4 MB).
- Repository initialized and committed (`18c7772`).

## Outstanding known limitations

- Unix-only code paths (UDS, `killpg`, permission checks) compile via
  `cfg(unix)` but were **not executed** in this environment (Windows);
  Linux/macOS verification of those paths is pending.
- Codex real execution not run (CLI absent).
- No orchestrator, no automated review gates (by design for Sprint 01).
- Git worktrees are not a security sandbox; agents run with user privileges.

## Next steps (Sprint 02)

- Intelligent orchestrator (configurable frontier model) turning feature
  specs into task graphs.
- Review/QA agent tasks and acceptance gates on artifacts.
- Module/file-level claim coordination on top of `resource_leases`.
- Unix CI (Linux + macOS) running the existing integration suite.
