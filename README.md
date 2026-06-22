# ferrus

[![Ferrus version](https://img.shields.io/badge/ferrus-0.3.0--alpha.1-orange)](https://crates.io/crates/ferrus)
[![Rust version](https://img.shields.io/badge/rustc-1.95+-964B00)](https://releases.rs/docs/1.95.0/)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](https://github.com/RomanEmreis/ferrus/blob/main/LICENSE)
[![Rust](https://github.com/RomanEmreis/ferrus/actions/workflows/rust.yml/badge.svg)](https://github.com/RomanEmreis/ferrus/actions/workflows/rust.yml)
[![Publish](https://github.com/RomanEmreis/ferrus/actions/workflows/publish.yml/badge.svg)](https://github.com/RomanEmreis/ferrus/actions/workflows/publish.yml)

**Deterministic orchestration of AI agents for real software work.**

Ferrus turns coding agents into controlled, repeatable workers.

It runs a Supervisor → Executor → Reviewer loop over your repository — not as a chat, but as a **state machine**.  
Tasks are planned, implemented, checked, and reviewed in a structured, restart-safe flow. Unlike chat-based agents, ferrus enforces structure and lifecycle.

Everything is explicit:
- Runtime state lives in SQLite; task context lives in scoped Markdown artifacts
- Agents are stateless between runs
- Crashes are recoverable
- No hidden context

## Supported agents

Ferrus works with existing coding agents:

- **Codex**
- **Claude Code**
- **Qwen Code** (experimental)

Agents are treated as interchangeable workers — ferrus provides the runtime, coordination, and state.

Internally, agent support is normalized through `src/agents/`: `mod.rs` defines the shared Supervisor/Executor contracts and MCP config entry shape, while `claude/`, `codex/`, and `qwen/` adapt each CLI's launch flags, model overrides, headless prompt transport, and local permission/config conventions.

> 💡 **Status**: ferrus is currently in alpha and not ready for production.

[Tutorial](https://ferrus.dev) | [Roadmap](https://github.com/RomanEmreis/ferrus/blob/main/docs/milestones.md)

---

## How it works

```
  you
   │
   └─► ferrus HQ
         │
         ├─► Supervisor (Claude Code or Codex) — plans tasks
         │         │ exits after task created;
         │
         ├─► Executor (Claude Code or Codex)   — implements, checks, submits
         │         │ runs headlessly
         │
         └─► Reviewer (Claude Code or Codex)   — spawned automatically on submission
                   │ exits after approve/reject; runs headlessly
```

HQ watches state transitions and spawns the right agent at the right time.

State is coordinated through `ferrus.db`, with human-readable task context under `.ferrus/tasks/` and `.ferrus/runs/`. If an agent crashes and restarts, Ferrus can recover its run and task lease without reconstructing state from Markdown files.

---

## Quick start

Install:

```sh
cargo install ferrus
# or on Linux/macOS:
curl -fsSL https://github.com/RomanEmreis/ferrus/releases/latest/download/install.sh | sh
```

```powershell
# or on Windows:
iwr https://github.com/RomanEmreis/ferrus/releases/latest/download/install.ps1 -useb | iex
```

Run:

```sh
ferrus init                                                # scaffold ferrus.toml, .ferrus/, and ~/.ferrus project state
ferrus register --supervisor claude-code --executor codex  # write agent configs and tool permissions
ferrus                                                     # enter HQ
```

Then type `/task` — a supervisor spawns, you describe what you want, and the full loop runs automatically.

On Linux and macOS for `x86_64` and `aarch64`/`arm64`, `install.sh` downloads the matching release binary into `~/.local/bin` by default. On Windows, `install.ps1` installs `ferrus.exe` into `%LOCALAPPDATA%\ferrus\bin` by default. Release archives are verified with published SHA-256 checksums before installation. Set `FERRUS_INSTALL_DIR` to override the destination, or `FERRUS_INSTALL_VERSION=vX.Y.Z` to install a specific release tag.

---

## HQ

`ferrus` with no arguments opens an interactive shell:

| Command | Description |
|---|---|
| `/plan` | Free-form planning session with the supervisor (no task created) |
| `/task` | Define a task from the selected milestone, then run the executor→review loop automatically |
| `/task --manual` | Define a free-form task without selected milestone context |
| `/spec` | Draft, approve, and save a feature specification |
| `/milestones` | Select the current spec and milestone |
| `/reset-spec` | Clear the selected spec and milestone |
| `/check` | Run configured checks from HQ when the current workflow permits; does not mutate task state |
| `/check --force` | Run configured checks from HQ regardless of task status; does not mutate task state |
| `/supervisor` | Open an interactive supervisor session (no initial prompt) |
| `/executor` | Open an interactive executor session (no initial prompt) |
| `/resume` | Manually resume the executor headlessly; also recovers Consultation by relaunching both supervisor and executor |
| `/review` | Manually spawn supervisor in review mode (escape hatch when automatic spawning failed) |
| `/status` | Show task state, agent list, and session log paths |
| `/tasks` | List SQLite task runtime rows |
| `/runs [--limit N]` | List SQLite run attempts |
| `/events [--limit N] [--run <id>]` | List SQLite runtime events |
| `/attach <name>` | Show log path for a running headless agent |
| `/stop` | Stop all running agent sessions (prompts for confirmation) |
| `/reset` | Force-reset resettable tasks and clear their scoped artifacts (prompts for confirmation) |
| `/init [--agents-path]` | Initialize ferrus in the current directory |
| `/register [--supervisor <agent>] [--executor <agent>]` | Register Claude Code or Codex configs from HQ |
| `/model <supervisor|executor> <model>` | Update the supervisor or executor model override |
| `/model <supervisor|executor> --clear` | Clear the supervisor or executor model override |
| `/help` | List all HQ commands |
| `/quit` | Exit HQ |

> **Quit HQ:** Press **Ctrl+C** twice within 2 seconds to exit. The first press shows a yellow "Press Ctrl+C again to exit" prompt in the status line; the second confirms and exits. The prompt clears automatically after 2 seconds if you change your mind.

> **TUI features:** Type `/` to see autocomplete suggestions; press **Tab** / **Shift+Tab** to navigate and **Enter** to accept. A status line at the bottom of the terminal shows the current task state and retry/cycle counters in real time.

### How the loop works

```
ferrus> /task
  └─ supervisor spawns → you describe the task → supervisor calls enqueue_task
       └─ executor spawns (headless) → implements → check → submit
            └─ reviewer spawns (headless) → reads submission → approve or reject
                 ├─ approved → Complete
                 └─ rejected → executor re-spawns with feedback
```

Agents are **stateless between runs**. Ferrus resolves each run to a SQLite task row and scoped artifacts under `.ferrus/tasks/` and `.ferrus/runs/`; each spawn exits when its job is done.

---

## State machine

```
pending
 └─► executing      ← /wait_for_task claim
       ├─► addressing ← /reject → work loop
       ├─► consultation ← /consult
       │     └─► (restore paused status) ← /wait_for_consult
       ├─► awaiting_human ← /ask_human
       │     └─► (restore paused status) ← /wait_for_answer
       ├─► reviewing ← /submit final gate pass
       │     ├─► addressing ← /reject
       │     └─► complete ← /approve
       └─► failed ← retry or review-cycle limit
```

Any active Executor work state (Executing, Addressing) can pause to `Consultation` via `/consult`. HQ spawns the configured Supervisor in consultation mode, and the executor immediately calls `/wait_for_consult` to block until the Supervisor answers via `/respond_consult`.

Any active state, including `Consultation`, can pause to `AwaitingHuman` via `/ask_human`. The agent immediately calls `/wait_for_answer` to block until the human responds. The human types their answer in the HQ terminal (raw text, no slash prefix). `/wait_for_answer` restores the previous state and returns the answer.

- Each task advances independently; `max_parallel_tasks` controls concurrent executors.
- HQ `/reset` force-resets resettable tasks and clears their scoped artifacts. MCP `/reset` is only valid for a failed task.

---

## CLI reference

### `ferrus init [--agents-path <path>]`

Scaffolds ferrus in the current project (default `--agents-path .agents`):

- Creates `ferrus.toml` with default limits and an empty check command list
- Creates `.ferrus/` templates, task/run artifact directories, `agents.json`, and `logs/`
- Registers the project in `~/.ferrus/projects/<project-id>/`
- Writes `.ferrus/project.toml` with the project id and local data directory
- Creates `~/.ferrus/projects/<project-id>/project.toml` with project metadata
- Creates `~/.ferrus/projects/<project-id>/ferrus.db` with `tasks`, `runs`, and `events` tables
- Creates `docs/specs/` for approved feature specifications
- Creates skill files agents load to understand their role:
  - `<agents-path>/skills/ferrus/SKILL.md` — general overview
  - `<agents-path>/skills/ferrus-supervisor/SKILL.md` + `ROLE.md`
  - `<agents-path>/skills/ferrus-executor/SKILL.md` + `ROLE.md`
- Adds `.ferrus/` to `.gitignore`

### `ferrus serve [--role supervisor|executor] [--agent-name <name>] [--agent-index <n>]`

Starts the agent coordination server on stdio. Agents load this as an MCP server. `--agent-name` and `--agent-index` are embedded in the `claimed_by` field (e.g. `"executor:codex:1"`). Pass `--role` to expose only the tools for that role:

| `--role` | Tools exposed |
|---|---|
| `supervisor` | Definition sessions: `enqueue_task`, `create_spec`; task sessions: `wait_for_review`, `review_pending`, `approve`, `reject`, `wait_for_consultation`, `respond_consult`, `ask_human`, `wait_for_answer`, `heartbeat` |
| `executor` | `wait_for_task`, `check`, `consult`, `submit`, `wait_for_consult`, `ask_human`, `wait_for_answer`, `status`, `reset`, `heartbeat` |
| *(omitted)* | All tools |

The unfiltered server additionally exposes compatibility tools such as `create_task` and `answer`. The `status` tool includes scoped SQLite task context when called by a running agent with a resolved runtime identity.

### `ferrus register [--supervisor <agent>] [--supervisor-model <model>] [--executor <agent>] [--executor-model <model>]`

Writes agent config files so they automatically load `ferrus serve` as a tool server, and adds only the selected agents' local files to `.gitignore`. At least one of `--supervisor` or `--executor` is required; each model flag requires the matching role flag. Supported agents:

| Agent | Config written |
|---|---|
| `claude-code` | `.claude/mcp-supervisor.json` or `.claude/mcp-executor.json` + `.claude/settings.local.json` permissions |
| `codex` | `.codex/config.toml` |
| `qwen-code` | `.qwen/settings.json` |

### `ferrus doctor`

Checks that `.ferrus/project.toml`, global project metadata, task/run artifacts, and the `ferrus.db` schema agree with the current workspace. It also reports interrupted runs and expired leases that can be fixed with `ferrus recover`.

### `ferrus projects list`

Lists projects registered under `~/.ferrus/projects`, including project id, name, database presence, last opened timestamp, workspace path, and data directory.

### `ferrus recover`

Runs the same runtime recovery that HQ performs on startup: dead running rows are marked `interrupted`, expired task leases without a live run are released, and recorded human answers are reconciled.

Use `ferrus recover --dry-run` to print the pending recovery counters without changing runtime state.
Use `ferrus recover --worktrees` to also remove orphaned managed task worktrees that no active task or active run still owns. Combine it with `--dry-run` to preview the orphan count without removing anything.

### `ferrus tasks list`

Prints task runtime rows from `ferrus.db`, including task status, active claim owner, lease expiry, and artifact path.

### `ferrus runs list [--limit N]`

Prints recent run attempts from `ferrus.db`, including role, agent, status, PID, timestamps, and workspace path.

### `ferrus events list [--limit N] [--run <id>]`

Prints recent runtime events from `ferrus.db`. Use `--run <id>` or `--run-id <id>` to filter to one run attempt.

### `ferrus migrate` / `ferrus upgrade`

Registers an existing pre-registry project in `~/.ferrus/projects/<project-id>/`, initializes the SQLite database, creates `.ferrus/tasks/` and `.ferrus/runs/`, and imports non-empty legacy task, review, submission, human-question, and consultation artifacts into the scoped layout.

---

## `ferrus.toml`

```toml
[checks]
commands = [
    "cargo clippy -- -D warnings",
    "cargo fmt --check",
    "cargo test",
]

[limits]
max_check_retries = 20   # consecutive check failures before state → Failed
max_review_cycles = 3    # reject→fix cycles before state → Failed
max_feedback_lines = 30  # trailing lines per failing command shown in /check and /submit output
wait_timeout_secs = 60   # max duration of one wait_* tool call before it returns timeout so the agent can poll again
max_parallel_tasks = 1   # maximum number of concurrent executor sessions

[lease]
ttl_secs = 90                  # how long a claimed lease is valid without renewal
heartbeat_interval_secs = 30   # how often agents should call heartbeat

[spec]
directory = "docs/specs"       # where /create_spec writes approved specs

[hq.supervisor]
agent = "claude-code"  # agent for supervisor/reviewer role: claude-code | codex | qwen-code
model = ""             # optional override; empty = agent default

[hq.executor]
agent = "codex"        # agent for executor role: claude-code | codex | qwen-code
model = ""             # optional override; empty = agent default
```

Check commands run in the active task workspace. Full output is written to `.ferrus/logs/check_<attempt>_<scope>_<ts>.txt`, where the task/run scope prevents parallel checks from overwriting each other. `/check` and `/submit` return a short failure summary inline.

---

## Runtime files

Ferrus now separates human-readable project artifacts from machine-local runtime state:

| Path | Contents |
|---|---|
| `.ferrus/` | Project-local templates, task/run artifacts, agent registry, and logs |
| `~/.ferrus/projects/<project-id>/` | Machine-local project metadata, SQLite runtime database, and global logs |

SQLite is the runtime source of truth. `ferrus.db` stores task status, claims and leases, run ownership, lifecycle events, retry counters, paused interaction metadata, and selected-spec state. Markdown files are scoped task intent and run artifacts, not a mirrored state machine. On HQ startup Ferrus marks dead active runs as `interrupted`, preserves leases backed by live runs, releases other expired leases, and resumes recoverable task flows.

### `.ferrus/`

| File | Contents |
|---|---|
| `project.toml` | Local pointer to `~/.ferrus/projects/<project-id>/` |
| `agents.json` | Runtime registry for agent sessions, statuses, PIDs, and log ownership |
| `TASK.md` | Task drafting template |
| `CONSULT_TEMPLATE.md` | Read-only consultation request template |
| `SPEC_TEMPLATE.md` | Read-only feature specification template |
| `tasks/<task-id>.md` | Numbered task intent artifact |
| `runs/<task-id>/SUBMISSION.md` | Executor submission notes |
| `runs/<task-id>/REVIEW.md` | Supervisor review or rejection notes |
| `runs/<task-id>/QUESTION.md` | Pending human question |
| `runs/<task-id>/ANSWER.md` | Human answer |
| `runs/<task-id>/CONSULT_REQUEST.md` | Executor consultation request |
| `runs/<task-id>/CONSULT_RESPONSE.md` | Supervisor consultation response |
| `runs/<task-id>/PATCH.diff` | Patch produced from an isolated executor workspace |
| `runs/<task-id>/INTEGRATION_ERROR.md` | Recoverable patch or integration-check failure context |
| `logs/` | Scoped check output and PTY session logs per agent |

`.ferrus/` is gitignored by `ferrus init`.

### `~/.ferrus/projects/<project-id>/`

| File | Contents |
|---|---|
| `project.toml` | Project id, name, workspace path, `.ferrus` path, git metadata, timestamps, schema version |
| `ferrus.db` | SQLite source of truth for tasks, runs, events, leases, counters, and project runtime state |
| `logs/` | Reserved for machine-local logs that should not be committed |

---

## Dogfooding

Ferrus is partially developed using its own orchestration workflow.

This repository is used to validate the Supervisor → Executor → Reviewer loop in real development scenarios.

---

## Getting involved

If you're interested in Ferrus:

- Try running it on your project
- Share feedback on the workflow (what breaks, what feels unnatural)
- Open issues with observations or ideas

At this stage, feedback on the model is more valuable than code contributions.

---

## Licence

Licensed under Apache 2.0.
