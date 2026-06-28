---
name: ferrus
description: "Use when working on a project that uses ferrus for AI agent orchestration — full tool reference, state machine, resources, prompts, and config"
---

# Ferrus

ferrus is an MCP server that coordinates AI agents in a **Supervisor–Executor** workflow.

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
- Supervisors call `/wait_for_review` to claim reviewing task rows, then `/approve` or `/reject`.
- Rejected tasks return to `addressing` and can be claimed again by an executor.

## Runtime Model

SQLite task rows are the runtime source of truth. Typical statuses are `pending`, `executing`,
`addressing`, `consultation`, `awaiting_human`, `reviewing`, `complete`, and `failed`.
Consultation and human-answer flows store paused status and requester metadata in SQLite, with
request/response artifacts scoped under `.ferrus/runs/<task-id>/`.

## Per-Task State Machine

The old single global `STATE.json` is gone, but every SQLite task row still follows the same
Supervisor-Executor lifecycle. With `--limit 1`, the flow is effectively the original single-task
workflow, only DB-backed:

```text
pending
 └─► executing      ← /wait_for_task claim
       ├─► addressing ← /reject (Supervisor) → work loop
       ├─► consultation ← /consult (Executor)
       │     └─► (restore paused status) ← /wait_for_consult
       ├─► awaiting_human ← /ask_human
       │     └─► (restore paused status) ← /wait_for_answer
       ├─► reviewing ← /submit final gate pass (Executor)
       │     ├─► addressing → work loop
       │     └─► complete ← /approve (Supervisor)
       └─► failed ← /check, /submit, or /reject hits retry/cycle limit
```

## CLI

```sh
ferrus init [--agents-path <path>]              # scaffold project files and register ~/.ferrus runtime
ferrus serve [--role supervisor|executor]       # start MCP server on stdio
ferrus register --supervisor <a> --executor <a> # write MCP config for agents
ferrus doctor                                   # verify project metadata, artifacts, and runtime DB
ferrus projects list                            # inspect ~/.ferrus project registry
ferrus recover                                  # recover interrupted runs and stale leases
ferrus recover --dry-run                        # preview recovery without mutating runtime state
ferrus recover --worktrees                      # remove orphaned managed task worktrees
ferrus tasks list                               # inspect SQLite task runtime rows
ferrus runs list                                # inspect SQLite run attempts
ferrus events list                              # inspect SQLite runtime events
ferrus migrate                                  # import legacy project state into SQLite
```

Set `RUST_LOG=ferrus=debug` (or `info`/`warn`) for verbose logs to stderr.

## HQ (run `ferrus` with no arguments)

| Command | Description |
|---|---|
| `/plan` | Free-form planning session with the supervisor (no task created) |
| `/task` | Queue one task from the next ready milestone, then run the scheduler |
| `/task --manual` | Queue one free-form task without spec context |
| `/spec` | Draft, approve, and save a feature specification |
| `/milestones` | Select the current spec |
| `/reset-spec` | Clear the selected spec |
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
| `/init` | Initialize ferrus in the current directory |
| `/register` | Register agent configs |
| `/model` | Update the supervisor or executor model override |
| `/quit` | Exit HQ |

## MCP tools

### Supervisor
| Tool | From state | Description |
|---|---|---|
| `create_task` | — | Compatibility alias for queued task creation on unfiltered servers |
| `enqueue_task` | — | Write numbered task artifact and DB `pending` row |
| `create_spec` | any | Write approved Markdown spec to the configured spec directory |
| `wait_for_review` | — | Long-poll until state is Reviewing |
| `review_pending` | Reviewing | Read task + submission context |
| `approve` | Reviewing | Accept; moves to Complete |
| `reject` | Reviewing | Reject with notes; moves to Addressing |
| `wait_for_consultation` | — | Long-poll until an Executor consultation request is ready and attach this Supervisor run to it |
| `respond_consult` | Consultation | Record the consultation response and let the Executor resume via `/wait_for_consult` |

`create_task` remains a compatibility tool only on an unfiltered `ferrus serve` instance; role-scoped
Supervisor sessions use `enqueue_task` so the full tool list fits on one MCP `tools/list` page.

### Executor
| Tool | From state | Description |
|---|---|---|
| `wait_for_task` | — | Long-poll until Executing or Addressing |
| `check` | Executing, Addressing | Run all configured checks; use it freely during development and again immediately before final `/submit` |
| `consult` | Executing, Addressing | Ask the Supervisor for guidance; moves to Consultation |
| `wait_for_consult` | Consultation | Block until the Supervisor responds; restores previous state |
| `submit` | Executing, Addressing | Run the final review gate and, on success, write submission notes; moves to Reviewing |

### Shared
| Tool | From state | Description |
|---|---|---|
| `ask_human` | Executing, Addressing, Consultation, Reviewing | Last-resort human fallback. Write a scoped question; moves to AwaitingHuman. Call `/wait_for_answer` immediately after. |
| `wait_for_answer` | AwaitingHuman | Block until the human answers; restores previous state and returns the answer |
| `status` | any | Executor-scoped status and runtime context |
| `reset` | Failed | Mark the failed task as reset |
| `heartbeat` | any claimed | Executor-scoped lease renewal; returns `{"status":"renewed"}` or `{"status":"error","code":"..."}` |

## MCP resources

| URI | Contents |
|---|---|
| `ferrus://task` | Current task description (compatibility/current context) |
| `ferrus://task/<task-id>` | Numbered task artifact, for example `.ferrus/tasks/t-001.md` |
| `ferrus://task_template` | Task drafting template (`TASK.md`) |
| `ferrus://review` | Scoped Supervisor rejection notes (`REVIEW.md`) |
| `ferrus://submission` | Scoped Executor submission notes (`SUBMISSION.md`) |
| `ferrus://question` | Scoped pending human question (`QUESTION.md`) |
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
max_review_cycles = 3    # reject→fix cycles before Failed
max_feedback_lines = 30  # lines per command shown in /check and /submit output
wait_timeout_secs = 60   # max duration of one wait_* tool call; agents should call again after timeout
max_parallel_tasks = 1   # max concurrent executor sessions

[lease]
ttl_secs = 90            # lease validity without renewal
heartbeat_interval_secs = 30  # how often to call /heartbeat

[spec]
directory = "docs/specs" # where /create_spec writes approved specs
```

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

### `~/.ferrus/projects/<project-id>/`

| File | Contents |
|---|---|
| `project.toml` | Project metadata and canonical workspace paths |
| `ferrus.db` | SQLite source of truth for tasks, runs, events, leases, counters, and project runtime state |
