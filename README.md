# Forge

Forge is a local-first, terminal-native software factory that orchestrates
multiple AI coding agents. This repository contains **Sprint 01**: the
reliable execution infrastructure (daemon, task engine, agent runtime, Git
worktree isolation, SQLite persistence, IPC, TUI) on top of which the
intelligent orchestrator will be built in later sprints.

Sprint 01 deliberately ships **no** LLM orchestrator: plans are deterministic
TOML files (`forge run plan.toml`), and the daemon executes them through
configured agent CLI adapters.

## Requirements

* Rust stable (1.85+). Tested with `stable-x86_64-pc-windows-gnu` 1.99.
* `git` on `PATH`.
* A Unix-like OS (macOS/Linux) uses Unix domain sockets; Windows uses a
  loopback TCP endpoint with a mandatory session token (documented deviation,
  see `docs/DECISIONS.md`).
* Optional: Codex CLI and/or OpenCode 2 installed and already authenticated
  through their own official mechanisms.

### Building on Windows without Visual Studio

```powershell
# one-time: rustup + portable LLVM-MinGW (see scripts/dev-env.ps1)
. .\scripts\dev-env.ps1
cargo build --release
```

`scripts/dev-env.ps1` puts `rustup` and LLVM-MinGW on `PATH`; the linker is
pinned to the toolchain's own `rust-lld` in `.cargo/config.toml`, and
`dlltool` (raw-dylib import libraries) comes from LLVM-MinGW.

## Quick start

```bash
cargo build --release
cd /path/to/your/repo
forge init                 # registers the project, writes ~/.config/forge/config.toml
$EDITOR ~/.config/forge/config.toml   # add [[agents]] entries
forge doctor               # honest environment diagnostics
forge daemon start --background
forge run examples/demo-plan.toml
forge status
forge tui                  # attach the terminal UI
```

The TUI is a *client*: quitting it (`q`) never stops running tasks.

## CLI

| Command | Description |
| --- | --- |
| `forge init [--name N] [--root PATH]` | Register the current Git repository, create config/runtime dirs |
| `forge doctor [--json]` | Diagnose environment: git, agents, database, daemon, permissions |
| `forge daemon start [--background]` | Start the daemon (foreground by default) |
| `forge daemon stop [--drain]` | Graceful shutdown (optionally wait for running tasks) |
| `forge daemon status` | Report pid/endpoint/uptime/task counts |
| `forge tui [--project N]` | Attach the TUI |
| `forge run <plan.toml> [--dry-run]` | Submit a deterministic task plan (idempotent per file content) |
| `forge status [--project N] [--json]` | Task list |
| `forge agents [--json]` | Agent list + probe status (works offline via local probing) |
| `forge logs <task> [-n N]` | Log tail of a task |
| `forge cancel <task> [--reason S]` | Terminate a running task (idempotent) |
| `forge retry <task>` | Re-queue a FAILED/INTERRUPTED task (fresh worktree attempt) |

## TUI

```
FORGE  project: demo-api  branch: main  agents: 2/2  connected
┌ TASKS [focus] ────────────────┐┌ AGENTS ──────────────┐
│ > T-001  RUNNING  sim-a  …    ││ > sim-a  1/1  RUNNING │
│   T-002  DONE     sim-b  …    ││   sim-b  0/1  IDLE    │
└───────────────────────────────┘└──────────────────────┘
┌ OUTPUT: T-001 / sim-a ────────────────────────────────┐
│ 21:41:08 [out] editing pricing service                │
└───────────────────────────────────────────────────────┘
┌ EVENTS ───────────────────────────────────────────────┐
│ 21:41:11 T-002 started                                │
└───────────────────────────────────────────────────────┘
n:new  enter:inspect  tab:focus  d:diff  l:logs  x:cancel  r:refresh  ?:help  q:detach
```

Keyboard: `j`/`k`/arrows move, `tab` cycles focus (tasks → agents → output),
`enter` inspects, `n` creates a task, `a` focuses agents, `l` opens logs,
`d` opens the Git diff, `x` cancels (with confirmation), `r` refreshes,
`?` help, `q` detaches. Text input mode swallows all shortcuts.

## Task plans

```toml
plan_version = 1
project = "demo-api"

[[tasks]]
key = "alpha"                    # local key for dependencies
title = "create alpha module"
spec = "Create alpha.txt."
capabilities = ["code"]          # required agent capabilities
claims = ["file:alpha.txt"]      # exclusive resource claims
deps = []                        # keys of prerequisite tasks

[[tasks]]
key = "gamma"
title = "integration report"
deps = ["alpha", "beta"]
```

Re-running the same file content is idempotent (content-derived request id).

## Where things live

| Path | Contents |
| --- | --- |
| `~/.config/forge/config.toml` (`%APPDATA%\forge`) | Configuration |
| `~/.local/share/forge` (`%LOCALAPPDATA%\forge`) | Data root (`FORGE_DATA_DIR`) |
| `<data>/state.db` | SQLite (WAL) task registry, events, artifacts |
| `<data>/logs/<project>/t-XXX-rN.log` | Raw agent output, size-limited with rotation |
| `<data>/worktrees/<project>/t-XXX-rN` | One Git worktree per task attempt |
| `<data>/run/` | Endpoint, token, pid and lock files (owner-only on Unix) |

## Known limitations (Sprint 01)

* No LLM orchestrator, no automatic code review/acceptance gates.
* Git worktrees isolate working directories — they are **not** a security
  sandbox (no process/filesystem/network isolation of agents).
* Windows uses loopback TCP + token instead of Unix sockets.
* Logs are filesystem-backed; the events table stores signal-bearing events
  only (warnings/errors), not every output line.
* Worktrees of completed/failed tasks are retained for inspection and are
  never deleted automatically.
* Unix-specific paths (UDS, process groups, permissions) are implemented but
  were verified only through platform-neutral tests on Windows; see
  `docs/SPRINT_01_REPORT.md`.

## Documentation

* `docs/ARCHITECTURE.md` — components, daemon lifecycle, state machine.
* `docs/DECISIONS.md` — design decisions and deviations.
* `docs/AGENT_ADAPTERS.md` — adapter architecture and how to add one.
* `docs/PROGRESS.md` — phase-by-phase implementation log.
* `docs/SPRINT_01_REPORT.md` — final sprint report with evidence.
