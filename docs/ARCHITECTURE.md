# Forge architecture (Sprint 01)

## Overview

```
┌────────────────────────────┐        ┌─────────────────────────────────────┐
│  forge (CLI + TUI client)  │        │  forge daemon (persistent process)  │
│  ─ init / doctor / status  │  IPC   │  ─ engine (scheduler + state)       │
│  ─ run / logs / cancel     │◄──────►│  ─ SQLite worker thread             │
│  ─ ratatui TUI (client)    │  UDS   │  ─ agent supervisors                │
└────────────────────────────┘  TCP   │  ─ Git worktree lifecycle           │
                                (Win) │  ─ recovery on startup              │
                                      └──────────────┬──────────────────────┘
                                                     │ process groups
                                      ┌──────────────▼──────────────────────┐
                                      │ codex / opencode / generic adapters │
                                      │  (one supervised process per task)  │
                                      └──────────────┬──────────────────────┘
                                                     │ cwd = worktree
                                      ┌──────────────▼──────────────────────┐
                                      │ Git worktree per task attempt       │
                                      └─────────────────────────────────────┘
```

Crates (dependencies flow downwards only):

| Crate | Responsibility |
| --- | --- |
| `forge-core` | Domain model, task state machine, DAG, scheduler (pure), config schema + path resolution, plan validation, claim conflict rules |
| `forge-store` | SQLite (WAL) persistence: migrations, typed repositories, dedicated DB worker thread |
| `forge-git` | Git CLI wrapper: worktree lifecycle, diffs, dirty checks, path safety |
| `forge-runtime` | Agent adapters (codex/opencode/generic), probing, subprocess supervision, platform process helpers |
| `forge-protocol` | Typed IPC messages, NDJSON framing, Unix-socket/loopback-TCP transport, multiplexing client |
| `forge` | `forge` binary: CLI commands, daemon host (engine + connections), TUI, `doctor`, controlled demo process |

## Daemon

The daemon owns everything that must outlive a TUI session:

* **Engine** — a single task with one `select!` loop over: control messages
  from IPC connections, supervisor events, supervisor outcomes, and a 500 ms
  maintenance tick. There is no busy polling; the tick only runs on a timer
  and performs lease heartbeats, lease expiry, timeout safety checks and
  scheduling.
* **Store worker** — one OS thread owns the single write connection. Async
  callers submit closures over a bounded channel (`Store::call`), so SQLite
  never blocks a Tokio executor thread and memory is back-pressured.
* **Connections** — one task per client, bounded by
  `daemon.connection_limit`; request handling is spawned with a small
  per-connection permit limit so notifications keep flowing during slow
  queries. Writes are serialized through a single writer.
* **Supervisors** — one task per running task; spawns the agent in its own
  process group, streams stdout/stderr into a size-limited log file, forwards
  normalized events into a bounded channel (counted drops), enforces the
  wall-clock timeout, escalates graceful→forceful termination and reports an
  outcome.
* **Recovery** — on startup: orphan process handling (`kill`/`report`),
  session closure, lease release, worktree retention, `INTERRUPTED` marking,
  and a recovery event per affected task.
* **Single instance** — a lock file with pid; stale locks (dead pid) are
  reclaimed. The endpoint (Unix socket path or TCP port) and the session
  token are advertised in the runtime directory with owner-only permissions
  on Unix.

Shutdown semantics:

* `forge daemon stop` → graceful: supervisors are asked to terminate, running
  tasks become `INTERRUPTED` (reason recorded), leases are released, sessions
  closed, events flushed, endpoint/lock files removed.
* `forge daemon stop --drain` → the engine stops dispatching new tasks and
  exits when running tasks finish (bounded by `shutdown_drain_secs`, min 300).
* Crash → recovery on next start (see above); the SQLite WAL guarantees the
  database is consistent.

## Task state machine

```
CREATED ─► READY ─► LEASED ─► RUNNING ─► REVIEW_PENDING ─► COMPLETED
   │         │        │         │              │
   │         │        │         ├─► FAILED ────┘ (retry: FAILED → READY)
   │         │        │         ├─► CANCELLED
   │         │        └─────────┴─► INTERRUPTED (retry: → READY)
   │         └─► BLOCKED (dependency failed/cancelled; → READY when unblocked)
   └─► BLOCKED / CANCELLED
```

Legal transitions are defined exhaustively in `forge-core::state`; every
state change goes through `UPDATE ... WHERE id=? AND state=?` so a stale
writer can never corrupt the machine. In Sprint 01 the "execution contract"
is: process exited 0 **and** artifacts recorded (git diff stat/summary, exit
code, duration, log path, command). Empty diffs complete with a `WARN` event
instead of pretending success. Automated review gates are a later sprint.

## Scheduling

`forge-core::scheduler::schedule` is a pure function over an in-memory view:

1. `CREATED` tasks whose dependencies are all `COMPLETED` → `Promote`.
   Dependencies that are `FAILED`/`CANCELLED` → `Block`.
2. `READY` tasks are dispatched in priority/sequence order to the first
   available agent with matching capabilities (and the preferred agent when
   configured), respecting per-agent slots, global `max_concurrency`, and
   resource claims: a task is never dispatched while another task holds an
   overlapping claim (`file:`, `dir:`, `resource:` with prefix semantics).

Leases (SQLite + in-memory mirror) implement the claims, with heartbeat,
expiry and takeover; the scheduler only ever sees conflicts, never raw SQL.

## Worktrees

One worktree per task **attempt**: `<data>/worktrees/<project>/t-XXX-rN`,
branch `<prefix><project>/t-XXX-rN`, base commit recorded. Creation is
self-healing (stale git registrations are pruned and retried; an
already-created branch is attached without `-b`). Dirty worktrees are never
removed, and the canonical repository is never modified by tasks. Retries
use a fresh attempt (new worktree, new branch).

**Git worktrees are isolation of working directories, not a sandbox.** See
SECURITY NOTES below.

## Events, logs and artifacts

* The **raw output stream** of each task lives in
  `<data>/logs/<project>/t-XXX-rN.log` (`timestamp \t stream \t text` lines),
  size-limited with a single `.log.1` rotation.
* The **events table** stores signal-bearing events only (state changes,
  warnings/errors from agent output, dispatch/exit/recovery notes), which
  keeps database growth proportional to signal rather than to output volume.
* **Artifacts** recorded per task: `worktree`, `git_diff_stat`,
  `git_diff_summary` (files/added/deleted/paths), `exit_code`, `duration_ms`,
  `log_file`, `command`.

## IPC

Newline-delimited JSON frames with an 8 MiB cap. Frames are
`request`/`response`/`notify`; requests are multiplexed by id, and
notifications are pushed to subscribed clients (task changes, agent changes,
events, resync). The handshake exchanges protocol versions; mismatches fail
with a clear message. Clients that lag behind receive `Resync` and refetch a
snapshot.

Transport: Unix domain sockets (mode 0600) on Unix; loopback TCP on random
port **plus a mandatory 256-bit session token** on Windows (documented
deviation).

## TUI

Ratatui + crossterm. The TUI is a pure client: it holds an `App` state
updated from snapshots/notifications, renders it, and turns keys into
effects (`FetchLogs`, `CancelTask`, …) executed via IPC. Rendering and key
handling are pure functions, so both are covered by `TestBackend` tests at
80×24, 100×30 and 140×40, including degraded layouts, empty states, long
logs and dialogs.

## Security notes (Sprint 01)

* Agents run as the user's own OS user, with the user's credentials and
  network access. **There is no process, filesystem or network sandbox.**
  Worktrees only isolate working directories and branches.
* No shell interpolation anywhere: all external programs are executed with
  argument arrays; on Windows, `.cmd` shims are resolved to the real
  executables they wrap (`resolve_npm_shim`) precisely to avoid cmd.exe
  re-parsing of arguments.
* Secrets are never read from agent credential stores; agent authentication
  is the CLI's own session. Diagnostics redact `token|secret|password|api_key`
  patterns.
* Runtime files: data dir/run dir/logs/worktrees are 0700 on Unix; endpoint
  and token files 0600; the socket is created 0600.
* Resource limits: bounded channels everywhere (jobs 1024, run events 2048,
  per-run lines 512, notifications 1024), max line length, capped log files,
  capped diff/log tails, bounded concurrency (global + per-agent), bounded
  connections, capped in-flight requests per connection.
