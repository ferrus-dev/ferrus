# Ferrus Milestones

This document tracks the current direction for `ferrus` and the status of the
major roadmap areas. It is not a date-based roadmap.

Last reviewed against the repository: 2026-09-07.

## Guiding principles

- `ferrus` should remain a reliable orchestrator, not a fragile collection of scripts wrapped around LLMs.
- New capabilities must not weaken the core Supervisor-Executor loop for a single task.
- Major architectural changes should be introduced behind clear abstractions rather than hard-coding current implementation details.
- Local-first workflows matter. `ferrus` should work well without mandatory dependence on cloud-only services.
- The system should get more capable without forcing agents to repeatedly rediscover the same repository context from scratch.

## Status summary

| Area | Status | Notes |
|---|---|---|
| Windows support | Mostly implemented | Windows platform hooks, shell execution, installer, Windows CI, and smoke tests exist. Real agent-loop validation and support-policy docs still need tightening. |
| Storage layer and SQLite backend | Done | Versioned SQLite migrations, tasks, runs, events, leases, counters, selected spec state, and recovery. Markdown remains scoped human-readable artifacts. |
| Event log and observability | Baseline done | Runtime events, task/run/event CLI views, HQ dashboard panels, and recovery inspection are implemented. Replay/export and richer historical views remain future work. |
| Pluggable agent adapters | Partially done | Shared `SupervisorAgent`/`ExecutorAgent` traits and adapters for Codex, Claude Code, Qwen Code, goose, and opencode exist. Explicit capability contracts and runnable native agents remain future work. |
| Multi-agent flow | Partially done | `/run`, queued tasks, `max_parallel_tasks`, per-task leases, worktree isolation, independent review, frozen submissions, three-way integration with rollback, and integration-error reporting exist. Full task graph, decomposition contracts, and final integration policy remain open. |
| Spec closure and project memory | Local baseline implemented | Outcome archival, curated memory indexing, revision-pinned queries, and evidence-backed repository links exist. Raw runtime bodies are excluded from default ingestion. |
| Repository graph and indexed context | Local baseline implemented | Optional SQLite sidecar, incremental extraction, bounded CLI/MCP retrieval, task overlays, and frozen review views. Rust/Cargo and generic file structure are supported. |
| Distributed context data plane | Prototype implemented | Opt-in contracts and local prototype adapters for authorized jobs, encrypted storage, publication, queries, and maintenance. No deployed remote service is implied. |
| Ferrus nano-agent | Foundation implemented; runtime planned | #73 adds native binding and claim/status/heartbeat; #74 adds the bounded engine/journal; #75 adds opt-in LM Studio Chat Completions; #76 adds bounded native read/search/patch tools. Live model validation, HQ launch, interactive UI, and standalone delivery remain pending. |

## Milestone 1: Windows Support

Status: mostly implemented.

Goal: make `ferrus` genuinely cross-platform so HQ, state management, agent spawning,
and checks work reliably on Linux, macOS, and Windows.

What is implemented:

- platform-specific process, shell, parent-lifecycle, TUI cleanup, and headless process hooks live under `src/platform/`;
- Windows uses `cmd /C` for configured checks and Win32 job objects for best-effort headless process cleanup;
- Windows-specific Codex launcher handling exists for npm-style Codex installations;
- release metadata includes the Windows target, and `install.ps1` exists;
- CI runs `fmt`, `clippy`, tests, `cargo build`, `ferrus init`, and `ferrus doctor` on `windows-latest`.

What remains:

- document the Windows support policy and known limitations;
- validate the full Supervisor-Executor loop with real supported agent backends on Windows, not only init/doctor smoke tests;
- tighten Windows process-tree cleanup where backend CLIs spawn children that are not covered by the current root-process fallback;
- keep backend-specific Windows launch behavior current as agent CLI packaging changes.

## Milestone 2: Storage Layer and SQLite Backend

Status: done.

Goal: remove the direct coupling between runtime state and markdown/json files by introducing
a real storage layer, with SQLite as the primary backend for state, tasks, reviews, logs, and history.

What is implemented:

- `~/.ferrus/projects/<project-id>/ferrus.db` stores runtime task rows, run rows, events, leases, counters, failure metadata, and project runtime state;
- `.ferrus/project.toml` points the checkout to the machine-local project registry;
- `.ferrus/tasks/<task-id>.md` and `.ferrus/runs/<task-id>/` are scoped human-readable artifacts, not the runtime state machine;
- `ferrus init`, `migrate`, `doctor`, `recover`, `projects list`, `tasks list`, `runs list`, and `events list` operate on the SQLite-backed runtime;
- MCP tools resolve scoped runtime task context from SQLite and update task rows transactionally;
- legacy `STATE.json` is only an import source for migration and is removed by migration paths;
- ordered SQLite schema migrations record their history and validate the current schema version.

What remains:

- richer event querying, export, and replay remain future observability work.

## Milestone 3: Repository Graph and Indexed Context

Status: local baseline implemented; broader language coverage and operational evaluation continue.

Goal: reuse structured repository context through bounded queries, with explicit identity and
freshness, while keeping source files and Git authoritative.

What is implemented:

- backend-neutral graph contracts and a rebuildable `repo-graph.db` sidecar independent of `ferrus.db`;
- incremental indexing with Rust, Cargo, and generic file/document/configuration extraction;
- CLI index/status/show/neighbors/search/context commands and read-only MCP retrieval tools;
- bounded responses and pagination, evidence-backed relationships, and hash-verified snippets;
- task baseline plus changed-file overlays, refresh after checks, and frozen submission/review views;
- scoped refresh leases, retention, recovery, and best-effort refresh after canonical integration;
- local retrieval fixtures and performance/evaluation tooling.

What remains:

- expand language-specific extraction and cross-file resolution beyond Rust/Cargo;
- broaden task-level quality and token/cost evaluation on realistic repositories;
- consume graph and memory APIs natively from nano in #78, then add working-set policy in #81;
- keep graph failures independent of task lifecycle, and retain explicit fallback when context is unavailable.

See [graph architecture](repository-graph-architecture.md),
[retrieval](repository-graph-retrieval.md), and [evaluations](repository-graph-evaluations.md).
Missing graph relationships remain unknown, not proof that no relationship exists.

### Distributed context data plane

Status: optional prototype implemented under `src/distributed/`.

The prototype includes scoped identity and authorization, source packaging, encrypted object/fact
storage, durable coordinator leases and retries, bounded workers, atomic publication, pinned query
APIs, and resumable maintenance. Repository and memory revisions remain independent. Local HQ,
indexing, and retrieval do not initialize cloud clients or implicitly upload source.

Production network/storage adapters, deployment, credentials, enforced worker isolation, and
operational validation remain separate work. The prototype's secure worker requirements are a
contract for such adapters, not evidence of a deployed sandbox or cloud service.
See [distributed contracts](distributed-indexing-architecture.md).

## Milestone 4: Multi-Agent Flow

Status: partially implemented.

Goal: move from single-task execution to coordinated parallel work,
where multiple executors can operate independently and the supervisor manages decomposition and integration.

What is implemented:

- specs can define stable milestone IDs and dependencies;
- HQ can select a spec and derive ready milestones deterministically;
- `/run` and `/run --limit N` ask the supervisor to prepare a fixed batch of milestone-derived queued tasks;
- `/enqueue_task` creates pending SQLite task rows with optional `spec_path` and `milestone_id`;
- duplicate active work for the same `(spec_path, milestone_id)` is rejected;
- HQ schedules pending/executing/addressing tasks up to `limits.max_parallel_tasks`;
- each task has its own lease, run records, scoped artifacts, and check logs;
- executor sessions run in managed git worktrees under the project runtime directory;
- submissions preserve `PATCH.diff`, a reachable immutable Git tree, and best-effort frozen graph context;
- approval integrates baseline, current canonical tree, and frozen submitted tree using three-way merge semantics;
- conflicts and failed integration checks produce scoped `INTEGRATION_ERROR.md`; rollback restores the captured canonical tree;
- Executor respawns are bounded per work phase by `max_executor_dispatches`.

What remains:

- introduce a real task graph for dependencies between queued work items, not just spec milestone readiness;
- define supervisor-owned decomposition contracts for large tasks that are not already represented as spec milestones;
- make the final integration policy explicit: conflict ownership, retry strategy, ordering, partial failure behavior, and operator visibility;
- improve dashboard visibility for parallel integration state and blocked dependencies;
- harden the worktree path for every supported executor backend. `opencode` remains unsuitable for executor worktree isolation because of its own global project binding.

Definition of done:

- one large task can be split and completed by multiple executors in parallel;
- each part runs through its own review loop;
- final integration is reproducible, understandable to the operator, and covered by documented conflict-handling rules.

## Milestone 5: Ferrus Nano-Agent

Status: native session, engine/journal, provider, and file tools implemented (#73-#76); the runnable harness is not available yet.

Goal: build `ferrus-nano` (backend `nano`) as a minimal Rust coding-agent harness. Start with a
headless managed Executor, using Ferrus operations, repository graph, and project memory through
direct Rust calls. Use neva for external MCP extensions. Keep the engine independent of the UI
so interactive HQ, standalone delivery, and additional roles can follow.

The accepted layout adds `src/nano/`. Existing project, checks, graph/memory adapters, and MCP
tool files retain their responsibilities. Extract small typed helpers in place where needed;
add `src/shared/` only for implementations actually shared by HQ and nano. No broad reorganization
or mandatory library migration is part of this track.

Implemented foundation:

- host-owned project/agent/task/run/workspace/baseline binding from launch data and registered runtime state;
- native typed claim, status, and heartbeat, with exact run validation at transaction boundaries;
- sequential provider/tool/host boundaries, persisted budgets, cancellation, a private single-writer journal, and pure recorded replay;
- an opt-in Chat Completions adapter targeting LM Studio, with private credential-file references, bounded streaming, and shared retry accounting;
- bounded native workspace read/search and exact digest-checked patch tools, with protected runtime paths and explicit partial-edit results ([contract](ferrus-nano-workspace.md));
- regression coverage for bindings, lease ownership, MCP parity, engine limits, effect ordering, journal recovery, and offline provider protocols. The live provider smoke test remains opt-in.

Delivery is tracked in [Ferrus nano-agents](https://github.com/ferrus-dev/ferrus/milestone/6).
The [architecture and complete PR index](ferrus-nano-architecture.md#planned-prs-and-github-issues)
contains one issue per planned PR, dependencies, and acceptance criteria:

| Stage | Issues | Remaining scope |
| --- | --- | --- |
| N1: headless Executor | #77-#80 | Live model validation, command sessions, native context, lifecycle operations, and HQ launch/events |
| N2: context efficiency | #81-#82 | Working-set invalidation, budgets, and compaction |
| N3: reliability and extensions | #83-#85 | External MCP via neva, resume/reconciliation, comparative evaluation, and headless release gates |
| N4/N5: interactive and standalone | #86-#88 | HQ interaction, standalone host/binary, and shared UI |
| N5: additional roles | #89-#90 | Supervisor planning/spec/archive, Reviewer, and Consultant |

Definition of done for the first headless release:

- HQ can launch nano as an Executor, with existing checks, submit, review, lease, and workspace rules;
- graph-disabled, stale-context, cancellation, and recovery cases have explicit behavior;
- a fixed task suite measures quality, token use, cost, and elapsed time against external integrations.

Lower cost, higher determinism, and better throughput are hypotheses until evaluated. Interactive
and standalone delivery remain planned extensions, not requirements to ship the headless Executor.

## Supporting Tracks

### Event log and observability

Status: baseline implemented.

`ferrus.db` now records runtime events, and users can inspect tasks, runs, and events from the CLI.
HQ also has a dashboard foundation that surfaces project state, selected milestones, runtime activity,
errors, and pending human questions.

Future work should focus on historical analysis rather than basic event capture: replay, export,
filtering by task/run/spec, richer dashboard timelines, and better diagnostics for integration failures.

### Pluggable execution and runtime interfaces

Status: partially implemented.

The orchestration layer depends on shared supervisor/executor traits instead of one concrete CLI,
and backend-specific launch/config behavior is isolated in `src/agents/*`.

Future work should make backend capabilities explicit: worktree safety, model/provider metadata,
tool reliability assumptions, context-window limits, local-model suitability, and whether a backend
can safely run as executor, reviewer, consultant, or nano-agent provider.

### Task decomposition and merge policy

Status: partially implemented.

Spec milestones already provide a coarse decomposition model, and `/run` can turn ready milestones
into queued tasks. Approval merges each frozen submission into the current canonical tree and
records recoverable integration errors, preserving unrelated canonical changes.

Future work should define decomposition and integration as first-class policies, not just scheduler behavior:
task contracts, file ownership hints, dependency edges, conflict routing, merge ordering, and how a supervisor
should re-plan when one parallel branch fails.

### Spec closure and project memory

Status: local baseline implemented.

`/archive-spec` requires completed work, uses Supervisor spec-closure mode to prepare an approved
`## Outcome`, and archives scoped task/run artifacts into machine-local project history. SQLite
retains task/run provenance; the tracked spec retains the compact outcome. `/spec` offers archival
before switching away from a completed selected spec.

The independent `project-memory.db` sidecar indexes tracked specification structure, approved
Outcomes, sanitized archive metadata, and read-only terminal runtime provenance. Default adapters
exclude raw submissions, reviews, patches, logs, questions, answers, and consultations. Repository
cross-links identify exact memory revision and graph snapshot pairs; similarity is not authority.
CLI and MCP support memory and federated retrieval with independent freshness reporting. Memory
refresh after archival is best-effort and cannot turn a completed archive into a failure.

Remaining work:

- richer archive inspection and optional portable export;
- wider evaluation of retrieval quality and stale/unresolved cross-links;
- native context consumption in nano (#78).

See [project memory](project-memory.md), [architecture](project-memory-architecture.md), and
[evaluations](project-memory-evaluations.md).

## Proposed order

1. Complete the nano headless Executor sequence (#73-#85), then evaluate it before performance claims.
2. Continue graph language coverage and repository/memory retrieval quality work alongside that sequence.
3. Close real Windows agent-loop validation and support-documentation gaps.
4. Define task dependency, decomposition, conflict-routing, and partial-failure policies beyond current milestone scheduling.
5. Improve runtime history, archive inspection/export, and explicit external-backend capability metadata.
6. Add nano interaction and standalone delivery (#86-#88), then additional roles (#89-#90).
7. Evolve the distributed prototype only through explicit deployment and security/operational acceptance gates.

## Non-goals for now

- turning this roadmap into a date-driven quarterly plan;
- committing to delivery dates before the core architecture stabilizes;
- adding major product surface area before strengthening the orchestration core;
- replacing useful human-readable task/run artifacts with opaque database-only state.
