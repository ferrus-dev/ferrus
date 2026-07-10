# Repository Graph Phase 0: Contracts and Storage Foundation Specification

## Summary

This phase establishes the durable architectural contracts for Ferrus repository indexing before any
language-specific indexing or agent retrieval is implemented. It defines repository and snapshot identity,
the evidence-backed graph model, freshness semantics, backend boundaries, and a versioned machine-local
SQLite sidecar that can publish immutable graph snapshots atomically.

The repository graph is a rebuildable materialized view of source content. It is not the source of truth for
orchestration, and it must remain separate from the runtime task graph and project memory.

## Goals

- Establish a dedicated `repository_graph` bounded context outside `src/project.rs`.
- Define stable v1 contracts for repository sources, snapshots, nodes, edges, evidence, diagnostics, and queries.
- Model canonical, dirty, and task-worktree source views without assuming that one mutable checkout is authoritative.
- Store the local derived index in a separate, versioned `repo-graph.db` beside `ferrus.db`.
- Support immutable snapshot builds and atomic publication without exposing partial results.
- Define a versioned, deterministic `[repository_graph]` configuration contract before configuration affects
  snapshot identity.
- Keep public domain and wire contracts independent of SQLite, absolute paths, PIDs, and local filesystem locks.
- Make incompatible derived-index changes recoverable through an explicit rebuild.

## Non-Goals

- Discovering or parsing repository files.
- Implementing Rust, Cargo, Markdown, or configuration extractors.
- Providing symbol search, context retrieval, MCP tools, or prompt integration.
- Supporting Executor worktree overlays.
- Implementing a distributed indexer or selecting a cloud database.
- Adding embeddings, vector search, generated summaries, or deep graph algorithms.
- Replacing `ferrus.db` as the source of truth for orchestration runtime state.

## Context

Ferrus already has a machine-local project registry, canonical workspace metadata, SQLite runtime state,
runtime events, managed Executor worktrees, and pinned baseline Git trees. However, the current runtime schema
is initialized directly in `src/project.rs`, and no reusable repository index, graph-domain model, or query API
exists.

Repository graph data has different lifecycle and consistency requirements from task coordination data:

- source content or Git remains authoritative;
- graph data may be deleted and rebuilt;
- indexing may be write-heavy while task leases and state transitions must stay responsive;
- incompatible semantic changes may require a rebuild rather than an in-place migration;
- cloud storage must eventually replace local SQLite without changing graph semantics.

This specification is the prerequisite for every later repository graph phase. Cross-spec dependencies are
documented in prose because Ferrus milestone readiness currently resolves dependencies only within one selected
specification.

## Requirements

- Add a dedicated module boundary under `src/repository_graph/`; graph persistence and query SQL must not be
  added to `src/project.rs`.
- Define distinct identities for local Ferrus projects, repositories, source revisions, analysis snapshots,
  task views, graph nodes, and graph builds.
- Persist only repository-relative paths in graph records and public DTOs. Platform paths may exist only at
  source-adapter boundaries.
- Treat snapshots as immutable. Build states must distinguish at least `building`, `published`, `failed`, and
  `superseded`.
- A published snapshot must identify its source manifest, graph model version, index configuration digest,
  and extractor-set digest.
- Define an optional `[repository_graph]` configuration namespace with deterministic defaults and reserved
  extension tables for source policy, analyzers, limits, retention, memory, and remote adapters.
- Normalize the effective configuration before hashing: expand defaults, normalize repository-relative patterns,
  sort set-like values, and exclude platform spelling differences.
- Classify configuration as semantic or operational. Only settings that can change discovered content or graph
  facts participate in the snapshot's analysis-config digest; credentials, endpoints, retention, telemetry, and
  other operational values must not.
- Missing configuration and configuration that explicitly states every default must produce the same effective
  semantic digest. Secrets and credentials must never be included in a digest, diagnostic, or event.
- Node and edge facts must support source location, extractor provenance, resolution state, confidence, and
  typed properties. Absence of an edge must not imply proof that no relationship exists.
- Keep `RepositoryGraph`, orchestration `TaskGraph`, and `ProjectMemory` as separate domains. Later federation
  may add typed cross-links without merging their state machines.
- Define ports equivalent to `RepositorySource`, `Extractor`, `CrossFileResolver`, `GraphStore`, `GraphQuery`,
  `SnapshotContent`, and `EventSink` without leaking backend-specific types.
- Store the local graph at `~/.ferrus/projects/<project-id>/repo-graph.db` as a deletable derived sidecar.
- Create the sidecar lazily only when an explicit graph build/index operation first needs it. `ferrus init`,
  `ferrus doctor`, and read-only graph status inspection must succeed when it is absent and must not create it.
- Give the sidecar an explicit schema version and ordered migrations from its first revision.
- The initial sidecar schema must represent schema metadata, index builds, snapshots, published views, files,
  nodes, edges, and diagnostics.
- Snapshot publication must be an atomic compare-and-set operation. A failed or interrupted build must leave
  the previously published snapshot readable.
- Version database schema, graph model, extractor contracts, and query wire format independently.
- Do not persist full source bodies in the graph sidecar. Store content identities, metadata, signatures, and
  spans; source snippets are a later on-demand concern.
- Define bounded query request types even though query execution is deferred. Requests must carry explicit
  limits rather than permitting unbounded traversal.
- Record graph lifecycle diagnostics without source content or secret values.

## Milestones

- [ ] #0.0 Define repository graph architecture and identity contracts

ID: rg0.0
Depends on: none

Document the separation between repository graph, task graph, and project memory; define repository, source
revision, snapshot, build, node, and task-view identities; specify canonical, dirty, and worktree freshness; and
define normalized semantic versus operational repository-graph configuration.

- [ ] #0.1 Introduce repository graph domain types and module boundaries

ID: rg0.1
Depends on: rg0.0

Create the dedicated module layout and backend-neutral domain types for nodes, edges, evidence, diagnostics,
freshness, snapshots, builds, and bounded query requests. Persisted paths must be repository-relative.

- [ ] #0.2 Add the versioned repository graph SQLite sidecar

ID: rg0.2
Depends on: rg0.0

Resolve `repo-graph.db` through the existing project registry, add an explicit migration runner and initial
schema, and report incompatible versions as `requires_rebuild` without modifying `ferrus.db`.

- [ ] #0.3 Implement immutable snapshot build and atomic publication primitives

ID: rg0.3
Depends on: rg0.1, rg0.2

Implement `GraphStore` lifecycle operations for starting, failing, completing, publishing, reading, and
superseding snapshots. Partial builds must remain invisible to ordinary queries.

- [ ] #0.4 Define backend-neutral query and content-access contracts

ID: rg0.4
Depends on: rg0.1

Define versioned request, response, pagination, truncation, error, and freshness envelopes for status, search,
neighborhood, and context operations, plus a hash-verifying `SnapshotContent` boundary for later snippets.

- [ ] #0.5 Add graph lifecycle diagnostics and foundation contract tests

ID: rg0.5
Depends on: rg0.3, rg0.4

Add sidecar health inspection, migration fixtures, atomic-publication failure tests, deterministic serialization
tests, and tracing/event adapters that never include source bodies.

## Acceptance Criteria

- Repository graph code lives behind a dedicated module boundary and does not enlarge `src/project.rs` with
  graph storage or query logic.
- `repo-graph.db` is created lazily in the registered machine-local project directory when indexing first needs it
  and can be deleted without damaging Ferrus runtime state.
- `ferrus init`, `ferrus doctor`, and graph status inspection succeed with no sidecar, report the optional index as
  absent/not built, and do not create graph storage.
- Missing configuration and explicitly expanded defaults produce the same semantic analysis-config digest across
  supported platforms; reordered set-like values do not change it.
- Operational settings and secrets do not change the analysis-config digest or appear in graph diagnostics.
- A database created at every supported sidecar schema version upgrades deterministically, while an unsupported
  version produces an actionable `requires_rebuild` result.
- Failed and interrupted builds never replace or mutate the last published snapshot.
- Snapshot publication is atomic and rejects an older build attempting to overwrite a newer published view.
- Public DTOs contain no `rusqlite` types, absolute workspace paths, PIDs, or filesystem-lock concepts.
- Nodes and edges carry snapshot identity, provenance, resolution state, confidence, and optional source spans.
- Graph, task, and memory domains remain explicitly distinct in types and documentation.
- Query contracts enforce result, byte, depth, and duration budgets even before their execution is implemented.
- Unit and integration tests cover migrations, incompatible versions, snapshot lifecycle, atomic publication,
  path normalization, and serialization.
- `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test` pass.

## Risks and Open Questions

- Stable semantic identities across renames cannot be guaranteed in v1; opaque node IDs should remain scoped to
  one snapshot, with best-effort semantic keys treated separately.
- Non-UTF-8 repository paths need an explicit encoding or skip-with-diagnostic policy before the wire format is
  frozen.
- Configuration extensions must preserve deterministic normalization; an unclassified new setting must not be
  added silently to snapshot identity.
- The first schema may over-normalize or under-normalize graph properties; backend-neutral contracts must allow
  the physical SQLite layout to change through rebuilds.
- Runtime `ferrus.db` still needs its own explicit schema migration strategy before later phases persist snapshot
  pins in orchestration records.
- Atomic publication and reader concurrency need cross-platform tests, especially on Windows.
