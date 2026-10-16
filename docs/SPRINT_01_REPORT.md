# Forge — Sprint 01 report

Status: **COMPLETE** (with two explicitly environment-limited items, listed
under KNOWN LIMITATIONS).

Date: 2026-10-16 · Commit: `18c7772b28aabb0c540357731a747f6ba4e97c59` ·
Host: Windows 11 x64, Rust 1.99.1 (`stable-x86_64-pc-windows-gnu`), Git 2.55.

## What was delivered

Sprint 01 built the reliable execution infrastructure of Forge: a
persistent local daemon that supervises real coding-agent CLI processes in
isolated Git worktrees, with durable SQLite state, a typed IPC protocol, a
terminal UI client, honest diagnostics, recovery semantics, and a
1900+ line test suite plus a scripted end-to-end demo.

No LLM orchestrator is included (by design). Plans are deterministic TOML
files; agents are configured, probed and dispatched without any vendor being
hardcoded.

## Verification evidence

| Gate | Command | Result |
| --- | --- | --- |
| Format | `cargo fmt --all -- --check` | clean (exit 0) |
| Lints | `cargo clippy --workspace --all-targets -- -D warnings` | clean (exit 0) |
| Tests | `cargo test --workspace` | **145 passed, 0 failed** |
| Release | `cargo build --release` | OK (`forge.exe`, 10 452 992 bytes) |
| E2E demo | `scripts/e2e-demo.ps1` | **30 checks, 0 failed** |
| Real agent | OpenCode 2 v2.0.29 (isolated repo) | task **COMPLETED**, exit 0, artifacts + diff verified |
| Real agent | Codex CLI | **NOT RUN** — not installed; adapter reports `missing` |

Test breakdown (145): forge-core 38 · forge-store 15 · forge-git 7 ·
forge-protocol 14 + 4 · forge-runtime 26 + 5 · forge (TUI/state/util) 30 ·
forge daemon integration 6.

## Real end-to-end evidence (E2E script, release binary)

```
== submit plan (2 independent + 1 dependent task) ==
  plan #1 accepted
  1  COMPLETED  agent=sim-a     create alpha module
  2  COMPLETED  agent=sim-b     create beta module
  3  COMPLETED  agent=sim-a     integration report        (depended on 1+2)
  PASS  two different agents used
  PASS  tasks overlapped in time delta=...ms
== worktree isolation and diffs ==
  PASS  worktree for T-001 exists / T-002 exists / T-003 exists
  PASS  worktrees are isolated (no cross-task files)
  PASS  canonical repository untouched
  PASS  diff shows alpha.txt      (alpha.txt | 2 ++)
== logs ==       PASS log contains agent output / process exit
== cancellation ==
  PASS  cancelled task is CANCELLED / cancellation is stable
== TUI attach / detach / reconnect ==
  PASS  TUI attached to the daemon
  PASS  daemon survives TUI detach
  PASS  TUI reconnects and renders task data
== crash recovery (kill -9 the daemon while a task runs) ==
  PASS  task marked INTERRUPTED after crash ("daemon restarted while the task was active")
  PASS  recovery events recorded
  PASS  retry re-queues the task / retried task dispatched again
== daemon stop and restart persistence ==
  PASS  endpoint file removed after stop
  PASS  tasks persisted across daemon restart / completed tasks still COMPLETED

30 checks, 0 failed
```

Real model execution (OpenCode 2, isolated temp repo, `forge retry T-001`):

```
state=COMPLETED agent=opencode-b
summary=exit=Some(0) class=success files=1 +1 -0 dropped=0
worktree t-001-r1/hello-forge.txt  →  "hello from opencode"
git diff --stat (worktree): hello-forge.txt | 1 +
canonical repo: clean
log: 00:07:15 [sys] $ opencode run --auto <prompt>
     00:07:18 [out] Created `hello-forge.txt` with exactly one line: `hello from opencode`. …
     00:07:19 [sys] process exited: code=Some(0) class=success duration_ms=4084
```

The controlled process (`forge __demo-agent`) is clearly separated from real
model execution: demo agents are named `sim-*`, write deterministic files and
are used only for infrastructure verification.

## Bugs found and fixed during implementation (highlights)

1. `select!` starvation in the supervisor: after the output channel closed,
   `recv()` returned `None` immediately and starved the `child.wait()`
   branch — processes exited but no outcomes arrived (tasks stuck RUNNING).
   Fixed with a disabled-branch flag; regression covered by tests.
2. serde internally-tagged enums silently fail on newtype variants holding
   sequences (`AgentsChanged(Vec<..>)`), killing TUI connections with
   "cannot serialize…". Fixed (struct variants) and guarded by a test that
   serializes every protocol variant.
3. Safety-net timeout unit bug (seconds + milliseconds) — fixed, with
   lost-outcome detection using a grace period.
4. Detached daemon inherited a capturing shell's pipe handles, so
   `$out = forge daemon start --background` never returned. Fixed by
   clearing the inheritance flag on the CLI's std handles around the spawn
   (`SetHandleInformation`) and having the daemon write its own log file.
5. Windows: extensionless npm scripts shadowed `opencode.cmd`; `.cmd` shims
   with arguments are refused by Rust (and cmd.exe does not preserve
   metacharacters). Fixed by resolving npm shims to the real wrapped
   executable and routing prompts away from batch argument lists.
6. Stale git worktree registrations (after deleting the data dir) blocked
   worktree creation; creation is now self-healing (prune + retry + attach).

## Known limitations

* **Unix paths not executed here.** UDS transport, `killpg`, and Unix
  permission handling are implemented behind `cfg(unix)` and compile as
  parts of the unix-only code, but this environment is Windows; Linux/macOS
  execution of those paths is unverified (all other logic is
  platform-neutral and tested).
* **Codex real execution: NOT RUN** (CLI not installed; probe reports
  `missing`).
* Worktrees are not a sandbox; no process/filesystem/network isolation.
* No orchestrator, no review gates (explicitly out of scope for Sprint 01).
* Logs are filesystem-backed; the events table stores signal-bearing events
  only.
* Retained worktrees are never garbage-collected automatically.

## Recovery procedure after interrupted execution

1. `forge doctor` — reports interrupted/failed tasks and database health.
2. `forge status` — the affected tasks are `INTERRUPTED` with a recorded
   reason (orphan processes are killed by default during recovery;
   `recovery.orphan_action = "report"` to only report them).
3. Inspect retained worktrees under `<data>/worktrees/<project>/` and the
   task log; decide whether the partial work is useful.
4. `forge retry <task>` — re-queues with a fresh attempt (new worktree,
   new branch); the previous worktree stays for inspection.
