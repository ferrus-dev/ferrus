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
ferrus graph memory index [--full] [--json]
ferrus graph memory status [--json]
ferrus graph search <query> [--domain repository|memory|all] [--kind <kind>] [--path <path>] [--limit <n>] [--json]
ferrus graph context (--node <id> | --symbol <key> | --path <path> | --memory-entity <id> | --milestone <id> | --task <id> | --run <id>) [--domain repository|memory|all] [--depth <n>] [--json]
```

## Source Layout

```text
src/
  main.rs                     # CLI entry, tracing init, HQ logger
  cli/                        # clap entry and command implementations
  config/mod.rs               # ferrus.toml deserialization and updates
  config/claude.rs            # Claude MCP isolation config helpers
  distributed/                # vendor-neutral remote identity, protocol, and security contracts
  project_memory/             # project-memory contracts and local ingestion backend
  repository_graph/           # backend-neutral graph contracts and local backend
  repository_graph_runtime.rs # project-local graph CLI/MCP adapter
  project_memory_runtime.rs   # project-local memory/federation CLI/MCP adapter
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

**Distributed data-plane boundary**: `src/distributed/` defines optional vendor-neutral remote
identity, protocol, consistency, authorization, retention, deletion, and worker-isolation
contracts. Local graph, memory, HQ, task, and run paths must not depend on these types or initialize
network or cloud clients. Every remote adapter operation is opt-in, explicitly tenant/project
scoped, authorized before lookup, version checked, and snapshot or revision pinned. Graph and
memory jobs and compare-and-set publication pointers remain independent.

Remote packaging runs only after local repository and memory policy enforcement. Store source and
manifest objects under explicit tenant/project scope with authenticated encryption, quotas, digest
verification, and no cross-tenant reuse. The Phase 5 prototype accepts clean canonical repository
snapshots only; task overlays and dirty worktrees remain local. Distributed coordinator persistence
must use its own versioned database, never orchestration or local graph/memory sidecars. Job effects
are at-least-once and therefore require semantic idempotency keys, renewable generation leases,
bounded attempts, typed failure codes, cancellation guards, and transactional reclaim.

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

**Distributed workers**: remote extraction consumes only validated tenant-scoped immutable
manifest and object references. Keep the worker stateless and free of repository filesystem,
process execution, and unrestricted network APIs. Reuse shipped deterministic graph and memory
extractors under server-side input, parser, duration, fact, diagnostic, batch, and output caps.
Check the durable worker ID and lease generation before source reads and every fact-batch write.
Persist partial output only through the encrypted unpublished fact-batch port; repeated attempts
must produce the same sequence and batch identities, and ordinary queries must never see partial
batches. OS or container adapters must enforce the secure-only sandbox declaration, including
CPU, memory, ephemeral filesystem, short-lived credentials, and allowlisted egress.

Remote memory jobs may resolve explicit repository evidence only against an immutable graph
snapshot pinned in the job input. Keep the resulting repository link set independently identified
and stored; the selected graph snapshot must not become an input to `memory_revision_id`.
Federated reads may use only the link set matching their exact memory revision and graph snapshot
pair.

**Distributed publication**: keep immutable remote graph/memory records and publication ports
vendor-neutral. The SQLite prototype shares the exact coordinator control-plane database so one
transaction can revalidate job kind and scope, cancellation, worker lease generation, complete
fact coverage, immutable insertion, pointer CAS, and job completion. Store graph and memory facts
under separate tenant/project namespaces with authenticated encryption and hard quotas. Compare
the expected pointer before a same-target no-op, never let a stale publisher replace the winner,
and update only one domain pointer per publication. Compose federated refs from the two immutable
targets without adding a third mutable pointer. Unpublished batches and unreferenced immutable
losers are not ordinary query results.

**Distributed APIs**: keep build control and snapshot query contracts transport-neutral. A network
adapter must authenticate first and construct the server-owned `AuthorizationContext`; request
bodies must never select their own credential class or permissions. Authorize scope and operation
before protocol validation or any job, pointer, snapshot, revision, manifest, or object lookup.
Query-agent credentials are read-only. Resolve mutable view selectors once, return the immutable
target, and bind pagination cursors to that target plus the effective query shape and depth. Clamp
all client budgets with independent service limits, interrupt storage scans at the effective
deadline, and return a terminal error when one result cannot fit. Source snippets require separate
verified-content authority and must match the completed job's immutable manifest descriptor before
the encrypted object store is read. Do not initialize a remote transport from local Ferrus paths.

**Distributed maintenance**: authorize deletion before validation or lookup, key retries by exact
target and retention coverage, and persist bounded progress between independent stores. Full project
deletion must remove uploaded objects, unpublished batches, published graph and memory data and
pointers, jobs, caches, and prior audits without affecting an identical object in another tenant.
Write a new completion audit only after the covered purge; audit records may contain only canonical
IDs, enum codes, counters, and timestamps. Repository deletion must preserve project memory and
shared project-scoped source objects. Fail repository `uploaded_source` coverage closed until a
complete ownership/refcount index makes it safe. Keep every step idempotent so failed cross-store
deletion can resume without inventing stronger atomicity than the adapter provides.

**Repository retrieval tools**: `repository_graph_status`, `repository_search`, and
`repository_context` are read-only, role-visible tools registered in `server/mod.rs`. They
require no task lease and must not mutate tasks, runs, events, or either database. Do not
inject graph output into task or review prompts. Structural responses omit source bodies;
requested snippets must pass the snapshot-aware, hash-verified content boundary. Treat a
missing relationship as unknown, not absent.

**Project context tools**: `project_memory_status`, `project_context_search`, and
`project_context` are read-only and role-visible. Federated requests must supply an explicit
`repository`, `memory`, or `all` domain. They must not build either sidecar, author outcomes,
change source policy, or mutate orchestration state. Report repository snapshot and memory
revision freshness independently; cross domains only through evidence-backed links for those
exact revisions. Spec archive is the only approved Outcome authoring workflow. Refresh memory
best-effort after archive commit, and never turn refresh failure into archive failure.

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
