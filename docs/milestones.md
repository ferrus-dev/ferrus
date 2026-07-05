# Ferrus Milestones

This document tracks the current direction for `ferrus` and the status of the
major roadmap areas. It is not a date-based roadmap.

Last reviewed against the repository: 2026-07-05.

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
| Storage layer and SQLite backend | Done | SQLite is the runtime source of truth for tasks, runs, events, leases, counters, selected spec state, and recovery. Markdown remains scoped human-readable artifacts. |
| Event log and observability | Baseline done | Runtime events, task/run/event CLI views, HQ dashboard panels, and recovery inspection are implemented. Replay/export and richer historical views remain future work. |
| Pluggable agent adapters | Partially done | Shared `SupervisorAgent`/`ExecutorAgent` traits and adapters for Codex, Claude Code, Qwen Code, goose, and opencode exist. Capability contracts and native ferrus agents remain future work. |
| Multi-agent flow | Partially done | `/run`, queued tasks, `max_parallel_tasks`, per-task leases, worktree isolation, independent review, patch application, and integration-error reporting exist. Full task graph, decomposition contracts, and final integration policy remain open. |
| Spec closure and project memory | Not started | Planned `/archive-spec` should summarize completed spec work into a durable `## Outcome` section and move raw task/run artifacts out of the checkout. |
| Repository graph and indexed context | Not started | No reusable repository index or query API exists yet. |
| Ferrus nano-agent | Not started | Local-model-friendly external adapters exist, but no ferrus-native lightweight agent runtime exists yet. |

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
- legacy `STATE.json` is only an import source for migration and is removed by migration paths.

What remains:

- schema versioning and migrations should become explicit before the database grows much further;
- richer event querying, export, and replay remain future observability work.

## Milestone 3: Repository Graph and Indexed Context

Status: not started.

Goal: build a repository graph during initialization and keep it available as reusable structured context
so agents can navigate the codebase faster and spend fewer tokens rediscovering the same information.

What the repository graph should eventually capture:

- file and module structure;
- symbols and their relationships where available;
- dependency edges between components;
- documentation and configuration entry points;
- a compact representation that can be queried incrementally rather than regenerated from scratch every run.

Open architectural direction:

- add a stable indexing abstraction first, then decide whether the initial backend is SQLite tables, a sidecar file index, or a hybrid;
- expose context through Ferrus MCP resources/tools instead of embedding index-specific behavior in agent prompts;
- keep the index optional and rebuildable so `ferrus init` remains lightweight.

Definition of done:

- `ferrus init` or a follow-up indexing command can build repository context ahead of agent execution;
- agents can query this context instead of rescanning the entire repo by default;
- context retrieval is cheap enough to improve both token efficiency and practical task throughput.

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
- submissions preserve `PATCH.diff`, review context exposes that patch, and approval applies it to the canonical checkout;
- failed patch application or post-approve checks create scoped `INTEGRATION_ERROR.md`, update SQLite failure state, and are surfaced to review.

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

Status: not started.

Goal: add lightweight ferrus-native agents that `ferrus` can manage directly,
using local or remote LLMs without depending only on external coding agents.

What exists today:

- the agent layer is already trait-based (`SupervisorAgent` and `ExecutorAgent`);
- multiple external backends are supported, including local-model-friendly goose and opencode adapters;
- goose can be useful with local providers, but it is still an external agent backend, not a ferrus-native nano-agent runtime;
- opencode is currently reliable for supervisor/reviewer use only, not isolated executor workflows.

What remains:

- define a minimal ferrus-native agent runtime;
- support local and remote model providers behind a clear provider interface;
- define a capability model for what nano-agents can read, edit, check, or submit;
- add evaluation and quality gates for small or specialized tasks;
- integrate nano-agents as first-class orchestration participants without weakening the existing external-agent loop.

Definition of done:

- `ferrus` can launch its own mini-agents as first-class orchestration participants;
- there is at least one practical workflow where nano-agents improve cost, speed, or quality.

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
into queued tasks. Approval applies each accepted patch into the canonical checkout and records
recoverable integration errors.

Future work should define decomposition and integration as first-class policies, not just scheduler behavior:
task contracts, file ownership hints, dependency edges, conflict routing, merge ordering, and how a supervisor
should re-plan when one parallel branch fails.

### Spec closure and project memory

Status: not started.

Completed specs should leave behind compact project memory instead of forcing future agents to read every raw
task and run artifact. The proposed HQ command is `/archive-spec`.

The intended workflow:

- require that the selected spec has no non-terminal tasks and all intended milestones are complete;
- launch the Supervisor in a spec-closure mode that reviews related task descriptions, submissions, reviews,
  integration errors, and check evidence;
- append or update a `## Outcome` section in the spec with concise implementation notes, deviations from the
  original spec, validation evidence, follow-up work, and useful context for future agents;
- move raw task and run artifacts for that spec out of the checkout after user confirmation;
- store archive metadata in SQLite so task/run history remains queryable even after files move.

The archive should default to a machine-local directory tree, not a compressed file:

```text
~/.ferrus/projects/<project-id>/archive/specs/<spec-slug>-<closed-at>/
  manifest.toml
  spec.md
  tasks/
    <task-id>.md
  runs/
    <task-id>/
      SUBMISSION.md
      REVIEW.md
      PATCH.diff
      INTEGRATION_ERROR.md
```

This is portable across Windows, macOS, and Linux, easy to inspect by hand, and avoids depending on platform
archive tools. Compression can be added later as an optional export format, with `.zip` as the most portable
human-facing option if a single file is needed.

By default, raw artifacts should move to `~/.ferrus/projects/<project-id>/archive/...` rather than stay under
the repository's `.ferrus/` directory. The repository should keep the spec and its `## Outcome` memory, while
machine-local runtime history keeps detailed forensic artifacts. A future option can support keeping archives
inside the repository for teams that explicitly want to version task/run history.

## Proposed order

1. Close the Windows support gap: real agent-loop validation and support documentation.
2. Add explicit SQLite schema versioning/migrations and richer runtime event queries.
3. Add `/archive-spec` for spec closure, `## Outcome` project memory, and machine-local task/run archival.
4. Finish the multi-agent integration policy around task graphs, conflicts, and partial failures.
5. Build repository graph and indexed context on top of the SQLite/runtime abstractions.
6. Formalize backend capability metadata for external agents.
7. Design and prototype ferrus-native nano-agents.

## Non-goals for now

- turning this roadmap into a date-driven quarterly plan;
- committing to delivery dates before the core architecture stabilizes;
- adding major product surface area before strengthening the orchestration core;
- replacing useful human-readable task/run artifacts with opaque database-only state.
