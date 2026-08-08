# Distributed Context Data Plane Contracts

This document is the normative RG5.0 contract for the optional distributed repository graph
and project memory prototype. It defines identities, protocols, consistency, tenant isolation,
data lifecycle, and the worker threat model. It does not select a cloud vendor or implement a
remote backend.

## Local-first boundary

Local Ferrus remains the default. Repository graph and project memory continue to use their
local adapters when remote indexing is absent or disabled. Loading Ferrus configuration,
starting HQ, running tasks, and querying local sidecars must not initialize a network client,
cloud SDK, queue, or remote credential provider.

The `distributed` module contains vendor-neutral wire and policy contracts only. Existing
repository graph and project memory domain DTOs remain independent of it. Future remote
adapters may translate between the two layers, but local stores and runtimes must not depend
on distributed types.

Remote indexing is opt-in. Missing endpoint, tenant, project, repository, or authorized
credential is an error. A local or remote failure must never trigger an implicit upload,
backend switch, or mutation of Ferrus orchestration state.

## Identity hierarchy

Cloud identity is deliberately distinct from machine-local Ferrus identity:

```text
tenant
  -> project
       -> repository
            -> repository manifest
            -> graph snapshot
       -> memory manifest
       -> memory revision
       -> index job
```

Every remote object, request, job, fact batch, publication, cache entry, and audit event is
authorized through explicit tenant and project scope. Repository-scoped values include the
repository as well. A digest identifies content; it does not grant access and never erases
scope. Identical content in two tenants remains two separately authorized, retained, and
deletable objects. Cross-tenant deduplication is forbidden.

`RemoteProjectRef`, `RemoteRepositoryRef`, `RemoteGraphSnapshotRef`, and
`RemoteMemoryRevisionRef` wrap the shipped semantic snapshot and revision identities with
cloud scope. `FederatedViewRef` pairs one immutable graph snapshot with one immutable memory
revision from the same tenant and project. It is a query selection value, not a third mutable
publication pointer.

Remote IDs are canonical bounded lowercase ASCII tokens. All content identities use the
existing validated digest contract. Unknown fields, invalid IDs, unsupported versions, and
scope mismatches fail closed at deserialization or validation boundaries.

## Protocol versions

Control, fact, query, and policy contracts have independent versions. A service must reject
an unsupported version explicitly. It must not reinterpret an older payload under newer
extractor, model, policy, or publication semantics.

The protocol groups are:

| Group | Purpose |
| --- | --- |
| Control | Submit, inspect, lease, heartbeat, cancel, publish, and delete |
| Fact | Idempotent worker output for one immutable job target |
| Query | Snapshot-, revision-, or federated-view-pinned bounded retrieval |
| Policy | Authorization, protection, retention, deletion, and worker isolation |

Wire errors contain a bounded code and retryable flag. They have no source, query, path,
credential, backend message, stack trace, or free-form detail channel.

## Immutable input and idempotency

Repository graph and project memory are separate job kinds with separate manifest contracts.
Repository jobs accept only a repository manifest. Memory jobs accept only a memory manifest.
A mismatched kind and input is invalid.

The logical index-job idempotency key includes:

- protocol version;
- tenant and project, plus repository through the repository manifest;
- graph or memory job kind;
- immutable manifest digest;
- source or memory policy digest;
- effective semantic configuration digest;
- model version;
- extractor-set digest.

Request IDs trace attempts but do not affect logical build identity. Repeated submission of
the same semantic inputs must return or resume the same logical job and converge on the same
immutable result. A change to scope, kind, policy, semantics, model, extractors, or manifest
creates a different key.

### Privacy-filtered packaging and source objects

RG5.1 implements packaging after the shipped local source boundaries. Repository packaging
accepts only a clean canonical repository manifest. The local include, ignore, sensitive-path,
binary, generated/vendor, size, file-kind, and symlink policies run before any object-store
write. Dirty Git workspaces and untracked overlays fail closed. The remote manifest contains
only included repository-relative paths, content identities, lengths, file modes, inferred file
roles, tenant-scoped object references, policy identity, and bounded diagnostic-code counts.
Excluded paths are never serialized into the remote manifest.

Project-memory packaging accepts only the four Phase 4 categories: specification structure,
approved Outcome, sanitized archive manifest, and sanitized runtime provenance. Specification
files are projected before upload. Non-authorized bytes become spaces while newlines and byte
offsets remain stable, so existing deterministic extractors and evidence spans still work
without uploading task bodies or unrelated specification text. Archive and runtime JSON must
match their closed sanitized schemas and canonical encoding. Every raw artifact category fails
closed even if a caller constructs a more permissive local policy.

Source objects and the manifest body are uploaded separately. The manifest reference names the
tenant-scoped immutable manifest object, digest, source policy, and cloud scope. Packaging
revalidates the mutable local source after all verified reads and returns no manifest if it
changed. Repeated packaging of identical authorized input returns the same manifest body and
reuses existing objects.

The prototype `EncryptedFilesystemObjectStore` demonstrates the storage contract without a
cloud SDK. It derives immutable object locations under tenant and project directories, verifies
SHA-256 before writes and after reads, encrypts each object with AES-256-GCM, authenticates its
tenant/project/object/digest scope as associated data, and serializes quota updates in a
separate SQLite metadata database. The encryption key is supplied by the caller and is never
stored with the objects. Construction fails unless the calling adapter declares authenticated
transport. Tests verify that plaintext is absent at rest and ciphertext tampering is rejected.
Production adapters must establish transport security rather than trusting a declaration.

## Job consistency

Index jobs use at-least-once execution. Exactly-once worker execution is not assumed.

```text
queued -> leased -> running -> publishing -> complete
   |         |         |             |
   +---------+---------+-------------+-> failed
   +---------+---------+-------------+-> cancelled
             |         |
             +---------+-> queued after expiry or retry
```

Terminal states are `complete`, `failed`, and `cancelled`. A claim creates a lease generation.
Only its worker may heartbeat or advance the job while the lease is valid. Active leases are
renewed through publication. Expiry makes non-terminal work reclaimable. Attempts, lease
duration, total duration, retries, and resource use are server bounded.

Cancellation and publication are serialized by the coordinator. Once cancellation is
accepted, no worker fact or publication transaction may make the job visible. A stale worker
or lease generation cannot publish. A duplicate queue delivery may repeat computation, but
it cannot create duplicate visible facts or publications.

### Durable coordinator prototype

RG5.2 implements `IndexJobCoordinator` as a vendor-neutral port and
`SqliteIndexJobCoordinator` as a durable prototype adapter. Its database is explicitly supplied
by the remote adapter and is never `ferrus.db`, `repo-graph.db`, or `project-memory.db`. Opening
the coordinator applies only its own versioned schema.

Submission validates the complete versioned job and tenant-scoped manifest reference. The job
ID is deterministic from the semantic idempotency key. A unique scoped key plus an immediate
transaction makes concurrent duplicate submission converge on one durable record, including
after process restart.

Claims are serialized by tenant, project, and job kind. They create monotonically increasing
lease generations and bounded expirations. Start, heartbeat, failure, publication entry, and
completion require the exact live worker and generation. Lease expiry is clamped to the durable
total-job deadline, and no transition or heartbeat can extend work beyond it. A retryable worker
failure requeues a leased or running job while attempts remain; the next claim increments the
attempt and clears the prior bounded failure code. Publishing failures are terminal because
remote publication may need reconciliation rather than blind extraction retry.

Maintenance reclaims expired leases transactionally. It requeues work below the attempt limit,
records a typed terminal attempt-limit failure at the cap, and honors cancellation. Cancellation
atomically clears the lease and moves every non-terminal state to `cancelled`, so a stale worker
cannot enter publication afterward. Job inspection returns only manifests, identities, state,
lease metadata, counters, timestamps, and a bounded failure code. It has no source body or
free-form backend-error field.

## Fact batches

Workers emit immutable versioned batches for exactly one graph snapshot/build or memory
revision/build. Each header carries job, target, shard, sequence, extractor-set digest,
payload digest, and final-batch marker. The deterministic batch ID covers those values.

Graph payload nodes, edges, and diagnostics must name the target graph snapshot and build.
Memory entities, relationships, and diagnostics must name the target memory revision and
build. Cross-kind, cross-project, and mixed-target batches are rejected. Replaying an
identical batch is a no-op; changing its payload without changing its identity is invalid.

Partial batches may be durable for retry. Ordinary queries cannot observe them. Storage
promotes only a validated complete job into an immutable queryable snapshot or revision.

### Stateless worker and unpublished fact-store prototype

RG5.3 implements `StatelessIndexWorker` over the vendor-neutral coordinator, tenant object
store, and fact-batch store ports. It accepts only a currently running job whose worker ID and
lease generation still match the durable coordinator. Authority is checked before source reads
and before every fact-batch write, so accepted cancellation, lease loss, or job expiry stops the
attempt. Worker failures cross the boundary only as bounded `worker.*` codes.

The worker reads the manifest body through its tenant-scoped immutable object reference and
validates the body against the job input. Repository jobs reconstruct the shipped immutable
`SourceManifest`, run the existing generic, Cargo, Rust syntax, and conservative cross-file
extractors, and never invoke repository commands. Memory jobs reconstruct only the authorized
Phase 4 source manifest and run the existing deterministic memory extractors over the sanitized
objects uploaded by RG5.1. Retry timestamps are pinned to the durable job creation time so a
repeated memory attempt produces identical provenance and batch identities.

`WorkerLimits` independently caps manifest objects and bytes, per-object bytes, per-source and
total facts, diagnostics, parser and resolver time, total attempt time, batch facts and bytes,
and total output bytes. The worker is sequential even when the containing sandbox permits more
process concurrency. Output is sorted, deterministically chunked, and written as one sequence
with exactly one final marker. A retry recomputes the same immutable batches and reuses the
existing durable rows.

`FactBatchStore` exposes only unpublished worker progress and a separate ingestion read intended
for RG5.4. It is not a repository or memory query port. `SqliteFactBatchStore` is the durable
prototype adapter: it scopes every row by tenant, project, job kind, job, shard, and sequence;
encrypts the complete batch with AES-256-GCM; authenticates the scope as associated data;
enforces project and batch quotas; rejects sequence/final-marker conflicts and ciphertext
tampering; and converges repeated writes on the same batch. Partial rows survive worker loss but
remain outside every ordinary query path.

The Rust worker intentionally has no repository filesystem, process-command, or network API.
`WorkerSandbox` has secure-only variants for repository execution, egress, and filesystem
access, plus nonzero memory, CPU-time, and concurrency limits. A deployment adapter must apply
those controls with an OS process, container, or stronger sandbox before entering the worker;
the library contract is not itself a multi-tenant kernel isolation boundary. The prototype
tests exercise this contract in process, while production deployment still requires externally
enforced CPU, memory, filesystem, credential, and allowlisted-egress isolation.

## Publication and query consistency

Graph and memory have independent compare-and-set publication pointers and generations.
Publishing graph state cannot advance memory, and publishing memory cannot advance graph.
The publication transaction verifies job kind, scope, completion, cancellation state, lease
generation, expected pointer, and complete fact set before changing one pointer. A slower old
job loses to a newer publication instead of replacing it.

### Immutable storage and publication prototype

RG5.4 implements `RemotePublicationStore` as a vendor-neutral storage and publication port.
`SqliteRemotePublicationStore` is the durable relational prototype. It uses the same explicitly
supplied control-plane database as `SqliteIndexJobCoordinator`, while the unpublished worker
batch store remains separate. This lets one immediate transaction revalidate the exact worker
and lease generation, reject cancellation or expiry, insert or reuse the complete immutable
fact set, compare the expected pointer, update only the requested domain, and complete the job.
If any step fails, neither facts nor a pointer become visible.

Immutable graph snapshots and memory revisions occupy separate tenant/project namespaces.
Repository rows add repository scope. Fact bodies are encrypted with AES-256-GCM and authenticate
the tenant, project, job, domain, target, fact kind, and fact ID as associated data. Metadata is
bounded to identities, digests, counts, and timestamps. Per-project snapshot, fact, and encrypted
byte quotas plus per-snapshot and per-fact limits fail closed before publication. Conflicting
facts, missing internal relationship endpoints, incompatible schemas, and ciphertext or count
tampering return typed errors without exposing source-derived text.

The graph and memory CAS checks run before the same-target no-op branch. A stale publisher may
leave an immutable unreferenced target for later retention, but it completes without changing the
winner's pointer. Graph and memory generations advance independently. The prototype composes a
`FederatedViewRef` by reading both current pointers together; it does not persist a third mutable
federated pointer.

This adapter is a storage and consistency proof, not the RG5.5 query service. It supports internal
scoped snapshot reads for the next adapter layer, but exposes no network endpoint, search index,
credential handling, or unpinned query API. The current worker emits one deterministic shard, so
the ingestion prototype requires one contiguous sequence with exactly one final marker.

Repository-only queries pin one graph snapshot. Memory-only queries pin one memory revision.
Federated queries pin one validated `FederatedViewRef`. If a request asks for `latest`, the
service resolves each requested pointer exactly once at request start, uses the immutable
resolved values for the entire operation, and returns them in the response. Task and run
contexts always carry explicit immutable identities and never follow `latest` after dispatch.

Server-side result, byte, depth, duration, cursor, snippet, and diagnostic limits clamp all
client budgets. Remote adapters preserve existing local freshness, provenance, truncation,
and missing-relationship semantics. A missing relationship remains unknown, not absent.

## Authorization matrix

Authentication resolves a server-owned `AuthorizationContext`. A request body cannot choose
its credential class or permissions. The service authorizes the operation and scope before
looking up an object, job, project, snapshot, cache entry, or search record. Unauthorized and
foreign-scope probes return the same bounded denial without disclosing existence.

| Credential class | Allowed capability set |
| --- | --- |
| Query agent | Graph query, memory query, verified content |
| Snapshot uploader | Source upload only |
| Index worker | Claim job, read scoped source objects, write fact batches |
| Coordinator | Inspect/cancel jobs and publish graph or memory |
| Project operator | Upload, submit, inspect/cancel, query, verified content |
| Tenant administrator | Explicit full administrative matrix, including project/repository deletion and administrative diagnostics |

Credentials are scoped to tenant, project, or repository. Narrow scopes cannot be widened by
an object ID or digest. Query-agent credentials cannot upload, submit, inspect administrative
diagnostics, publish, change policy, or delete data. Worker credentials cannot query arbitrary
tenant data or publish a pointer.

## Data classification and protection

| Data class | Sensitivity | Required handling |
| --- | --- | --- |
| Repository source | Confidential | Policy filter before upload, tenant-scoped object, encrypted transport and storage |
| Curated memory source | Confidential | Phase 4 allowlist before upload, separate manifest and object scope |
| Derived facts | Confidential | Tenant/project rows and indexes, invisible before publication |
| Query input and verified snippets | Confidential | Bounded, authorized, excluded from default telemetry |
| Reusable credentials | Sensitive | Secret store only, never DTOs, logs, facts, or audits |
| Operational and audit metadata | Operational | Bounded IDs, codes, counters, and timestamps only |

Authenticated encryption in transit is mandatory. Remote storage must encrypt data at rest.
If the prototype backend cannot meet at-rest encryption, it must fail closed by default and
declare the limitation explicitly in deployment configuration and documentation. Tenant
scope applies to primary rows, object keys, queues, search indexes, caches, logs, metrics,
traces, and errors, not only to API routes.

## Retention and deletion

Retention is defined independently for uploaded source, unpublished facts, published graph
snapshots, published memory revisions, query caches, and audit records. Each class has an
explicit maximum age or is retained until deletion. Published pins required by active work
must be protected from age-based collection.

Project or repository deletion is an authenticated, idempotent job keyed by its exact scope
and requested coverage. It removes every covered source and derived class, including
secondary indexes and caches, without deleting another tenant's identical content. Progress
and failures use bounded audit metadata. A retry resumes the same logical deletion.
Completion means all live stores in the declared coverage are removed.

Backup expiry, immutable audit retention, legal holds, and provider deletion guarantees are
deployment policy. If they prevent immediate physical deletion, the service must document
the remaining class and deadline rather than reporting stronger deletion than it provides.

## Worker threat model

Repository and memory content are untrusted input. A malicious repository may contain parser
bombs, oversized files, adversarial encodings, symlinks, crafted manifests, misleading paths,
or content intended to trigger a package hook or network call.

Workers therefore:

- consume only authenticated versioned manifests and tenant-scoped read-only objects;
- use an ephemeral workspace and retain no durable progress locally;
- never execute repository code, compilers, build scripts, proc macros, package hooks, or tests;
- allow egress only to allowlisted control and object-storage endpoints;
- enforce snapshot, file, memory, parser-time, job-time, concurrency, fact, and diagnostic caps;
- emit facts only through the validated batch contract;
- receive no query-agent, publication, deletion, or tenant-administration authority;
- erase workspaces and short-lived credentials after an attempt.

The first prototype should prefer OS or container isolation with CPU, memory, filesystem, and
network controls. Ordinary in-process limits are not sufficient protection for a hostile
multi-tenant source corpus.

## Threats and required mitigations

| Threat | Required mitigation |
| --- | --- |
| Tenant object enumeration | Authorization before lookup; uniform bounded denial |
| Cross-tenant digest collision or reuse | Scope all references and storage keys; no cross-tenant deduplication |
| Duplicate queue delivery | Semantic job key and deterministic idempotent fact batches |
| Worker loss or stale worker | Renewable generation lease, bounded attempts, reclaim, publication guard |
| Cancellation race | Coordinator transaction prevents facts or publication after accepted cancellation |
| Old build replaces new | Independent generation-based CAS pointer per domain |
| Partial result disclosure | Unpublished namespace; query only immutable published targets |
| Parser or resource exhaustion | Worker sandbox and hard input, time, memory, output, and concurrency caps |
| Repository code execution | No build/package hooks; repository execution denied |
| SSRF or data exfiltration | Allowlisted egress and no unrestricted network |
| Credential theft through telemetry | Short-lived scoped credentials; no reusable credential fields or logs |
| Source, memory, path, or query leakage | Typed errors and audits; no free-form payload or local absolute paths |
| Deletion misses secondary data | Coverage by retention class, idempotent traversal, auditable completion |
| Protocol semantic drift | Independent explicit versions and fail-closed compatibility checks |

## Prototype limits

RG5.0 defines contracts, not a production service. It does not choose the queue, object store,
relational store, search index, graph database, cloud vendor, deployment model, SLO, region,
or billing model. Remote task overlays, dirty worktrees, arbitrary filesystem streaming, and
cross-repository federation remain out of scope. Later RG5 milestones must implement these
contracts behind optional adapters and prove them with shared local/remote behavior tests.
