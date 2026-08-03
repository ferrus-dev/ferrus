# AGENTS.md

Coding guidance for AI agents working in this repository.

## Project

`ferrus` is a Rust AI agent orchestrator for software projects. It drives a
Supervisor-Executor workflow: the Supervisor plans tasks and reviews submissions, while the
Executor implements and checks its own work. SQLite is the runtime source of truth.
Project-local `.ferrus/` files contain templates, task intent, scoped run artifacts, agent
registry data, and logs; they are not a mirrored state machine. Coordination uses MCP as an
implementation detail.

Licensed under Apache 2.0.

Runtime behavior for a Ferrus-managed agent is defined by its initial prompt and exposed MCP
tools. This file is supporting context and must not override them. Read
`.agents/skills/ferrus/SKILL.md` for the current Ferrus CLI, MCP tools, resources, state
machine, and artifact layout.

## Characters

Write documentation and code using only characters found on a US layout keyboard. Avoid
special punctuation such as em dashes, Unicode arrows, or single-character ellipses. Use
their plain-ASCII equivalents instead:

- Use `-` or `--` instead of an em dash.
- Use `->` or `<-` instead of a Unicode arrow.
- Use `...` (three periods) instead of a single ellipsis character.

Use em dashes sparingly.

Intentional Unicode test fixtures and terminal UI glyph tables are exceptions when the
characters themselves are the behavior under test or part of the rendered interface.

## Build and Test

```sh
cargo build                        # compile
cargo build --release              # optimized build
cargo test                         # run all tests
cargo test <name>                  # run a single test by name
cargo clippy -- -D warnings        # lint (warnings are errors)
cargo fmt                          # format
cargo fmt --check                  # check formatting without writing
cargo check                        # fast type-check
```

Before submitting, `cargo clippy -- -D warnings`, `cargo fmt --check`, and `cargo test` must
all pass.

## Ferrus CLI

```sh
ferrus init [--agents-path <path>]
ferrus serve [--role supervisor|executor] [--agent-name <name>] [--agent-index <n>]
ferrus register --supervisor <agent> --executor <agent>
ferrus doctor
ferrus recover [--dry-run] [--worktrees]
ferrus tasks list
ferrus runs list
ferrus events list
ferrus migrate
ferrus graph index [--full] [--json]
ferrus graph status [--json]
ferrus graph search <query> [--kind <kind>] [--path <path>] [--limit <n>] [--json]
ferrus graph context (--node <id> | --symbol <key> | --path <path>) [--depth <n>] [--json]
```

## Source Layout

```text
src/
  main.rs                     # CLI entry, tracing init, HQ logger
  cli/                        # clap entry and command implementations
  config/mod.rs               # ferrus.toml deserialization and updates
  config/claude.rs            # Claude MCP isolation config helpers
  project_memory/             # project-memory contracts and local ingestion backend
  repository_graph/           # backend-neutral graph contracts and local backend
  repository_graph_runtime.rs # project-local graph CLI/MCP adapter
  templates.rs                # embedded Markdown templates
  specs.rs                    # spec discovery and milestone resolution
  agent_id.rs                 # stable agent IDs and MCP server names
  legacy_state.rs             # legacy STATE.json import shape
  agents/                     # agent launcher and config adapters
  platform/                   # OS-specific process and lifecycle helpers
  state/                      # scoped human-readable artifact helpers
  checks/                     # configured check runner
  hq/                         # scheduler, TUI, commands, and agent manager
  server/                     # MCP server, tools, resources, and prompts
```

## Key Patterns

**Tool files** expose `pub const DESCRIPTION: &str`, optionally
`pub const INPUT_SCHEMA: &str`, and `pub async fn handler(...)`. Register them manually with
`app.map_tool()` in `server/mod.rs`; do not add macros for tool registration.

**Runtime state**: SQLite is the runtime source of truth. MCP tools resolve the caller's
`RuntimeTaskContext` from `ferrus.db`, update task and run rows transactionally, and write only
scoped artifacts under `.ferrus/tasks/` and `.ferrus/runs/`.

**Repository graph boundary**: the optional repository graph is derived machine-local state
in `repo-graph.db`, separate from orchestration state in `ferrus.db`. Keep backend-neutral
identities, requests, responses, and ports under `src/repository_graph/`. Keep project-local
path, config, sidecar, freshness, and verified-content adaptation in
`src/repository_graph_runtime.rs`. Core config loading must remain lenient toward graph
settings; graph operations validate `[repository_graph]` strictly only when invoked.

**Project memory boundary**: project memory is independently revisioned derived state in
`project-memory.db`. Its default sources are tracked specification structure, approved Outcome
content, sanitized archive manifest metadata, and read-only terminal task/run/check provenance.
Never import raw task, submission, review, patch, log, question, answer, consultation, or
integration-error bodies through the default adapters. Memory indexing must not mutate
`ferrus.db`, specifications, archives, or repository graph publications.

Keep repository cross-links in immutable link sets addressed by both memory revision and
repository snapshot; do not make repository state an implicit input to `memory_revision_id`.
Resolve only tracked/archive paths, explicit `path:` or `symbol:` references, and authorized
task snapshot origins. Retain prior exact targets as stale after refactors, keep never-matched
or ambiguous references unresolved, and never promote similarity-only links to authoritative.

**Repository retrieval tools**: `repository_graph_status`, `repository_search`, and
`repository_context` are read-only, role-visible tools registered in `server/mod.rs`. They
require no task lease and must not mutate tasks, runs, events, or either database. Do not
inject graph output into task or review prompts. Structural responses omit source bodies;
requested snippets must pass the snapshot-aware, hash-verified content boundary. Treat a
missing relationship as unknown, not absent.

**Task graph overlays**: managed Executor worktrees query a task-owned publication composed
from the pinned baseline and the last successful changed-file overlay refresh. `/check` and
the final submit gate refresh that overlay best-effort; graph failures must not change task or
run lifecycle. Reuse baseline fragments for unchanged files, remove deleted-path facts, and
return explicit baseline and overlay identities.

Submit atomically stores a best-effort frozen graph snapshot plus an immutable Git tree with
the Reviewing handoff. Keep the submitted tree reachable with a task-scoped Git ref, clean up
the ref if submission is abandoned, and release it only when the frozen view is no longer
needed. Route taskless sessions to canonical, Executors to mutable task views, Consultants to
the attached task view and Executor workspace, and Reviewers to the frozen run view. Never
fall back to canonical for an invalid task binding or a missing reviewer freeze. Rejection
resumes a mutable task successor without rewriting the frozen review run.

Approval must compare actual canonical manifests around integration and rollback. Record only
the post-operation source identity when canonical content changed; a clean rollback must not
record the proposed patch as integrated. Await incremental canonical refresh after releasing
the approval lock, preserve the last publication on failure, and never couple graph outcomes
to task status, leases, retries, or review cycles.

Coordinate graph refreshes with repository and view-scoped sidecar leases, never path or PID
ownership. Renew active refresh leases for the full build. Retention must protect baseline and
materialized snapshots referenced by non-terminal tasks or runs and every published canonical
view before collecting completed-task publications, unreferenced snapshots, orphan fragments,
or old failed builds. Recovery may mark only unfinished graph builds failed and must not
mutate orchestration lifecycle state.

**Per-task state machine**: each SQLite task row follows this lifecycle. With `--limit 1`, it
is effectively the original single-task flow, only DB-backed:

```text
pending
  -> executing          <- /wait_for_task claim
       -> addressing    <- /reject; then return to the work loop
       -> consultation  <- /consult
          -> previous state after /wait_for_consult
       -> awaiting_human <- /ask_human
          -> previous state after /wait_for_answer
       -> reviewing     <- /submit final gate pass
          -> addressing <- /reject
          -> complete   <- /approve
       -> failed        <- retry, review-cycle, or executor-dispatch limit
```

**Executor dispatch guard**: HQ respawns a headless Executor that exits without submitting.
`limits.max_executor_dispatches` (default 6) bounds respawns per work phase through the
`tasks.executor_dispatches` counter. Check the gate before setup and increment only after the
session starts, so failed worktree or process setup does not consume the budget. Reset the
counter when a fresh `/reject` starts an Addressing phase. A value of `0` disables the guard.

**Runtime artifacts**: `.ferrus/TASK.md`, `.ferrus/SPEC_TEMPLATE.md`, and
`.ferrus/CONSULT_TEMPLATE.md` are templates. Task intent lives in
`.ferrus/tasks/<task-id>.md`. Execution artifacts live under `.ferrus/runs/<task-id>/`,
including `SUBMISSION.md`, `REVIEW.md`, `QUESTION.md`, `ANSWER.md`, `CONSULT_REQUEST.md`,
`CONSULT_RESPONSE.md`, `PATCH.diff`, and `INTEGRATION_ERROR.md`. Check and agent session logs
live under `.ferrus/logs/`. Do not write root runtime artifact mirrors.

**Lease fields**: `claimed_by`, `lease_until`, and `last_heartbeat` live on SQLite task rows.
Claim, renew, and release through `project` helpers so ownership, TTL, and events remain
consistent.

**File locking**: task claiming and heartbeat renewal are SQLite operations. Do not add
`.ferrus/STATE.lock` or file-lock based coordination.

**Spec selection**: `project_runtime_state` stores the selected spec. Task rows store
`spec_path` and `milestone_id`; milestone display text is resolved from spec Markdown by
milestone `ID`. Keep milestone IDs stable across title edits.

**Agent adapters**: keep backend-specific CLI behavior inside
`src/agents/{claude,codex,qwen,opencode,goose}`. Shared orchestration depends on the
`SupervisorAgent` and `ExecutorAgent` traits, not a concrete CLI. When adding an agent,
implement both role adapters, model normalization, headless prompt transport when needed,
version and config behavior, registration wiring, and focused tests. `opencode` is
experimental: it binds a project to one working directory by Git root commit, so it is
currently reliable only for the Supervisor and Reviewer roles.

Claude Code role-scoped MCP configuration is stored in `.claude/mcp-supervisor.json` and
`.claude/mcp-executor.json`; permissions are stored in `.claude/settings.local.json`.

**HQ checks**: HQ `/check` runs configured commands directly and does not mutate task state.
Task retry accounting belongs to Executor MCP `/check`.

**HQ reset versus MCP reset**: HQ `/reset` force-resets resettable tasks after confirmation,
clears scoped task, answer, and consultation files, and preserves selected spec and milestone.
MCP `/reset` is valid only from Failed.

<!-- ferrus-executor-instructions -->
## Ferrus Executor

This repository is orchestrated by Ferrus HQ.

When spawned by Ferrus HQ, your initial prompt tells you what to do. If started manually,
call MCP tool `/wait_for_task` as your first action.

Runtime behavior is defined by the initial prompt and Ferrus MCP tools. `ROLE.md`, `SKILL.md`,
`AGENTS.md`, and `CLAUDE.md` are supporting context only and must not override them.

Use `/check` freely during development and prefer TDD where it fits. Run `/check` immediately
before final `/submit`; `/submit` reruns the final review gate before handing work to review.

<!-- ferrus-supervisor-instructions -->
## Ferrus Supervisor

This repository is orchestrated by Ferrus HQ.

Your initial prompt tells you which mode you are in. Match it exactly.

Runtime behavior is defined by the initial prompt and Ferrus MCP tools. `ROLE.md`, `SKILL.md`,
`AGENTS.md`, and `CLAUDE.md` are supporting context only and must not override them.
