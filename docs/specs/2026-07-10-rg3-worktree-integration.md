# Repository Graph Phase 3: Task Worktree and Orchestration Integration Specification

## Summary

This phase makes repository context consistent with Ferrus task execution. Each task or run is pinned to an
immutable baseline repository snapshot, and an Executor working in a managed Git worktree receives a derived view
that overlays changed, added, and deleted files on that baseline. Canonical integration invalidates or refreshes
the canonical graph only after successful approval and integration checks, without making repository indexing a
correctness dependency of the orchestration state machine.

## Goals

- Persist a reproducible repository snapshot reference for task and run context.
- Reuse Ferrus pinned baseline Git trees as authoritative task-view inputs.
- Compose a task view from a baseline snapshot plus a bounded changed-file overlay.
- Keep concurrent tasks isolated even when the canonical graph advances.
- Mark canonical context stale after successful integration and refresh it asynchronously or explicitly.
- Preserve successful task approval when graph refresh is unavailable or fails.
- Retain snapshots referenced by active tasks and garbage-collect unreferenced derived views safely.
- Surface task-view freshness, overlay support, and failures through status, CLI, MCP, and diagnostics.

## Non-Goals

- Replacing Git worktrees or changing Ferrus patch integration policy.
- Using the repository graph as a scheduler, lease store, or task dependency graph.
- Blocking task dispatch, checks, submission, review, or approval on index availability.
- Providing real-time file watching or re-indexing on every editor keystroke.
- Sharing overlays between unrelated tasks or repositories.
- Solving arbitrary merge conflicts, ownership assignment, or multi-task integration ordering.
- Implementing remote workers, cloud storage, or cross-machine snapshot distribution.
- Adding new language semantics beyond the Phase 1 extractors.

## Context

Phases 0 through 2 must be complete. This phase assumes immutable local graph snapshots, canonical workspace
indexing, read-only CLI/MCP retrieval, freshness reporting, bounded queries, safe snippets, and evaluation coverage.

Ferrus already creates managed Executor worktrees and captures each task's starting content as a Git tree pinned
under a Ferrus baseline ref. The Executor may then modify, add, or delete files while the canonical checkout and
other task worktrees continue independently. A graph tied only to mutable canonical `latest` is therefore not a
reproducible task context.

The authoritative association between a running task and its repository view is orchestration metadata, not a
guess based on current working directory. Persisting that association in `ferrus.db` requires explicit runtime
schema migrations; the current `CREATE IF NOT EXISTS` and `ensure_column` evolution is insufficient for a new
cross-process contract.

## Requirements

- Introduce explicit, ordered, transactional runtime database migrations before adding repository snapshot
  references to task or run records.
- Existing project databases must adopt the migration baseline without losing tasks, runs, events, leases,
  counters, selection state, or archives.
- Persist the repository view identity needed to reproduce a task/run context across process restarts. The record
  must distinguish baseline snapshot, optional overlay revision, and current freshness.
- Do not use absolute worktree paths, PIDs, or local graph row IDs as portable snapshot identity.
- At task dispatch, resolve the existing pinned Ferrus baseline tree and find or build a matching repository graph
  snapshot when practical.
- Graph absence or build failure at dispatch must not block the task. Persist and expose `not_built` or `stale`
  context so the agent can fall back to direct repository inspection.
- A task/run must continue querying its pinned baseline snapshot even after a newer canonical snapshot is
  published.
- Define `WorkspaceRef` and `TaskRepositoryView` contracts before implementing overlays. The API must support
  baseline-only, baseline-plus-overlay, unavailable, stale, and failed states.
- Compute task overlay manifests relative to the pinned baseline tree, including:
  - modified tracked files;
  - added or non-ignored untracked files;
  - deleted files;
  - renamed paths represented as delete plus add unless stronger identity evidence exists;
  - effective index and sensitive-path policy.
- Reuse existing baseline tree and patch inventory behavior through a shared Git/source abstraction rather than
  duplicating more Git subprocess logic in HQ and MCP tools.
- Analyze only changed overlay files and path-context dependents. Reuse immutable baseline facts for unchanged
  content.
- Overlay deletion must hide baseline nodes, edges, snippets, and search hits for the deleted path.
- Overlay changes must replace baseline facts for the changed path and re-resolve affected cross-file relationships.
- Overlay queries must return both baseline snapshot ID and overlay revision/fingerprint.
- Snippet access must prefer verified overlay content for changed files and verified baseline content for unchanged
  files; it must never silently read a different canonical revision.
- Overlay refresh may be explicit or lazily triggered by a bounded query, but it must never start an unbounded
  repository rebuild from an agent tool.
- Concurrent overlay refreshes for one task must be serialized or deduplicated through idempotent build identity.
- Different tasks must never publish into or query another task's overlay namespace.
- Resolve repository views from authoritative runtime identity:
  - taskless Supervisor and manual sessions use the published canonical view;
  - an Executor uses its mutable task overlay;
  - a Consultant attached to a task uses that task's current view;
  - a Reviewer uses the frozen submitted view for the exact task;
  - an invalid task binding returns an explicit error rather than silently falling back to canonical context.
- When repository indexing is available, submission must best-effort refresh and atomically freeze the current task
  view before review handoff. Failure to freeze must be recorded explicitly but must not block an otherwise valid
  submission or alter the task lifecycle.
- Reviewer and recovery flows must reopen a successfully frozen submitted view after the Executor exits or its
  managed worktree is removed.
- Rejection must retain the task worktree and resume a mutable successor view derived from the rejected submitted
  view or its baseline without exposing reviewer-only state to another task.
- After approval successfully applies a patch and post-approve checks pass, record a canonical graph invalidation
  with the integrated source revision or manifest information.
- Canonical freshness must ultimately be derived from the actual post-operation source manifest, not only from the
  reported approval outcome. Capture or compare the manifest around integration and mark the graph stale whenever
  canonical content changed.
- Do not synchronously run a full canonical rebuild inside the approval transaction or filesystem integration lock.
- A best-effort incremental refresh may be scheduled after successful approval. Its failure must leave approval
  complete, preserve the last published canonical graph, and expose stale status and diagnostics.
- A rejected submission or failed integration whose rollback restores the original manifest must not record the
  proposed patch as integrated. If patch application or rollback leaves any partial canonical change, mark the
  canonical graph stale and rebuild from the actual resulting content rather than the proposed patch.
- External canonical edits must be detected by normal source-manifest freshness checks even when no Ferrus event
  exists.
- Retention must preserve snapshots and overlay fragments referenced by active tasks/runs or live queries.
- Garbage collection must remove only unreferenced superseded snapshots, failed-build fragments past retention,
  and completed-task overlays past the configured retention window.
- Removing a managed task worktree must not remove a still-referenced immutable baseline snapshot.
- Status and MCP responses must identify canonical versus task view, baseline snapshot, overlay revision,
  freshness, truncation, and fallback behavior.
- Index/view failures must never mutate task status, lease ownership, retries, review cycles, or failure state.
- Emit lifecycle metrics and events using project/task/run/build/snapshot IDs without source content or absolute
  worktree paths.

## Milestones

- [x] #3.0 Add explicit runtime schema migrations for repository view references

ID: rg3.0
Depends on: none

Introduce versioned `ferrus.db` migrations, adopt existing databases safely, and persist optional baseline
snapshot, overlay revision, and repository-view status for tasks or runs without changing existing lifecycle
semantics.

Implemented in `src/project.rs` with ordered transactional runtime migrations, durable migration history and
schema version validation, plus typed task/run repository-view persistence. Existing rows are adopted as
`not_built` without changing task or run lifecycle fields.

- [x] #3.1 Pin task and run context to baseline repository snapshots

ID: rg3.1
Depends on: rg3.0

Resolve Ferrus baseline Git trees during dispatch, find or build matching graph snapshots best-effort, persist the
association, pass it through runtime context, and keep task queries pinned when canonical `latest` advances.

Implemented across `src/hq/mod.rs`, `src/project.rs`, `src/repository_graph/source/mod.rs`, and
`src/repository_graph_runtime.rs`: managed-worktree dispatch schedules a non-blocking, best-effort baseline build
against the pinned Git tree; task and run records retain the resulting snapshot association; runtime retrieval
selects that immutable snapshot directly even after canonical publication advances. Missing, changed, or failed
baseline sources remain explicit graph-view states and do not alter task lifecycle state.

- [x] #3.2 Implement task worktree overlay manifests and invalidation

ID: rg3.2
Depends on: rg3.1

Create shared Git/source primitives that compute changed, added, deleted, renamed, policy, and content-identity
information relative to the pinned baseline without duplicating task patch logic.

Implemented in `src/repository_graph/source/worktree.rs` with portable `WorkspaceRef` and validated
`TaskRepositoryView` contracts, a read-only baseline-relative Git inventory, deterministic task-scoped overlay
revisions, effective-policy source descriptors, hash-verified changed-file reads, and revalidation. HQ baseline
capture, graph baseline validation, and submission patch inventory now share these primitives without mutating the
Executor's real Git index. Overlay graph composition and query routing remain scoped to rg3.3.

- [ ] #3.3 Compose baseline and overlay graph views

ID: rg3.3
Depends on: rg3.2

Analyze changed fragments, hide deleted baseline facts, re-resolve affected edges, provide task-scoped search,
context, and verified snippets, and return explicit baseline and overlay identities.

- [ ] #3.4 Freeze submitted views and route role-specific retrieval

ID: rg3.4
Depends on: rg3.3

Resolve taskless, Executor, Consultant, and Reviewer views from runtime identity; best-effort freeze submitted
views; reopen them for review/recovery; and resume a mutable successor after rejection without weakening submit.

- [ ] #3.5 Integrate manifest-driven canonical invalidation with approval

ID: rg3.5
Depends on: rg3.1

Compare actual canonical manifests around integration, record successful integrated revisions, handle partial or
rollback-failed mutations honestly, schedule refresh outside the approval critical section, and keep graph refresh
outcomes separate from task approval outcomes.

- [ ] #3.6 Add concurrent-view isolation, retention, recovery, and observability

ID: rg3.6
Depends on: rg3.4, rg3.5

Protect task namespaces, deduplicate refreshes, retain referenced and frozen snapshots, garbage-collect safe
candidates, recover interrupted overlay builds, expose canonical/task-view status and privacy-safe metrics, and add
multi-worktree fixtures, restart/failure tests, lifecycle documentation, and pinned-versus-canonical evaluations.

## Acceptance Criteria

- Existing `ferrus.db` fixtures migrate without loss or semantic changes, and repeated migrations are idempotent.
- Every managed task/run can report an explicit baseline snapshot or an actionable unavailable/stale state after
  process restart.
- Publishing a newer canonical snapshot does not change repository results for an already pinned task view.
- Two concurrent tasks based on different baselines or overlays cannot read or mutate each other's graph view.
- Modified and added worktree files replace or extend baseline facts only inside their task view.
- Deleted worktree files produce no active task-view nodes, edges, snippets, or search hits from the baseline.
- Rename fixtures do not leave both old and new paths active unless both actually exist.
- Overlay refresh invokes extractors only for changed files and declared path-context dependents.
- Task-view responses identify baseline snapshot, overlay revision, freshness, evidence, and truncation.
- Verified snippets never fall through to unrelated mutable canonical content.
- A Reviewer reopens the exact successfully frozen submitted view after the Executor exits and after recovery.
- Task-scoped retrieval with an invalid binding fails explicitly instead of returning unrelated canonical context.
- A forced submitted-view freeze failure remains visible but does not prevent an otherwise valid submission from
  entering review.
- Missing or failed repository indexing never prevents task dispatch, check, submit, review, reject, approve, reset,
  recovery, or lease renewal.
- Successful approval remains complete when forced graph invalidation or refresh fails; status and doctor expose the
  stale canonical graph.
- Rejected, integration-failed, or successfully rolled-back patches do not mark their proposed content as
  integrated when the canonical manifest is unchanged.
- Partial patch application or rollback failure that changes canonical content marks the graph stale and refreshes
  from the actual resulting manifest rather than reporting the old snapshot as fresh.
- Canonical refresh runs outside the approval transaction and filesystem integration lock.
- Active task snapshots survive ordinary garbage collection and managed worktree removal.
- Completed task overlays become collectible according to documented retention without removing shared baselines.
- Recovery handles an interrupted overlay build without exposing partial results or changing task state.
- Tests cover database upgrade, task restart, different baselines, concurrent overlays, add/change/delete/rename,
  submission freeze, review/rejection routing, snippet verification, approval success/failure, rollback failure,
  partial canonical mutation, external edits, retention, and cross-task isolation.
- `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test` pass.

## Risks and Open Questions

- Introducing explicit migrations for the existing runtime database is broader than repository graph alone but is
  necessary before snapshot pinning becomes authoritative orchestration metadata.
- Building a missing baseline snapshot at dispatch may be too expensive; orchestration must prefer graceful
  fallback or background work over blocking agent startup.
- Overlay cross-file resolution may approach the cost of full resolution when central manifests or module roots
  change.
- External agent backends may start MCP servers with different current directories; persisted task/run identity
  must remain authoritative.
- Git submodules, sparse checkouts, unborn repositories, and non-Git workspaces need explicit overlay capability
  reporting.
- Retention must balance reproducible historical task context against local disk growth.
- It remains open whether task overlay refresh should be explicit, query-triggered, periodically scheduled, or a
  combination with strict cost caps.
