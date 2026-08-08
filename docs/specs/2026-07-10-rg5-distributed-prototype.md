# Repository Graph Phase 5: Distributed Indexing and Cloud Prototype Specification

## Summary

This phase proves that Ferrus repository graph and project memory contracts can cross process and storage
boundaries. It packages authorized repository state as immutable source snapshots, executes durable idempotent
index jobs with stateless workers, independently publishes remote graph snapshots and project-memory revisions,
and serves explicitly pinned or federated queries under tenant authorization.

The prototype must preserve Ferrus's local-first behavior. Local SQLite remains the default and must continue to
work with no network, credentials, cloud SDK initialization, or remote control plane.

## Goals

- Reuse the shipped local graph, memory, freshness, and query semantics across remote adapters.
- Package repository input as immutable, content-addressed, policy-filtered snapshots.
- Execute indexing through durable, leased, at-least-once jobs with idempotent effects.
- Run extraction in stateless, resource-bounded workers that never execute repository code.
- Publish immutable graph snapshots and memory revisions independently with atomic pointers so partial output is
  never queryable and one domain cannot advance the other implicitly.
- Provide versioned control and query APIs scoped by tenant, project, repository, and snapshot.
- Enforce tenant isolation, least privilege, retention, deletion, and privacy-safe observability.
- Run one behavioral contract suite against local SQLite and remote prototype implementations.
- Remain vendor-neutral and avoid requiring a dedicated graph database before workloads justify one.

## Non-Goals

- Replacing, deprecating, or weakening local SQLite indexing.
- Making cloud connectivity mandatory for Ferrus HQ, CLI, agents, or task execution.
- Delivering a production-ready hosted SaaS, billing system, or organization administration product.
- Providing production multi-region availability, disaster recovery, or formal uptime guarantees.
- Selecting a permanent cloud vendor, queue, object store, search engine, or graph database.
- Sharing source content or derived fragments across tenants.
- Executing repository code, build scripts, package hooks, compilers, proc macros, or arbitrary network calls.
- Supporting arbitrary live filesystem streaming or every repository synchronization mechanism.
- Uploading dirty workspace or task-worktree overlays; the prototype accepts canonical repository snapshots and
  authorized project-memory sources only.
- Providing unbounded deep traversal, remote SQL, or arbitrary graph query languages.
- Solving cross-repository dependency federation in the initial prototype.

## Context

Phases 0 through 4 must be complete. This phase assumes immutable repository snapshots, deterministic extractor
and graph versions, local incremental indexing, atomic publication, bounded retrieval, task snapshot pinning and
worktree overlays, project memory federation, versioned wire DTOs, and adapter contract tests.

Ferrus runtime coordination remains separate. `ferrus.db` continues to own tasks, runs, events, leases, and
scheduler state. The cloud prototype is an optional derived context data plane and must never become a hidden
dependency of the local Supervisor-Executor state machine.

A distributed build can be duplicated, delayed, retried, cancelled, or executed by multiple workers. Exactly-once
execution is not assumed. Correctness depends on immutable input, deterministic identities, idempotent writes,
leases, bounded attempts, and compare-and-set publication.

Repository source and project memory are sensitive tenant data. A content digest identifies bytes but grants no
authorization. Identical content in different tenants must remain separately scoped, authorized, retained, and
deletable.

## Requirements

- Local Ferrus behavior must remain unchanged when remote repository graph configuration is absent or disabled.
- Remote indexing must be explicitly enabled and fail closed when endpoint, credentials, tenant, project, or
  repository scope is missing.
- Ferrus must never silently upload content or switch to remote indexing after a local or remote error.
- Remote task overlays remain local in this prototype. A task may pin a remote canonical baseline, but changed
  worktree content must not be uploaded or retained remotely under Phase 5.
- Define serializable versioned identities for tenant, project, repository, repository snapshot, memory revision,
  index job, graph build, extractor set, and index configuration.
- Distinguish local Ferrus project identity from cloud tenant/project/repository identity.
- Every remote request and persistent record must carry explicit tenant and project scope.
- Define separate `repository_graph` and `project_memory` job kinds and idempotency keys. Each key must include
  tenant, project/repository, job kind, the relevant immutable source or memory manifest, effective semantic
  configuration, model version, and extractor-set digest.
- Repeated submission of one logical build must converge on the same completed result or safely resume its job.
- Package source as an immutable manifest containing repository-relative path, content identity, file role and
  metadata, tenant-scoped object reference, and exclusion/redaction policy version.
- Package project memory through a separate immutable manifest that contains only Phase 4 authorized source
  locators, fingerprints, evidence metadata, and tenant-scoped object references. Raw task/run artifact categories
  excluded locally must remain excluded before upload.
- Apply include, ignore, sensitive-path, binary, generated/vendor, size, and symlink policies before upload.
- Use authenticated encryption in transit. Remote object and graph stores must support encryption at rest or mark
  the prototype limitation explicitly.
- Scope object keys, graph rows, search records, caches, queues, and derived fragments by tenant even when content
  digests match. Disable cross-tenant deduplication.
- Define durable job states including `queued`, `leased`, `running`, `publishing`, `complete`, `failed`, and
  `cancelled`.
- Job claiming must use leases and heartbeats with bounded retries, attempts, timeout, and cancellation behavior.
- Worker loss or lease expiry must make work safely reclaimable. Cancellation must prevent future publication.
- Workers must be stateless with respect to durable progress and consume only versioned job and source contracts.
- Treat repository content as untrusted input. Enforce file/snapshot size, parser time/memory, concurrency,
  diagnostic, and output limits; prohibit repository code execution and unrestricted outbound network access.
- Workers must emit idempotent versioned fact batches with job/build/snapshot identity, sequence or shard identity,
  nodes, edges, diagnostics, provenance, and extractor versions.
- Partial fact batches may be durable for retry but must remain invisible to ordinary queries until publication.
- Publication must create immutable graph snapshots and memory revisions behind separate compare-and-set pointers.
  An older slower job must not replace a newer published value in either domain, and publishing one domain must not
  mutate the other domain's pointer.
- Define a `FederatedViewRef` that pairs one immutable graph snapshot ID with one immutable memory revision ID
  without creating a third mutable source of truth.
- Repository-only or memory-only queries must name an immutable revision or resolve that domain's `latest` once at
  request start. Federated queries must name a `FederatedViewRef` or independently resolve both pointers once at
  request start and return the resolved pair.
- Task/run context must use explicit snapshot identities and must not follow mutable remote `latest` after dispatch.
- Remote adapters must implement the existing graph, memory, source-content, and query interfaces without leaking
  queue, object-store, database, or search-engine types into domain APIs.
- A relational adjacency store and separate bounded search index are acceptable. A graph database must not be a
  required dependency of the prototype.
- Provide versioned control operations for build submission, build inspection, cancellation, snapshot inspection,
  and project/repository deletion.
- Provide authenticated bounded query operations equivalent to supported local status, search, neighborhood,
  context, safe snippet, and federated memory retrieval.
- Enforce server-side depth, results, bytes, duration, pagination, snippet, and diagnostic caps independently from
  client requests.
- Authorization must distinguish snapshot upload, build submission, job inspection/cancellation, graph query,
  project deletion, and administrative diagnostics.
- Agent credentials must be query-only and scoped to the current tenant/project/repository where practical.
- Perform authorization before object lookup or existence disclosure.
- Test tenant isolation across IDs, objects, queues, graph rows, search indexes, caches, logs, metrics, and errors.
- Define independent retention for uploaded source, unpublished graph/memory fragments, published graph snapshots,
  memory revisions, query caches, and audit records. Remote task overlays are outside Phase 5 scope.
- Deletion must be idempotent, auditable, and remove covered tenant/project source and derived data without
  affecting another tenant.
- Logs, metrics, traces, and audit records must exclude source bodies, raw memory text, secrets, absolute local
  paths, query text where sensitive, and reusable credentials.
- Provide a shared adapter contract suite for local SQLite and remote implementations, including equivalent graph
  semantics, provenance, freshness, truncation, errors, and publication races.
- Protocol or storage incompatibility must return an explicit version error rather than silently producing graph
  data under different semantics.
- Cloud-specific code must remain behind features/adapters so local builds need no cloud SDK initialization or
  network availability.

## Milestones

- [x] #5.0 Define distributed identities, protocols, consistency, tenancy, and threat model

ID: rg5.0
Depends on: none

Specify versioned control/query/fact contracts, tenant and repository identity, idempotency, job state machine,
publication consistency, authorization matrix, data classification, retention, deletion, and worker threat model.

Implemented by the vendor-neutral contracts under `src/distributed/` and the normative consistency, tenancy,
data-lifecycle, and threat-model decisions in `docs/distributed-indexing-architecture.md`. This milestone adds no
remote backend, network dependency, cloud SDK, or change to the local SQLite execution path.

- [x] #5.1 Implement privacy-filtered repository and memory packaging and source storage

ID: rg5.1
Depends on: rg5.0

Package repository and authorized memory sources as separate manifests after local policy enforcement, upload them
to tenant-scoped immutable object storage, and verify manifests, encryption, quotas, and idempotent reuse.

Implemented by `src/distributed/source.rs` and `src/distributed/object_store.rs`. Repository packaging accepts only
locally filtered canonical manifests, project-memory packaging uploads only sanitized Phase 4 categories, and both
store their immutable manifest body and content objects under tenant/project scope. The durable prototype adapter
uses authenticated encryption at rest, digest verification, atomic object writes, quotas, and idempotent reuse.

- [x] #5.2 Implement durable idempotent index-job coordination

ID: rg5.2
Depends on: rg5.0

Add build submission, idempotency, durable states, leases, heartbeats, bounded attempts, retry, cancellation,
reclaim, and privacy-safe job inspection through a vendor-neutral coordinator interface.

Implemented by the `IndexJobCoordinator` port in `src/distributed/coordinator.rs` and its separately versioned
SQLite prototype adapter in `src/distributed/coordinator_sqlite.rs`. Concurrent duplicate submissions converge,
claims and transitions require renewable generation leases, retry and reclaim respect bounded attempts,
cancellation revokes publication authority, and inspection exposes typed metadata without free-form diagnostics.

- [ ] #5.3 Implement stateless graph and memory extraction workers and fact batches

ID: rg5.3
Depends on: rg5.1, rg5.2

Run existing graph and memory extractors in isolated resource-bounded workers, consume the matching immutable
manifest kind, emit idempotent versioned fact batches, persist retry progress, and prohibit repository code
execution and unrestricted egress.

- [ ] #5.4 Implement remote graph and memory storage with independent publication

ID: rg5.4
Depends on: rg5.3

Ingest graph and memory fact batches into tenant-scoped remote storage, keep partial builds invisible, publish graph
snapshots and memory revisions through independent compare-and-set pointers, and compose explicit federated refs.

- [ ] #5.5 Expose authenticated control and snapshot-pinned query APIs

ID: rg5.5
Depends on: rg5.0, rg5.4

Provide least-privilege build control and bounded graph/memory query services, query-only agent credentials,
authorization-before-disclosure, explicit protocol versions, and local behavior that remains opt-in and offline.

- [ ] #5.6 Validate recovery, deletion, observability, tenant isolation, and adapter parity

ID: rg5.6
Depends on: rg5.2, rg5.4, rg5.5

Exercise duplication, worker loss, lease expiry, cancellation, timeouts, publication races, network partitions,
tenant attacks, retention/deletion, privacy-safe telemetry, and the shared local/remote contract suite.

## Acceptance Criteria

- A project with no remote configuration performs no cloud request and retains the complete local SQLite workflow.
- Remote indexing requires an explicit endpoint, tenant/project/repository scope, and authorized credentials.
- Packaging identical authorized content under the same policy produces the same immutable manifest identity.
- Sensitive, ignored, binary, generated/vendor, external-symlink, and oversized content follows policy before any
  upload and cannot leak through diagnostics.
- Repeated submission of a graph or memory idempotency key creates no duplicate published revision and returns or
  resumes the same logical job.
- A worker crash after partial extraction can be retried elsewhere without corrupting or duplicating the published
  snapshot.
- Expired leases can be reclaimed, while an active lease cannot be stolen; cancellation prevents publication.
- Partial, failed, cancelled, and incompatible builds are not visible through ordinary graph or memory queries.
- Concurrent old and new builds cannot cause the older build to overwrite the newer graph or memory pointer.
- Publishing a graph snapshot does not advance project memory, and publishing memory does not advance the graph.
- Snapshot-, memory-, and federated-view-pinned queries remain internally consistent while either latest pointer
  advances.
- Remote responses preserve local graph/memory semantics, provenance, freshness, truncation, diagnostics, and
  versioned errors.
- The shared contract suite passes against local SQLite and the remote prototype adapters.
- Tenant A cannot discover, query, cancel, delete, or infer the existence of Tenant B's projects, jobs, snapshots,
  objects, graph rows, search records, caches, or memory revisions.
- Identical content in two tenants remains separately authorized, retained, and deletable.
- Query-only agent credentials cannot submit builds, change policy, inspect administrative diagnostics, or delete
  projects.
- Project deletion is idempotent, audited, and removes source and derived data covered by policy without affecting
  another tenant.
- Logs, traces, metrics, audits, and errors contain no source bodies, raw memory, secrets, absolute paths, or
  reusable credentials.
- Simulated queue duplication, worker loss, object-store timeout, graph-store timeout, publication race, and
  network partition have deterministic documented outcomes.
- A remote outage neither corrupts local index state nor changes Ferrus task/runtime state.
- Required repository checks pass for local-only and remote-prototype feature configurations.

## Risks and Open Questions

- Uploading proprietary repositories creates security, compliance, residency, deletion, and customer-trust
  obligations far beyond the local product.
- Malicious source may require stronger worker isolation than ordinary process resource limits.
- Snapshot and graph retention may dominate cost for monorepositories and frequent canonical revisions.
- At-least-once jobs require idempotency across every durable write, cache update, publication, and audit effect.
- Search indexes and caches are common tenant-isolation failure points even when primary rows are scoped correctly.
- Deletion guarantees are difficult when backups, audit retention, failed fragments, and search indexes differ.
- A relational store may eventually be insufficient, but selecting a graph database before measuring workloads
  would create premature coupling.
- It remains open whether the first deployment target is self-hosted, single-tenant managed, or multi-tenant.
- It remains open whether source objects are retained after publication or deleted immediately.
- A later phase may consider dirty/task-overlay upload only with a separate consent, authorization, retention,
  deletion, and isolation specification.
- It remains open which production SLOs, quotas, maximum repository sizes, regions, and retry budgets are required.
- Local and remote Ferrus versions need a rollout and compatibility policy for extractors and wire protocols.
