# Ferrus CLI and Runtime Reference

This document contains the operational detail intentionally omitted from the project README. The README is the
short entry point; this page is the reference for day-to-day local use.

## HQ commands

Running `ferrus` without arguments opens the interactive HQ shell.

| Command | Description |
|---|---|
| `/plan` | Start a free-form planning session without creating a task. |
| `/task` | Define a task from the selected milestone and start the Executor/Reviewer loop. |
| `/task --manual` | Define a free-form task without selected milestone context. |
| `/spec` | Draft, approve, and save a feature specification. |
| `/archive-spec` | Add an approved Outcome and archive completed spec artifacts. |
| `/milestones` | Select the current specification; optionally start its next ready milestone. |
| `/reset-spec` | Clear the selected specification. |
| `/check` | Run configured checks without changing task state. |
| `/check --force` | Compatibility form of `/check`; both ignore task status. |
| `/supervisor` | Open an interactive Supervisor session. |
| `/executor` | Open an interactive Executor session. |
| `/resume` | Resume the Executor or a paused consultation. |
| `/review` | Manually start review if automatic spawning failed. |
| `/status` | Show task state, agents, counters, and session logs. |
| `/tasks` | List task rows. |
| `/run [--limit N]` | Plan and queue a batch from ready milestones in the selected spec. |
| `/runs [--limit N]` | List recent run attempts. |
| `/events [--limit N] [--run ID]` | List runtime events. |
| `/attach NAME` | Show the log path for a running headless agent. |
| `/stop` | Stop running agent sessions after confirmation. |
| `/reset` | Stop sessions, mark all resettable tasks Reset, and clear their scoped artifacts after confirmation. |
| `/init [--agents-path PATH]` | Initialize Ferrus in the current directory. |
| `/register ...` | Register agent configuration from HQ. |
| `/model ROLE MODEL` | Set a role-specific model override. |
| `/model ROLE --clear` | Clear the role-specific model override. |
| `/help` | List HQ commands. |
| `/quit` | Exit HQ. |

Press Ctrl+C twice within two seconds to exit HQ. The first press displays a confirmation prompt. The TUI supports
slash-command completion with Tab and Shift+Tab and shows live task and retry state in its status line.

## Workflow

```text
ferrus> /task
  +-> Supervisor defines and enqueues the task
       +-> Executor implements, checks, and submits
            +-> Reviewer approves or rejects
                 +-> approve: task becomes Complete
                 +-> reject: Executor resumes with feedback
```

Agents are stateless between runs. SQLite owns runtime state; scoped Markdown files under `.ferrus/tasks/` and
`.ferrus/runs/` carry human-readable intent and artifacts.

## CLI commands

### `ferrus init [--agents-path PATH]`

Initializes a project, creates the local templates and artifact directories, registers machine-local project
metadata, creates `ferrus.db`, and installs Ferrus role skills under the selected agents path. Existing skill files are preserved.

### `ferrus serve [--role supervisor|executor] [--agent-name NAME] [--agent-index N]`

Starts the stdio MCP server used by agent adapters. A role limits lifecycle tools to those required by that role.
Taskless Supervisors expose task/spec creation, while task-scoped sessions hide those tools and expose heartbeat.
HQ archive mode exposes `archive_spec` and hides review/consultation tools. Only unfiltered servers expose
compatibility `create_task` and `answer`. Retrieval tools are registered in every role, even when an index is
unavailable; they report its status without building it.

### `ferrus register ...`

Writes role-scoped configuration for Claude Code, Codex, Qwen Code, opencode, or goose. Model flags require the
matching role flag. Goose receives its MCP extension at launch and therefore needs no project config file.

### Runtime inspection and recovery

```sh
ferrus doctor
ferrus recover [--dry-run] [--worktrees]
ferrus projects list
ferrus tasks list
ferrus runs list [--limit N]
ferrus events list [--limit N] [--run ID]
ferrus migrate
ferrus upgrade
```

`doctor` validates project metadata, runtime schema, artifacts, interrupted runs, and leases. `recover` applies the
same safe runtime recovery used by HQ; `--dry-run` previews it and `--worktrees` also considers orphaned managed
worktrees. Migration registers older projects and imports supported legacy artifacts into the scoped layout.

## Agent adapters

Agent support is normalized through `src/agents/`: shared Supervisor and Executor contracts are independent from
the concrete CLI, while the Claude Code, Codex, Qwen Code, opencode, and goose adapters own launch flags, model
overrides, headless prompt transport, and local permission or configuration conventions.

Qwen Code and goose are experimental. Ferrus attaches its role-scoped MCP server to goose at launch with
`--with-extension`, so no project config file is written. Configure its model provider separately with
`goose configure`. Goose honors per-task worktrees, and Ferrus bounds headless runs with turn and repeated-tool
limits. `GOOSE_MAX_TURNS` can raise the default turn budget; tool-calling reliability still depends on the model.

The opencode Executor adapter is not currently supported. Opencode identifies a project by its Git root commit and
binds it to one working directory in its global store, which can escape the isolated per-task worktree. Use it only
for Supervisor and Reviewer roles until that behavior changes.

## Repository graph and project memory

Enable local structural indexing explicitly:

```toml
[repository_graph]
enabled = true
```

Common commands:

```sh
ferrus graph index [--full] [--json]
ferrus graph status [--json]
ferrus graph search RuntimeTaskContext --kind struct --path src [--limit 20] [--json]
ferrus graph show --node NODE_ID [--json]
ferrus graph show --symbol SEMANTIC_KEY [--json]
ferrus graph show --path src/project.rs [--json]
ferrus graph context --symbol SEMANTIC_KEY [--depth 2] [--max-results 50] [--json]
ferrus graph neighbors NODE_ID --direction both --depth 2 --limit 50 [--json]
ferrus graph memory index [--full] [--json]
ferrus graph memory status [--json]
ferrus graph search decision --domain memory [--kind decision] [--json]
ferrus graph context --domain all --milestone rg3.6 [--depth 2] [--json]
```

Repository snapshots are immutable and atomically published. Incremental indexing reuses unchanged fragments;
`--full` bypasses that cache. Structural facts live in machine-local `repo-graph.db` without source bodies.
Requested snippets are loaded separately and verified against the selected snapshot.

Project memory lives in independent `project-memory.db` revisions. Default ingestion accepts tracked specification
structure, approved Outcome sections, sanitized archive metadata, and bounded runtime provenance. It excludes raw
task, review, submission, patch, log, question, answer, consultation, and integration-error bodies.

Managed Executor worktrees use task-owned graph views composed from a pinned baseline and changed-file overlay.
Submission freezes the view and its Git tree for review. Canonical, mutable task, and frozen review routing are
explicit; an unavailable task view never falls back to canonical data.

Further reading:

- [Repository graph architecture](repository-graph-architecture.md)
- [Repository retrieval contract](repository-graph-retrieval.md)
- [Repository graph benchmarks](repository-graph-benchmarks.md)
- [Repository graph evaluation](repository-graph-evaluations.md)
- [Project memory architecture](project-memory-architecture.md)
- [Project memory operations](project-memory.md)
- [Project memory evaluation](project-memory-evaluations.md)
- [Distributed indexing architecture](distributed-indexing-architecture.md)

## Configuration

A compact starting configuration is:

```toml
[checks]
commands = [
    "cargo clippy -- -D warnings",
    "cargo fmt --check",
    "cargo test",
]

[limits]
max_check_retries = 20
max_review_cycles = 3
max_feedback_lines = 30
wait_timeout_secs = 60
max_parallel_tasks = 1
max_executor_dispatches = 6

[lease]
ttl_secs = 90
heartbeat_interval_secs = 30

[spec]
directory = "docs/specs"

[hq.supervisor]
agent = "claude-code"
model = ""

[hq.executor]
agent = "codex"
model = ""
```

Executor MCP checks run in the task workspace and write full logs under `.ferrus/logs/`, returning bounded
failure summaries. Output is spooled to disk; each failed command's feedback retains at most
`max_feedback_lines` trailing lines and 64 KiB, including for a single long line. HQ `/check` runs in
HQ's working directory without task-state or retry changes.
A dispatch limit of zero disables that guard; a fresh rejection resets its counter. `max_parallel_tasks`
controls Executor concurrency, while `/run --limit N` caps the planned batch. Non-Git projects use the
canonical directory and permit only one Executor.

## Runtime files

Ferrus deliberately separates repository-local artifacts from machine-local runtime state.

| Path | Contents |
|---|---|
| `.ferrus/project.toml` | Pointer to machine-local project state. |
| `.ferrus/agents.json` | Agent session registry. |
| `.ferrus/tasks/<task-id>.md` | Task intent. |
| `.ferrus/runs/<task-id>/` | Submission, review, interaction, patch, and integration artifacts. |
| `.ferrus/logs/` | Scoped check and PTY logs. |
| `~/.ferrus/projects/<project-id>/project.toml` | Machine-local project metadata. |
| `~/.ferrus/projects/<project-id>/ferrus.db` | Runtime source of truth. |
| `~/.ferrus/projects/<project-id>/repo-graph.db` | Optional derived repository graph. |
| `~/.ferrus/projects/<project-id>/project-memory.db` | Optional derived project memory. |
| `~/.ferrus/projects/<project-id>/worktrees/<task-id>/` | Managed Executor Git worktrees. |
| `.ferrus/hq.log` | HQ tracing output. |
| `~/.ferrus/projects/<project-id>/archive/` | Archived completed specification artifacts. |

At startup, Ferrus reconciles dead runs and expired leases using the database. Markdown files are not a mirrored
state machine.
