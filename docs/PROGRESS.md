# Forge — Sprint 01 progress

Updated: 2026-10-16 (phase F in progress).

Legend: `[x]` done and verified, `[~]` partially done, `[ ]` not started.

## Phase A — Bootstrap — DONE

- Cargo workspace `forge` with crates: `forge` (CLI+TUI+daemon), `forge-core`,
  `forge-store`, `forge-git`, `forge-runtime`, `forge-protocol`.
- Windows toolchain set up without Visual Studio: rustup
  `stable-x86_64-pc-windows-gnu` + portable LLVM-MinGW; linker pinned to
  `rust-lld` (`.cargo/config.toml`), `dlltool` from LLVM-MinGW for
  raw-dylib import libraries.
- `forge-core` complete: domain model, explicit task state machine, DAG
  (Kahn), deterministic scheduler, TOML config + path resolution,
  plan validation, claim conflict rules.

Verification:
- `cargo test -p forge-core` → 38 passed.
- `cargo clippy -p forge-core --all-targets -- -D warnings` → clean.

## Phase B — Persistence — DONE

- `migrations/0001_init.sql` (projects, plans, tasks, task_dependencies,
  agents, agent_sessions, worktrees, events, resource_leases,
  task_artifacts, meta) with indexes and constraints.
- `forge-store`: dedicated DB worker thread (bounded channel, no SQLite on
  async threads), transactional migrations with `PRAGMA user_version`,
  WAL + foreign keys + busy timeout, read-only `DirectDb` for CLI/doctor,
  `probe_database` for diagnostics, typed repositories.

Verification:
- `cargo test -p forge-store` → 15 integration tests passed (migrations,
  WAL, reopen durability, transition atomicity, lease expiry/steal,
  events, worktrees, sessions, artifacts, plan idempotency, 16 concurrent
  async writers, read-only peer while writer open, newer-schema refusal).

## Phase C — Daemon & IPC — DONE

- `forge-protocol`: typed requests/responses/notifications, NDJSON framing
  with 8 MiB cap, Unix socket transport (0600) with loopback-TCP + token
  fallback on Windows, multiplexing client with notification stream.
- Daemon lifecycle: runtime dirs, owner-only permissions (Unix), single
  instance lock with stale-lock reclaim, endpoint + token + pid files,
  bounded accept loop (connection semaphore), graceful shutdown on
  ctrl-c/SIGTERM/IPC, endpoint cleanup.

Verification:
- `cargo test -p forge-protocol` → 12 unit + 4 integration tests passed
  (handshake, protocol mismatch, out-of-order multiplexed responses,
  notifications, disconnect wakes pending calls).
- Live: `forge daemon start --background` → ready in ~160 ms; daemon
  survives shell exit; `forge daemon status` reports pid/endpoint/uptime.

## Phase D — Agent runtime — DONE

- Adapter trait with capability probing from real `--help` output (no
  invented flags), Codex (`codex exec`, `--json`/`--cd` only when
  advertised), OpenCode 2 (`opencode run`, version detection), generic
  adapter with `{prompt}`/`{worktree}`/`{task}` templating.
- Supervisor: process groups, line streaming into size-limited rotating
  log files, bounded event channel with counted drops, wall-clock timeout,
  graceful→force kill escalation, exit classification
  (success/failed/auth-required/timeout/killed), stderr tail ring.
- Bundled deterministic controlled process `forge __demo-agent` for
  infrastructure tests (clearly separated from real model execution).

Verification:
- `cargo test -p forge-runtime` → 23 unit + 5 integration tests passed
  (success, auth classification, cancellation, timeout, high-volume
  output).
- Fixed during phase: select! starvation bug where a closed line channel
  spun hot and starved `child.wait()`; safety-net unit bug (seconds vs ms).

## Phase E — Scheduler & Git — DONE

- Worktrees: unique per task/attempt, branch prefix configurable, base
  commit recorded, self-healing on stale registrations (prune + retry +
  attach existing branch), dirty worktrees never removed, path traversal
  guards.
- Engine: single-owner loop (control messages, supervisor events,
  outcomes, 500 ms tick), dependency gating, capability matching,
  preferred agents, global + per-agent concurrency, resource leases with
  conflicts/expiry/heartbeat, cancellation (idempotent), retry with new
  worktree attempt, restart recovery (orphan kill/report, INTERRUPTED,
  session/lease cleanup), lost-outcome safety net, shutdown semantics.
- Execution contract for sprint 1: exit status + artifacts recorded
  (git diff stat/summary, exit code, duration, log file, command).

Verification (live E2E in an isolated temp Git repo):
- Plan with two independent tasks → both dispatched concurrently to two
  agents, two worktrees (`t-001-r1`, `t-002-r1`), files created inside
  the worktrees, canonical repository clean.
- `forge retry` re-dispatched FAILED tasks; `forge run` replay of the same
  plan file is idempotent (same request id → same tasks).
- `forge cancel T-003` on a long-running task → CANCELLED in ~2 s, no
  orphan processes left.
- Worktree diff inspection via `git diff` in the worktree
  (`alpha.txt | 2 ++`), log tail via daemon.

## Phase F — Ratatui TUI — IN PROGRESS

Implemented next; see docs/SPRINT_01_REPORT.md for the final state.
