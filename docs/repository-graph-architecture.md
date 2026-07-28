# Repository Graph Architecture and Identity Contracts

Status: accepted for Repository Graph Phase 0 (`rg0.0`).

Related specification: [Repository Graph Phase 0](specs/2026-07-10-rg0-contracts-foundation.md).

## Purpose

This document defines the architectural boundary and portable identity contracts for Ferrus repository context.
It is normative for the Phase 0 domain types, storage interfaces, snapshot publication, and query contracts that
follow it.

The repository graph is a rebuildable materialized view of one explicitly identified source state. Source content
or Git remains authoritative. The graph may be missing, stale, incompatible, or deleted without changing Ferrus
task state.

This document deliberately defines logical contracts rather than Rust layouts, SQLite tables, hash algorithms, or
parser choices. Those implementation details belong to later milestones and may change without changing the
semantics below.

## Domain Boundaries

Ferrus has three related but independent domains:

| Domain | Owns | Source of truth | Must not own |
|---|---|---|---|
| `RepositoryGraph` | Source snapshots, files, modules, symbols, documentation/configuration entries, typed relationships, evidence, diagnostics | Git or the selected workspace manifest | Task lifecycle, leases, reviews, historical conclusions |
| `TaskGraph` | Task dependencies, readiness, claims, leases, runs, state transitions, integration outcomes | `ferrus.db` | Code dependency semantics or repository search facts |
| `ProjectMemory` | Specifications, approved outcomes, decisions, deviations, validation evidence, follow-up work | Tracked specs plus authorized archive/runtime metadata | Scheduler state or authoritative current-code relationships |

The domains may reference one another only through typed, versioned references:

- a task or run may pin a repository snapshot or task view;
- project memory may contain an evidence-backed link to a repository path or semantic key;
- a context service may query and rank across domains;
- no domain may copy another domain's state machine into its own storage.

Repository and memory revisions advance independently. A federated response pairs immutable revision identifiers;
it does not create a mutable combined source of truth.

## Architectural Boundary

The dependency direction is:

```text
CLI / HQ / MCP
       |
RepositoryContextService
       |-- QueryService ------> GraphQuery ------> GraphStore
       |
       `-- IndexCoordinator
              |-- RepositorySource
              |-- Extractor registry
              |-- CrossFileResolver
              |-- GraphStore
              `-- EventSink
```

Rules:

- callers depend on domain request/response types, never on SQLite or Git subprocess details;
- extractors emit graph fragments and do not write SQL;
- `GraphStore` owns persistence and publication, not source discovery;
- `RepositorySource` owns filesystem/Git access and path confinement;
- `SnapshotContent` is the only later boundary allowed to return a source snippet for a pinned snapshot;
- `EventSink` records lifecycle metadata without making the existing runtime event table part of graph semantics.

The local implementation may use `repo-graph.db`. A cloud implementation may use different storage and workers.
Both must preserve the same snapshot, evidence, freshness, and bounded-query contracts.

## Repository Scope and Workspace Authority

Repository identity and workspace location are different concepts.

- `LocalProjectId` is the existing opaque machine-local Ferrus project registration. It namespaces local graph
  storage but is not a portable cross-clone repository identity.
- `RepositoryNamespace` identifies the authority that issued a repository identifier. The local authority is the
  registered Ferrus project; a future remote authority is tenant/project scoped.
- `RepositoryId` is opaque inside its namespace. The local backend may represent the one registered repository as
  `root` inside the local project namespace. A cloud service assigns its own repository ID.
- `RepositoryRef` is the portable pair `(namespace, repository_id)`.

Remote URLs, directory names, absolute paths, Git object IDs, and content digests are metadata, not repository
authorization or globally unique identity. Two clones with the same remote are not automatically the same Ferrus
repository, and forks must not collide.

The authoritative source root is resolved explicitly:

- canonical views use the workspace registered in project metadata;
- managed task views use the workspace recorded in runtime task/run context;
- baseline views use the pinned Ferrus Git tree for that task;
- manual non-Git views use an explicit source root supplied to `RepositorySource`;
- process current working directory is never authoritative identity.

Absolute local paths may be used transiently inside source adapters. They must not appear in persisted graph facts,
portable IDs, query cursors, events, or wire responses.

## Portable Repository Paths

`RepoPath` v1 has these rules:

- it is relative to the repository root;
- `/` is the serialized separator on every platform;
- it has no leading slash, drive prefix, NUL, empty component, `.` component, or `..` component;
- original case is preserved and identity is not lowercased;
- lookup normalization is separate from path identity;
- symlink metadata may be represented, but a source adapter must not follow a symlink outside the root;
- paths that cannot be represented as valid UTF-8 are skipped from v1 semantic analysis with a bounded diagnostic.

Skipping a path is an explicit capability limitation. It must not be interpreted as evidence that the file or its
relationships do not exist.

## Identity Model

All portable IDs are opaque values with an explicit type. SQLite row IDs, process IDs, absolute paths, and mutable
`latest` pointers are never portable identities.

### Source revision

`SourceRevision` describes one observed source state before analysis. It contains:

- `repository_ref`;
- `source_kind`, such as committed Git tree, workspace overlay, pinned task baseline, or non-Git manifest;
- optional immutable `base_revision`, such as an algorithm-tagged Git tree ID;
- `manifest_digest` over every included path, file mode, content identity, and effective source-policy version;
- `dirty` and `includes_untracked` indicators;
- the semantic analysis-configuration digest.

The manifest digest is algorithm-tagged. The exact digest algorithm and wire encoding are deferred to `rg0.1`.
Timestamps and absolute source locations do not participate in source identity.

For a clean Git tree, adapters should reuse Git content identities where valid. Dirty, untracked, or non-Git
content receives a content digest without mutating the user's Git index.

### Analysis snapshot

`SnapshotId` identifies one complete structural graph for:

```text
repository_ref
+ source manifest digest
+ graph model version
+ effective semantic analysis-config digest
+ extractor-set digest
```

The extractor-set digest is derived from the canonical set of extractor IDs, versions, and extractor-specific
semantic configuration. The snapshot ID excludes build attempt, timestamps, storage layout, query limits,
retention, endpoints, credentials, and telemetry settings.

A snapshot is immutable after completion. Rebuilding the same logical inputs may reuse the same snapshot identity;
it does not create different graph semantics merely because another build attempt occurred.

### Build attempt

`BuildId` identifies one execution attempt. It is unique rather than content-derived. Multiple build attempts may
target the same prospective snapshot.

Build state and published snapshot state are separate:

- a failed build does not make the last published snapshot disappear;
- partial facts remain invisible to ordinary queries;
- publication compares the expected current pointer before replacing it;
- an older build cannot overwrite a newer published view.

### Graph nodes and edges

`NodeId` and `EdgeId` are opaque and scoped to one snapshot. Implementations should derive them deterministically
from canonical fact identity so repeated builds are stable, but clients must not assume they survive another
snapshot.

`SemanticKey` is a best-effort language- or extractor-defined cross-snapshot lookup key. It is not a database
primary key and carries no stability guarantee across rename, refactor, parser change, or macro expansion.

Every graph fact carries:

- snapshot identity;
- fact kind;
- extractor ID and version;
- repository-relative evidence location when available;
- resolution state: `resolved`, `unresolved`, or `external`;
- confidence or exactness classification;
- optional typed properties.

Absence of a node or edge means "not known by this snapshot and its capabilities," not proof that the entity or
relationship does not exist.

### Task repository view

`TaskRepositoryView` is not a mutable alias to canonical `latest`. An available view contains:

- task/run authorization scope;
- immutable baseline snapshot ID;
- optional overlay revision ID;
- overlay source-manifest digest;
- view lifecycle: mutable task view or frozen submitted view;
- freshness for baseline and overlay separately.

Its explicit state distinguishes baseline-only, baseline-plus-overlay, stale, unavailable, and failed views, so a
caller never falls back to canonical context merely because a task view could not be built.

`WorkspaceRef` carries only repository identity, task-view namespace, and pinned Git tree identity. The local
worktree path belongs to the machine-local source adapter and is never part of a portable view identity.

`WorkspaceOverlayManifest` is the deterministic, policy-aware changed-file input for overlay construction. It
records additions, modifications, deletions, and rename evidence (as delete plus add), while source descriptors and
content identities are present only for paths that remain indexable under the effective source policy.

The overlay revision is derived from the pinned baseline plus the task's included changed, added, and deleted
content under the same source policy. Two tasks never share an overlay namespace merely because their content
matches.

A newer canonical snapshot does not make a pinned task baseline stale. Worktree changes make only that task's
overlay stale. A frozen submitted view is immutable and is reopened by identity for review or recovery.

### Project memory and semantic projections

`MemoryRevisionId` is independent of `SnapshotId` because approved outcomes and archive metadata can change without
source code changing.

Future embeddings are another derived semantic projection. Their revision is keyed by at least structural or memory
revision, embedding model/version, chunking/version, and semantic-search configuration. Changing an embedding model
does not change or rebuild the structural graph snapshot.

## Lifecycle and Freshness

Availability, build execution, publication, and freshness are orthogonal and must not be collapsed into one status.

### Availability

- `not_built`: no compatible published snapshot exists;
- `available`: a compatible published snapshot exists;
- `incompatible`: stored schema/model cannot be read and requires rebuild.

### Build execution

- `building`: an unpublished attempt is running;
- `complete`: an attempt produced a complete candidate or published snapshot;
- `failed`: an attempt stopped with diagnostics;
- `superseded`: a complete attempt lost publication compare-and-set to a newer view.

### Freshness

- `fresh`: the current effective source manifest, graph model, semantic config, and extractor set match the pinned
  snapshot or overlay;
- `stale`: a comparable current input differs;
- `unknown`: the source cannot be inspected sufficiently to compare;
- `not_applicable`: used for an immutable pinned source that no longer has a mutable "current" counterpart.

A status response may therefore report "published snapshot is stale; refresh build failed" without losing the
published snapshot.

Freshness is computed from actual source state, not lifecycle events alone:

1. Resolve repository and view from explicit runtime/project context.
2. Expand the effective semantic configuration and extractor set.
3. Compute or reuse the effective source manifest.
4. Compare its identity inputs with the pinned snapshot or overlay.
5. Return freshness plus diagnostics; do not silently rebuild from a read-only query.

Canonical integration events may trigger invalidation or a best-effort refresh, but an actual manifest comparison
is authoritative. Partial patch application or rollback failure that changes canonical content makes the old
canonical snapshot stale regardless of the reported task outcome.

## Repository Graph Configuration

The optional configuration namespace is `[repository_graph]`. Its absence means default optional behavior; ordinary
Ferrus orchestration remains usable without graph storage.

Reserved configuration areas are:

| Namespace | Purpose | Structural snapshot digest |
|---|---|---|
| `[repository_graph]` | Enablement and backend selection | Operational fields excluded |
| `[repository_graph.source]` | Included content, untracked policy, ordered ignore/sensitive rules, generated/vendor policy | Included |
| `[repository_graph.analyzers]` | Enabled extractors and extractor-specific semantic settings | Included |
| `[repository_graph.index_limits]` | Limits that can cause a file/fact to be included, skipped, or truncated during extraction | Included |
| `[repository_graph.query_limits]` | Result, depth, byte, duration, snippet, and diagnostic budgets | Excluded |
| `[repository_graph.retention]` | Snapshot/build cleanup policy | Excluded |
| `[repository_graph.memory]` | Authorized memory sources and memory extraction policy | Memory revision only |
| `[repository_graph.semantic]` | Future embedding/chunking/model policy | Semantic projection revision only |
| `[repository_graph.remote]` | Endpoint, credentials reference, upload policy, remote operational behavior | Excluded from structural digest |
| `[repository_graph.telemetry]` | Metrics and diagnostic emission | Excluded |

### Effective semantic configuration

Snapshot identity uses a canonical semantic projection, not raw TOML text:

1. Parse the versioned graph configuration schema.
2. Expand every default explicitly.
3. Reject or diagnose unknown graph settings rather than silently classifying them.
4. Normalize enum names and repository-path separators.
5. Deduplicate and sort values declared as sets.
6. Preserve order for rule lists whose order changes meaning, including ignore/negation rules.
7. Serialize with stable field ordering and canonical scalar representation.
8. Hash the structural semantic projection with an algorithm-tagged digest.

Missing configuration and configuration that explicitly states all defaults produce the same effective digest.
Equivalent supported-platform configurations produce the same digest. Operational settings do not.

Credentials, tokens, secret values, absolute credential-file paths, endpoints containing credentials, and telemetry
labels never participate in snapshot identity and never appear in diagnostics or events.

An older Ferrus version encountering an unsupported graph setting may disable graph operations with an actionable
configuration/version error. It must not make the core task runtime unusable solely because the optional graph
capability cannot interpret newer configuration.

## Security and Privacy Invariants

- Repository content is untrusted input; indexing never executes repository code, hooks, compilers, or macros.
- A content digest is identity evidence, not authorization.
- Repository, task overlay, and future tenant scope are checked before object or graph lookup.
- Source adapters confine paths and do not follow symlinks outside the selected root.
- Sensitive/excluded content policy is applied before extraction or future upload.
- Full source bodies are not stored in the structural graph sidecar.
- Source snippets are retrieved later only through `SnapshotContent` after path and content-identity verification.
- Logs, events, metrics, errors, and build diagnostics contain IDs, counts, timings, and bounded error metadata rather
  than source bodies, secrets, or absolute workspace paths.

## Contract Invariants

Later implementation must preserve all of these statements:

1. `ferrus.db` remains authoritative for orchestration; `repo-graph.db` remains derived and deletable.
2. Repository graph, task graph, project memory, and semantic projections have independent revisions.
3. Portable identity contains no absolute path, PID, filesystem lock, SQLite row ID, or mutable latest pointer.
4. Repository paths are normalized, relative, case-preserving, and root-confined.
5. Snapshots and frozen views are immutable; build attempts and publication pointers are separate.
6. Partial or failed builds never replace the last published view.
7. Freshness is computed against actual effective inputs, not inferred only from events.
8. Operational configuration and secrets do not change structural snapshot identity.
9. Missing graph capability never blocks the Supervisor-Executor state machine.
10. Every fact is evidence-backed and capability-scoped; missing facts never prove absence.

## Rejected Alternatives

- **Store graph tables in `ferrus.db`:** couples a large rebuildable data plane to task leases and transitions.
- **Use one mutable current graph:** cannot represent reproducible canonical, task overlay, and review views.
- **Use the remote URL as repository identity:** aliases forks and clones and fails for repositories without remotes.
- **Use absolute paths or CWD as identity:** breaks worktrees, relocation, Windows portability, and cloud execution.
- **Promise stable node IDs across snapshots:** rename, parser, and semantic changes make that contract unreliable.
- **Merge task, code, and memory edges into one graph:** confuses authority, lifecycle, and relationship meaning.
- **Hash raw TOML:** makes whitespace, ordering, explicit defaults, endpoints, and secrets affect semantic identity.
- **Put embeddings into the structural snapshot identity:** forces structural rebuilds when only the model changes.

## Deferred to Later Milestones

- `rg0.1`: concrete Rust types, serialization formats, graph vocabulary, and digest implementation;
- `rg0.2`: SQLite schema, migration runner, lazy sidecar lifecycle, and incompatible-version behavior;
- `rg0.3`: build persistence, atomic publication, compare-and-set, and supersession;
- `rg0.4`: bounded query, pagination, error, cursor, and `SnapshotContent` request/response contracts;
- `rg0.5`: contract fixtures, lifecycle diagnostics, cross-platform tests, and architecture enforcement.
