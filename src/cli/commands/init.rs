//! Scaffold project configuration, artifact templates, skills, and agent registration.

use anyhow::{Context, Result};
use std::path::Path;

use crate::templates::{SPEC_TEMPLATE, TASK_TEMPLATE};

const DEFAULT_FERRUS_TOML: &str = r#"[checks]
commands = []

[limits]
max_check_retries = 20  # consecutive check failures before state -> Failed
max_review_cycles = 3   # reject->fix cycles before state -> Failed
max_feedback_lines = 30 # trailing lines per failing command shown in /check and /submit output (full output always in .ferrus/logs/)
wait_timeout_secs = 60 # max duration of a single wait_* tool call before it returns timeout so the agent can poll again
max_parallel_tasks = 1 # max concurrent executor sessions; set to 2+ to run independent tasks in parallel
max_executor_dispatches = 6 # executor (re)spawns per work phase before state -> Failed; bounds a session that keeps exiting without submitting

[agents]
path = ".agents" # root directory for agent skill files

[spec]
directory = "docs/specs" # directory where /create_spec writes approved specs

[lease]
ttl_secs = 90              # how long a claimed lease is valid without renewal
heartbeat_interval_secs = 30 # how often agents should call /heartbeat

[hq.supervisor]
agent = "claude-code"  # agent to use for supervisor/reviewer role: claude-code | codex | qwen-code | goose | opencode
model = ""             # optional override; empty = agent default

[hq.executor]
agent = "codex"        # agent to use for executor role: claude-code | codex | qwen-code | goose | opencode
model = ""             # optional override; empty = agent default
"#;

const SUPERVISOR_SKILL: &str = r#"---
name: ferrus-supervisor
description: "Advisory Supervisor playbook for task drafting, review, and consultation quality"
---

# Supervisor Operating Playbook

This file is advisory only.
Runtime workflow is defined by the initial prompt and Ferrus MCP tools.

## Task drafting

- Define the expected outcome clearly
- State relevant constraints and acceptance criteria
- Keep task scope explicit and bounded
- Draft task text that the user can review directly

## Review quality

- Judge correctness against the task, not personal preference
- Focus on regressions, missing requirements, and verification gaps
- Write rejection feedback that is concrete and actionable

## Consultation quality

- Answer the Executor's actual blocker
- Prefer concrete direction over abstract discussion
- Clarify tradeoffs when there is no single obvious answer

## Human interaction

- Confirm task wording with the user before task creation
- Use `/ask_human` only when the answer cannot be reliably derived from the repository or current context

## Useful Ferrus tools

- `/create_task`
- `/create_spec`
- `/wait_for_review`
- `/review_pending`
- `/approve`
- `/reject`
- `/respond_consult`
- `/ask_human`

## Useful Ferrus resources

- `ferrus://task`
- `ferrus://spec_template`
- `ferrus://submission`
- `ferrus://review`
- `ferrus://consult_request`
"#;

const SUPERVISOR_ROLE: &str = r#"---
name: ferrus-supervisor-role
description: "High-level Supervisor role description and boundaries"
---

# Supervisor Role

High-level description of the Supervisor role.

## Responsibilities

- Define clear, executable tasks
- Review submitted work
- Provide consultation when the Executor is blocked

## Boundaries

- Does not implement Executor work in task-definition or review mode
- Does not bypass Ferrus tools or state transitions
- Does not manipulate `.ferrus/` files to force progress

## Notes

This file is descriptive only.
Runtime behavior is defined by the initial prompt and Ferrus MCP tools.
If this file conflicts with them, follow the prompt and tools.
"#;

const EXECUTOR_SKILL: &str = r#"---
name: ferrus-executor
description: "Advisory Executor playbook for implementation, code navigation, and submission quality"
---

# Executor Operating Playbook

This file is advisory only.
Runtime workflow is defined by the initial prompt and Ferrus MCP tools.

## Implementation guidelines

- Prefer minimal, targeted diffs
- Avoid unrelated refactoring
- Preserve existing project patterns unless the task requires otherwise

## Code navigation

- Start from entrypoints and public interfaces
- Trace dependencies before changing behavior
- Inspect surrounding code before modifying shared logic

## Common pitfalls

- Hidden side effects
- Implicit contracts between modules
- Test coupling and fixture assumptions
- State transitions that depend on tool behavior

## Ferrus guidance

- Use Ferrus tools rather than reconstructing state from `.ferrus/`
- Use `/check` freely during development; prefer TDD where it fits the task
- Run `/check` again immediately before the final `/submit`
- Read Ferrus resources when they help clarify task context
- Use the consultation template when escalating technical uncertainty

## Submission quality

- Provide a clear summary of what changed
- Include concrete manual verification steps
- Mention limitations or follow-up work explicitly when relevant

## Useful Ferrus tools

- `/wait_for_task`
- `/check`
- `/consult`
- `/wait_for_consult`
- `/ask_human`
- `/wait_for_answer`
- `/submit`

## Useful Ferrus resources

- `ferrus://task`
- `ferrus://review`
- `ferrus://consult_template`
- `ferrus://question`
- `ferrus://answer`
"#;

const CONSULT_TEMPLATE: &str = r#"## Problem
...

## What I tried
...

## Options (if any)
...

## Question
...
"#;

const EXECUTOR_ROLE: &str = r#"---
name: ferrus-executor-role
description: "High-level Executor role description and boundaries"
---

# Executor Role

High-level description of the Executor role.

## Responsibilities

- Implement assigned tasks
- Verify work via `/check`
- Submit completed results via `/submit`

## Boundaries

- Does not approve own work
- Does not redefine the task
- Does not bypass Ferrus tools or state transitions
- Does not emulate Ferrus tool effects by editing `.ferrus/` directly

## Notes

This file is descriptive only.
Runtime behavior is defined by the initial prompt and Ferrus MCP tools.
If this file conflicts with them, follow the prompt and tools.
"#;

const FERRUS_SKILL: &str = r#"---
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
 +-> executing      <- /wait_for_task claim
      +-> addressing <- /reject (Supervisor) -> work loop
      +-> consultation <- /consult (Executor)
      |    +-> (restore paused status) <- /wait_for_consult
      +-> awaiting_human <- /ask_human
      |    +-> (restore paused status) <- /wait_for_answer
      +-> reviewing <- /submit final gate pass (Executor)
      |    +-> addressing -> work loop
      |    +-> complete <- /approve (Supervisor)
      +-> failed <- /check, /submit, or /reject hits retry/cycle limit
```

## CLI

```sh
ferrus init [--agents-path <path>]              # scaffold project files and register ~/.ferrus runtime
ferrus serve [--role supervisor|executor]       # start MCP server on stdio
ferrus register --supervisor <a> --supervisor-model <m> --executor <a> --executor-model <m> # write MCP config for agents
ferrus doctor                                   # verify project metadata, artifacts, and runtime DB
ferrus projects list                            # inspect ~/.ferrus project registry
ferrus recover                                  # recover interrupted runs and stale leases
ferrus recover --dry-run                        # preview recovery without mutating runtime state
ferrus recover --worktrees                      # remove orphaned managed task worktrees
ferrus tasks list                               # inspect SQLite task runtime rows
ferrus runs list                                # inspect SQLite run attempts
ferrus events list                              # inspect SQLite runtime events
ferrus migrate                                  # upgrade an existing project registration
```

Set `RUST_LOG=ferrus=debug` (or `info`/`warn`) for verbose logs to stderr.

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
| `create_task` | -- | Compatibility alias for queued task creation; prefer `enqueue_task` |
| `enqueue_task` | -- | Write approved numbered task artifact and DB `pending` row |
| `create_spec` | any | Write approved Markdown spec to the configured spec directory |
| `archive_spec` | any | Write approved `## Outcome` project memory and archive completed spec task/run artifacts |
| `wait_for_review` | -- | Long-poll until state is Reviewing |
| `review_pending` | Reviewing | Read task + submission context |
| `approve` | Reviewing | Accept; moves to Complete |
| `reject` | Reviewing | Reject with notes; moves to Addressing |
| `wait_for_consultation` | -- | Long-poll until an Executor consultation request is ready and attach this Supervisor run to it |
| `respond_consult` | Consultation | Record the consultation response and let the Executor resume via `/wait_for_consult` |

`create_task` remains a compatibility tool only on an unfiltered `ferrus serve` instance; role-scoped
Supervisor sessions use `enqueue_task` for task creation and `archive_spec` for approved spec closure.

### Executor
| Tool | From state | Description |
|---|---|---|
| `wait_for_task` | -- | Long-poll until Executing or Addressing |
| `check` | Executing, Addressing | Run all configured checks; use it freely during development and again immediately before final `/submit` |
| `consult` | Executing, Addressing | Ask the Supervisor for guidance; moves to Consultation |
| `wait_for_consult` | Consultation | Block until the Supervisor responds; restores previous state |
| `submit` | Executing, Addressing | Run the final review gate and, on success, write submission notes and move to Reviewing |

### Shared
| Tool | From state | Description |
|---|---|---|
| `ask_human` | Executing, Addressing, Consultation, Reviewing | Last-resort human fallback. Write a scoped question; moves to AwaitingHuman. Call `/wait_for_answer` immediately after. |
| `wait_for_answer` | AwaitingHuman | Block until the human answers; restores previous state and returns the answer |
| `status` | any | Print current state, counters, and scoped SQLite task context when called by an active agent |
| `reset` | Failed | Mark the failed task as reset |
| `heartbeat` | any claimed | Renew lease; returns `{"status":"renewed"}` or `{"status":"error","code":"..."}` |

### Repository retrieval

Both roles can use the optional read-only `repository_graph_status`, `repository_search`, and
`repository_context` tools without a task lease. Check status first when availability is unknown, use search for
exact path/symbol discovery, then request a small bounded context packet. Ask for snippets only when source text is
needed; snippets are hash-verified against the returned snapshot. Graph use is optional, and a missing relationship
means only "not known by this index," not proof that the relationship does not exist. These tools never build or
mutate the index and graph output is not injected into task or review prompts.

## MCP resources

| URI | Contents |
|---|---|
| `ferrus://task` | Task description resolved from the caller's runtime context |
| `ferrus://task/<task-id>` | Numbered task artifact, for example `.ferrus/tasks/t-001.md` |
| `ferrus://task_template` | Task drafting template (`TASK.md`) |
| `ferrus://review` | Scoped Supervisor rejection notes (`REVIEW.md`) |
| `ferrus://submission` | Scoped Executor submission notes (`SUBMISSION.md`) |
| `ferrus://question` | Scoped pending human question (`QUESTION.md`) |
| `ferrus://answer` | Human answer (`ANSWER.md`) |
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
max_executor_dispatches = 6 # executor (re)spawns per work phase before Failed

[lease]
ttl_secs = 90            # lease validity without renewal
heartbeat_interval_secs = 30  # how often to call /heartbeat

[spec]
directory = "docs/specs" # where /create_spec writes approved specs

[hq.supervisor]
agent = "claude-code"   # agent for supervisor/reviewer role: claude-code | codex | qwen-code | goose | opencode
model = ""              # optional override; empty = agent default

[hq.executor]
agent = "codex"         # agent for executor role: claude-code | codex | qwen-code | goose | opencode
model = ""              # optional override; empty = agent default
```

## Runtime files

Ferrus separates project-local artifacts from machine-local runtime state:

- `.ferrus/` stores human-readable project files and task/run artifacts.
- `~/.ferrus/projects/<project-id>/` stores machine-local metadata, `ferrus.db`, and logs.

SQLite is the runtime source of truth. Executor task claims and heartbeat renewals are coordinated
through `ferrus.db` task lease columns. `ferrus.db` also stores task status, lifecycle events, reset
events, and HQ-spawned headless runs as the durable substrate for multi-task and multi-executor
coordination. On HQ startup, global project metadata is touched, stale running DB rows whose PIDs are
gone are marked `interrupted`, leases backed by live runs are preserved, and other expired leases are released.

### `.ferrus/`

| File | Contents |
|---|---|
| `project.toml` | Local pointer to `~/.ferrus/projects/<project-id>/` |
| `agents.json` | Runtime registry for agent sessions, statuses, PIDs, and logs |
| `TASK.md` | Task drafting template |
| `CONSULT_TEMPLATE.md` | Read-only consultation request template |
| `SPEC_TEMPLATE.md` | Read-only feature specification template |
| `tasks/` | Task descriptions such as `tasks/t-001.md`; active task files are cleared on reset |
| `runs/` | Execution-attempt artifacts such as `runs/t-001/REVIEW.md`, `SUBMISSION.md`, `QUESTION.md`, `ANSWER.md`, and consultation files; active run files are cleared on reset |
| `logs/check_<attempt>_<scope>_<ts>.txt` | Full check output, uniquely scoped for parallel runs |

### `~/.ferrus/projects/<project-id>/`

| File | Contents |
|---|---|
| `project.toml` | Project id, name, workspace path, git metadata, timestamps, version |
| `ferrus.db` | SQLite database with `tasks`, `runs`, `events`, leases, and `project_runtime_state` |
| `archive/specs/<spec-slug>-<closed-at>/` | Machine-local archives for completed spec task/run artifacts |
| `logs/` | Machine-local logs that should not be committed |
"#;

pub async fn run(agents_path: String) -> Result<()> {
    create_ferrus_toml(&agents_path).await?;
    create_ferrus_dir().await?;
    create_spec_dir().await?;
    create_skill_files(&agents_path).await?;
    let registration = crate::project::register_current_project().await?;
    update_gitignore().await?;
    println!(
        "Registered project {} in {}",
        registration.local_ref.project_id,
        registration.data_dir.display()
    );
    println!("\nferrus initialized. Run `ferrus serve` to start the MCP server.");
    Ok(())
}

async fn create_ferrus_toml(agents_path: &str) -> Result<()> {
    let path = Path::new("ferrus.toml");
    if path.exists() {
        println!("ferrus.toml already exists, skipping.");
    } else {
        // Substitute the agents path into the template
        let content = DEFAULT_FERRUS_TOML
            .replace(r#"path = ".agents""#, &format!(r#"path = "{agents_path}""#));
        tokio::fs::write(path, content)
            .await
            .context("Failed to write ferrus.toml")?;
        println!("Created ferrus.toml");
    }
    Ok(())
}

async fn create_ferrus_dir() -> Result<()> {
    let dir = Path::new(".ferrus");
    tokio::fs::create_dir_all(dir.join("logs"))
        .await
        .context("Failed to create .ferrus/logs/ directory")?;
    tokio::fs::create_dir_all(dir.join("tasks"))
        .await
        .context("Failed to create .ferrus/tasks/ directory")?;
    tokio::fs::create_dir_all(dir.join("runs"))
        .await
        .context("Failed to create .ferrus/runs/ directory")?;

    let consult_template_path = dir.join("CONSULT_TEMPLATE.md");
    if !consult_template_path.exists() {
        tokio::fs::write(&consult_template_path, CONSULT_TEMPLATE)
            .await
            .context("Failed to write .ferrus/CONSULT_TEMPLATE.md")?;
        println!("Created .ferrus/CONSULT_TEMPLATE.md");
    }

    let spec_template_path = dir.join("SPEC_TEMPLATE.md");
    if !spec_template_path.exists() {
        tokio::fs::write(&spec_template_path, SPEC_TEMPLATE)
            .await
            .context("Failed to write .ferrus/SPEC_TEMPLATE.md")?;
        println!("Created .ferrus/SPEC_TEMPLATE.md");
    }

    let task_template_path = dir.join("TASK.md");
    if !task_template_path.exists() {
        tokio::fs::write(&task_template_path, TASK_TEMPLATE)
            .await
            .context("Failed to write .ferrus/TASK.md")?;
        println!("Created .ferrus/TASK.md");
    }

    // Create empty agents registry
    let agents_path = dir.join("agents.json");
    if !agents_path.exists() {
        let empty = crate::state::agents::AgentsRegistry::default();
        let json = serde_json::to_string_pretty(&empty)?;
        tokio::fs::write(&agents_path, json)
            .await
            .context("Failed to write .ferrus/agents.json")?;
        println!("Created .ferrus/agents.json");
    }

    Ok(())
}

async fn create_spec_dir() -> Result<()> {
    let path = Path::new("docs/specs");
    let existed = path.exists();
    tokio::fs::create_dir_all(path)
        .await
        .context("Failed to create docs/specs/ directory")?;
    if !existed {
        println!("Created docs/specs/");
    }
    Ok(())
}

async fn create_skill_files(agents_path: &str) -> Result<()> {
    let skills_root = Path::new(agents_path).join("skills");

    // General ferrus skill
    let ferrus_dir = skills_root.join("ferrus");
    tokio::fs::create_dir_all(&ferrus_dir)
        .await
        .with_context(|| format!("Failed to create {}", ferrus_dir.display()))?;
    let ferrus_skill_path = ferrus_dir.join("SKILL.md");
    if !ferrus_skill_path.exists() {
        tokio::fs::write(&ferrus_skill_path, FERRUS_SKILL)
            .await
            .with_context(|| format!("Failed to write {}", ferrus_skill_path.display()))?;
        println!("Created {}", ferrus_skill_path.display());
    }

    // Role-specific skill + role definition files
    for (role, skill, role_def) in [
        ("ferrus-supervisor", SUPERVISOR_SKILL, SUPERVISOR_ROLE),
        ("ferrus-executor", EXECUTOR_SKILL, EXECUTOR_ROLE),
    ] {
        let skill_dir = skills_root.join(role);
        tokio::fs::create_dir_all(&skill_dir)
            .await
            .with_context(|| format!("Failed to create {}", skill_dir.display()))?;

        let skill_path = skill_dir.join("SKILL.md");
        if !skill_path.exists() {
            tokio::fs::write(&skill_path, skill)
                .await
                .with_context(|| format!("Failed to write {}", skill_path.display()))?;
            println!("Created {}", skill_path.display());
        }

        let role_path = skill_dir.join("ROLE.md");
        if !role_path.exists() {
            tokio::fs::write(&role_path, role_def)
                .await
                .with_context(|| format!("Failed to write {}", role_path.display()))?;
            println!("Created {}", role_path.display());
        }
    }
    Ok(())
}

async fn update_gitignore() -> Result<()> {
    let path = Path::new(".gitignore");
    let entries = [".ferrus/"];

    if path.exists() {
        let mut contents = tokio::fs::read_to_string(path)
            .await
            .context("Failed to read .gitignore")?;

        let mut added_entries = Vec::new();
        for entry in entries {
            if contents.lines().any(|line| line == entry) {
                continue;
            }

            if !contents.is_empty() && !contents.ends_with('\n') {
                contents.push('\n');
            }
            contents.push_str(entry);
            contents.push('\n');
            added_entries.push(entry);
        }

        if added_entries.is_empty() {
            return Ok(());
        }

        tokio::fs::write(path, contents)
            .await
            .context("Failed to update .gitignore")?;

        for entry in added_entries {
            println!("Added {entry} to .gitignore");
        }
    } else {
        let contents = format!("{}\n", entries.join("\n"));
        tokio::fs::write(path, contents)
            .await
            .context("Failed to create .gitignore")?;
        println!("Created .gitignore");
    }
    Ok(())
}
