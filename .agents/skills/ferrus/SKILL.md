---
name: ferrus
description: "Use when working on a project that uses ferrus for AI agent orchestration -- full tool reference, state machine, resources, prompts, and config"
---

# Ferrus

ferrus is an MCP server that coordinates AI agents in a **Supervisor-Executor** workflow.

This file is supporting context only.
Runtime behavior is defined by the active initial prompt and Ferrus MCP tools.
If this file conflicts with them, follow the prompt and tools.

## Roles

| Role | Responsibility |
|---|---|
| Supervisor | Writes tasks, reviews Executor submissions, approves or rejects |
| Executor | Implements tasks, runs checks during development, and submits when ready |

Agents run one-shot sessions under HQ and coordinate through SQLite runtime rows plus scoped artifacts
under `.ferrus/tasks/` and `.ferrus/runs/`.

Under HQ:
- Executors call `/wait_for_task` to claim a ready SQLite task row, implement, use `/check` during development, then call `/submit` for the final review gate.
- Reviewers call `/wait_for_review` to claim reviewing task rows, then `/approve` or `/reject`.
- Consultants call `/wait_for_consultation` to attach to an Executor request, then `/respond_consult`.
- Rejected tasks return to `addressing` and can be claimed again by an executor.

## Runtime Model

SQLite task rows are the runtime source of truth. Typical statuses are `pending`, `executing`,
`addressing`, `consultation`, `awaiting_human`, `reviewing`, `complete`, `failed`, and `reset`.
Consultation and human-answer flows store paused status and requester metadata in SQLite, with
request/response artifacts scoped under `.ferrus/runs/<task-id>/`.

## Specification Memory

Specs may include a `## Outcome` section after their implementation work is complete. Treat this
section as compact project memory: what was actually delivered, notable deviations from the original
spec, validation evidence, follow-up work, and context that can help future agents avoid rereading
raw task/run artifacts.

When drafting, reviewing, or planning from an existing spec, read `## Outcome` if present and use it
as historical context. Do not invent or update an outcome section unless the active prompt or Ferrus
tool workflow asks for spec closure or archival work.

## Per-Task State Machine

Each SQLite task has its own lifecycle. Set `limits.max_parallel_tasks = 1` for serial
Executor work; `/run --limit N` limits the number of milestones queued, not concurrency.

```text
pending -> executing                         /wait_for_task
executing or addressing -> reviewing         /submit (checks pass)
reviewing -> addressing                      /reject
reviewing -> complete                        /approve
executing or addressing -> consultation      /consult
consultation -> previous work state          /wait_for_consult
active work or review -> awaiting_human      /ask_human
awaiting_human -> previous state             /wait_for_answer
working or reviewing -> failed               retry, review-cycle, or dispatch limit
failed -> reset                              MCP /reset
```

`/reject` returns work to `addressing` only while the review-cycle budget permits it.
HQ `/reset` also resets pending and active tasks after confirmation; `reset` is terminal,
not a requeue. MCP `/reset` preserves task artifacts, while HQ `/reset` clears scoped artifacts.

## CLI

```sh
ferrus init [--agents-path <path>]  # scaffold project files and register machine-local runtime state
ferrus serve [--role supervisor|executor] [--agent-name <name>] [--agent-index <n>]  # start the role-scoped MCP server on stdio
ferrus register --supervisor <agent> --executor <agent>  # write agent MCP configuration and permissions
ferrus doctor  # check project metadata, runtime state, artifacts, and graph health
ferrus projects list  # list projects registered on this machine
ferrus recover [--dry-run] [--worktrees]  # recover interrupted runs and leases; optionally preview or clean orphaned worktrees
ferrus tasks list  # list SQLite task runtime rows
ferrus runs list [--limit N]  # list recent agent run attempts
ferrus events list [--limit N] [--run ID]  # list runtime events, optionally filtered by run
ferrus migrate  # import legacy project state into SQLite; alias: upgrade
ferrus graph index [--full] [--json]  # build or incrementally refresh the canonical repository graph
ferrus graph status [--json]  # inspect graph availability, freshness, and diagnostics
ferrus graph memory index [--full] [--json]  # build or refresh curated project memory
ferrus graph memory status [--json]  # inspect memory availability, revision, and freshness
ferrus graph search <query> [--domain repository|memory|all] [--kind <kind>] [--path <prefix>] [--limit N] [--json]  # search repository structure, project memory, or both
ferrus graph show (--node <id> | --symbol <key> | --path <path>) [--json]  # look up graph nodes by ID, symbol, or evidence path
ferrus graph neighbors <node-id> [--direction incoming|outgoing|both] [--depth N] [--limit N] [--json]  # follow bounded incoming or outgoing graph relationships
ferrus graph context (--node <id> | --symbol <key> | --path <path> | --memory-entity <id> | --milestone <id> | --task <id> | --run <id>) [--domain repository|memory|all] [--depth N] [--max-results N] [--max-bytes N] [--json]  # assemble bounded context from explicit repository or memory seeds
```

Registration accepts either role or both; `--supervisor-model` and `--executor-model` require
the matching role flag. CLI graph search/context default to `repository`; memory seeds need
`--domain memory` or `--domain all`.

Set `RUST_LOG=ferrus=debug` (or `info`/`warn`) for stderr logging in CLI/MCP mode.
HQ writes tracing logs to `.ferrus/hq.log` instead of the terminal.

## HQ (run `ferrus` with no arguments)

| Command | Description |
|---|---|
| `/plan` | Free-form planning session with the supervisor (no task created) |
| `/task` | Queue one task from the next ready milestone, then run the scheduler |
| `/task --manual` | Queue one free-form task without spec context |
| `/spec` | Draft, approve, and save a feature specification; offers to archive a completed selected spec first |
| `/archive-spec` | Summarize completed selected spec work into `## Outcome` and archive linked task/run artifacts |
| `/milestones` | Select the current spec |
| `/reset-spec` | Clear the selected spec |
| `/check [--force]` | Run checks in the HQ working directory without changing task state; `--force` is a compatibility no-op |
| `/supervisor` | Open an interactive supervisor session (no initial prompt) |
| `/executor` | Open an interactive executor session (no initial prompt) |
| `/review` | Manually spawn supervisor in review mode (escape hatch) |
| `/resume` | Resume the executor headlessly; also recovers Consultation by relaunching both consultant and executor |
| `/status` | Show task state, agent list, and session log paths |
| `/tasks` | List SQLite task runtime rows |
| `/run [--limit N]` | Plan a batch run from ready milestones in the selected spec |
| `/runs [--limit N]` | List SQLite run attempts |
| `/events [--limit N] [--run <id>]` | List SQLite runtime events |
| `/attach <name>` | Show log path for a running headless agent |
| `/stop` | Stop all running agent sessions |
| `/reset` | Force-reset resettable tasks and clear their scoped artifacts |
| `/init [--agents-path <path>]` | Initialize ferrus in the current directory |
| `/register` | Register agent configs |
| `/model ROLE <model>` | Set the role model override |
| `/model ROLE --clear` | Clear the role model override |
| `/help` | List HQ commands |
| `/quit` | Exit HQ |

## MCP tools

Tool visibility depends on the session, not just the role:

- Taskless Supervisors get `enqueue_task` and `create_spec`; task-scoped Reviewers/Consultants do not.
- `archive_spec` is exposed only in HQ archive mode or on an unfiltered server. Archive mode hides review/consultation tools.
- `heartbeat` is exposed to Executors and task-scoped Supervisors; it renews only a lease actually owned by the caller.
- `status` and `reset` are Executor tools. `ask_human` and `wait_for_answer` are shared.
- An unfiltered server exposes all tools, including compatibility `create_task` and the human-answer tool `answer`.

### Supervisor
| Tool | From state | Description |
|---|---|---|
| `create_task` | -- | Compatibility alias for queued task creation on unfiltered servers |
| `enqueue_task` | -- | Write numbered task artifact and DB `pending` row |
| `create_spec` | any | Write approved Markdown spec to the configured spec directory |
| `archive_spec` | any | Write approved `## Outcome` project memory and archive completed spec task/run artifacts |
| `wait_for_review` | -- | Long-poll until state is Reviewing |
| `review_pending` | Reviewing | Read task + submission context |
| `approve` | Reviewing | Accept; moves to Complete |
| `reject` | Reviewing | Reject with notes; moves to Addressing |
| `wait_for_consultation` | -- | Long-poll until an Executor consultation request is ready and attach this Supervisor run to it |
| `respond_consult` | Consultation | Record the consultation response and let the Executor resume via `/wait_for_consult` |

`create_task` remains a compatibility tool only on an unfiltered `ferrus serve` instance; role-scoped
task-definition sessions use `enqueue_task`; HQ archive sessions use `archive_spec` after Outcome approval.

### Executor
| Tool | From state | Description |
|---|---|---|
| `wait_for_task` | Pending, Executing, Addressing | Claim ready work; promote Pending to Executing |
| `check` | Executing, Addressing | Run all configured checks; use it freely during development and again immediately before final `/submit` |
| `consult` | Executing, Addressing | Ask the Supervisor for guidance; moves to Consultation |
| `wait_for_consult` | Consultation | Block until the Supervisor responds; restores previous state |
| `submit` | Executing, Addressing | Run the final review gate and, on success, write submission notes; moves to Reviewing |

### Shared and compatibility tools
| Tool | From state | Description |
|---|---|---|
| `ask_human` | Executing, Addressing, Consultation, Reviewing | Last-resort human fallback. Write a scoped question; moves to AwaitingHuman. Call `/wait_for_answer` immediately after. |
| `wait_for_answer` | AwaitingHuman | Block until the human answers; restores previous state and returns the answer |
| `status` | any | Executor-scoped status and runtime context |
| `reset` | Failed | Executor/unfiltered: mark the task as reset without deleting artifacts |
| `heartbeat` | any claimed | Renew the caller-owned lease; returns `{"status":"renewed"}` or `{"status":"error","code":"..."}` |
| `answer` | AwaitingHuman | Unfiltered server only: record the human answer; the waiting agent restores state |

### Repository retrieval

Both roles can use the optional read-only `repository_graph_status`, `repository_search`, and
`repository_context` tools without a task lease. Check status first when availability is unknown, use search for
exact path/symbol discovery, then request a small bounded context packet. Ask for snippets only when source text is
needed; snippets are hash-verified against the returned snapshot. Graph use is optional, and a missing relationship
means only "not known by this index," not proof that the relationship does not exist. These tools never build or
mutate the index and graph output is not injected into task or review prompts.

### Project memory and federated retrieval

Both roles can also use `project_memory_status`, `project_context_search`, and `project_context`
without a task lease. Search/context requests require an explicit `repository`, `memory`, or
`all` domain; `project_memory_status` takes no domain argument. Check memory status
before memory retrieval. Combined context preserves the exact repository snapshot, memory revision,
independent freshness, and evidence-backed cross-domain links. The tools do not build indexes,
author outcomes, change policy, or mutate task/run/archive state.

## MCP resources

| URI | Contents |
|---|---|
| `ferrus://task` | Current task description (compatibility/current context) |
| `ferrus://task/<task-id>` | Numbered task artifact, for example `.ferrus/tasks/t-001.md` |
| `ferrus://task_template` | Task drafting template (`TASK.md`) |
| `ferrus://review` | Scoped Supervisor rejection notes (`REVIEW.md`) |
| `ferrus://submission` | Scoped Executor submission notes (`SUBMISSION.md`) |
| `ferrus://question` | Scoped pending human question (`QUESTION.md`) |
| `ferrus://answer` | Scoped human answer (`ANSWER.md`) |
| `ferrus://consult_template` | Consultation request template (`CONSULT_TEMPLATE.md`) |
| `ferrus://spec_template` | Feature specification template (`SPEC_TEMPLATE.md`) |
| `ferrus://consult_request` | Scoped pending supervisor consultation request (`CONSULT_REQUEST.md`) |
| `ferrus://consult_response` | Scoped Supervisor consultation response (`CONSULT_RESPONSE.md`) |
| `ferrus://state` | SQLite runtime state summary as JSON |
| `ferrus://runtime_context` | Agent id, inherited Ferrus env vars, and resolved SQLite task context as JSON |

## MCP prompts

| Prompt | Description |
|---|---|
| `executor-context` | Scoped state + task + review notes bundled for the Executor |
| `supervisor-review` | Scoped state + task + submission notes bundled for the Supervisor |

## ferrus.toml

```toml
[checks]
commands = ["cargo clippy -- -D warnings", "cargo fmt --check", "cargo test"]

[limits]
max_check_retries = 20   # check failures before Failed
max_review_cycles = 3    # reject->fix cycles before Failed
max_feedback_lines = 30  # lines per command shown in /check and /submit output
wait_timeout_secs = 60   # max duration of one wait_* tool call; agents should call again after timeout
max_parallel_tasks = 1   # max concurrent executor sessions
max_executor_dispatches = 6 # max launches per work phase; 0 disables the guard

[lease]
ttl_secs = 90            # lease validity without renewal
heartbeat_interval_secs = 30  # how often to call /heartbeat

[spec]
directory = "docs/specs" # where /create_spec writes approved specs

[hq.supervisor]
agent = "claude-code"
model = ""              # empty = agent default

[hq.executor]
agent = "codex"
model = ""
```

A fresh rejection resets the Executor dispatch counter. Setup failures do not consume it.
Git projects use task worktrees; non-Git projects run in the canonical directory with one Executor.
`ferrus init` creates missing skills but does not overwrite existing ones.

## Runtime files

Ferrus separates project-local artifacts from machine-local runtime state. `.ferrus/` stores
human-readable project files and task/run artifacts. `~/.ferrus/projects/<project-id>/` stores
project metadata and `ferrus.db`, the runtime source of truth.

### `.ferrus/`

| File | Contents |
|---|---|
| `project.toml` | Local pointer to `~/.ferrus/projects/<project-id>/` |
| `agents.json` | Runtime registry for agent sessions, statuses, PIDs, and logs |
| `TASK.md` | Task drafting template |
| `CONSULT_TEMPLATE.md` | Read-only consultation request template |
| `SPEC_TEMPLATE.md` | Read-only feature specification template |
| `tasks/<task-id>.md` | Numbered task intent artifact |
| `runs/<task-id>/SUBMISSION.md` | Scoped Executor submission notes |
| `runs/<task-id>/REVIEW.md` | Scoped Supervisor review or rejection notes |
| `runs/<task-id>/QUESTION.md` | Scoped pending human question |
| `runs/<task-id>/ANSWER.md` | Scoped human answer |
| `runs/<task-id>/CONSULT_REQUEST.md` | Scoped Executor consultation request |
| `runs/<task-id>/CONSULT_RESPONSE.md` | Scoped Supervisor consultation response |
| `runs/<task-id>/PATCH.diff` | Scoped implementation patch |
| `runs/<task-id>/INTEGRATION_ERROR.md` | Scoped integration/check failure context |
| `logs/check_<attempt>_<scope>_<ts>.txt` | Full check output, uniquely scoped for parallel runs |
| `logs/` | PTY session logs per agent |
| `hq.log` | HQ tracing output |

### `~/.ferrus/projects/<project-id>/`

| File | Contents |
|---|---|
| `project.toml` | Project metadata and canonical workspace paths |
| `ferrus.db` | SQLite source of truth for tasks, runs, events, leases, counters, and project runtime state |
| `repo-graph.db` | Optional derived repository graph |
| `project-memory.db` | Optional derived project memory |
| `worktrees/<task-id>/` | Managed Executor Git worktrees |
| `archive/specs/<spec-slug>-<closed-at>/` | Machine-local archives for completed spec task/run artifacts |
