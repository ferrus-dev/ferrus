# Project Memory and Federation Contracts

This document is the normative RG4.0 contract for project memory. Project memory is a
derived, rebuildable domain. It does not replace orchestration state in `ferrus.db` and it
does not add historical payloads to repository graph nodes.

## Domain boundaries

The three domains advance independently:

- Orchestration owns task, run, review, lease, archive, and scheduler state.
- Repository graph owns revisioned facts about source structure.
- Project memory owns revisioned, curated historical knowledge.

`ProjectMemory`, `MemoryStore`, and `MemoryQuery` are backend-neutral ports. A local backend
may share a physical sidecar with repository graph, but table names, SQLite types, paths,
process IDs, and locks must not cross these interfaces.

## Revision identity and publication

A `memory_revision_id` is derived deterministically from project scope, the authorized
source fingerprint set, policy digest, memory model version, and extractor set digest. Build
IDs, timestamps, storage settings, and machine locations are not semantic revision inputs.

Builds move through `building`, `complete`, `published`, `failed`, or `superseded`. A
completed revision replaces a published view only through generation-based compare and set.
A failed or interrupted build leaves the previous publication readable. Equivalent inputs
reuse the semantic revision and do not create a new generation.

## Authorized sources and privacy

Every source category has an explicit enabled flag, sensitivity, and content-access mode.
The default policy enables only:

| Category | Default access | Sensitivity |
| --- | --- | --- |
| Specification structure | headings and milestone metadata | curated |
| Approved outcome | approved Outcome sections | curated |
| Archive manifest | identity and counts only | operational metadata |
| Runtime provenance | task, run, milestone, status, and check identity only | operational metadata |

Raw task descriptions, submissions, reviews, patches, logs, questions, answers,
consultations, and integration-error bodies are sensitive and disabled by default. A later
policy may enable a category explicitly, but no extractor may infer permission from the
availability of a file or database record.

Sources use project-scoped, portable locators. Arbitrary absolute paths are not a source
contract. Diagnostics and lifecycle events contain typed codes, IDs, categories, counts,
and durations only. They have no free-form content or message field.

## Entities, relationships, and provenance

Memory entities cover specifications, milestones, outcomes, decisions, deviations,
validation evidence, follow-up work, and task or run references. Relationships are typed as
`contains`, `implements`, `validates`, `supersedes`, `concerns`, `touches`, or `follows_up`.
These meanings are independent of repository dependency edges.

Every entity and relationship carries project scope, memory revision, source category,
portable locator, source fingerprint, extractor identity and version, evidence span or
record ID, resolution state, confidence, and observation/indexing timestamps. Repository
links require explicit path, semantic key, task origin, milestone origin, archive record, or
authorized changed-path evidence. Unresolved and stale links remain labeled. Similarity-only
or LLM-inferred links are never authoritative.

## Query and content boundaries

Status exposes every source category and its effective policy. Search and context requests
are versioned and bounded by result, byte, snippet, depth, duration, and diagnostic limits.
Cursors belong to an immutable revision and one exact request shape. Memory snippets are
optional and can be returned only after locator validation and fingerprint verification.

Structural entities and relationships may be stored in derived memory. Full source files or
raw archived artifacts are not copied when a locator, fingerprint, and evidence span are
sufficient.

## Federation

Federation is a read-only `ContextService`, not a merged store. Every request selects exactly
one domain: `repository`, `memory`, or `all`. The tagged target shape makes it impossible to
silently broaden a repository-only request. Existing repository-only contracts remain
unchanged.

Combined responses report repository snapshot and task-overlay identity when applicable,
memory revision identity, and freshness for both domains independently. `Fresh` in one
domain says nothing about the other. Results preserve their domain and provenance,
selection reasons, unresolved-link diagnostics, and truncation. Expansion crosses domains
only through evidence-backed links and stays within the common hard budget.

These contracts support a local in-process implementation first. Stable project,
repository, snapshot, revision, and request identities allow a future service to partition
storage and stateless workers by tenant and project without changing domain semantics.

## Local ingestion

The local implementation stores rebuildable memory in the registered project data directory
as `project-memory.db`. It is separate from `ferrus.db` and `repo-graph.db`. A build discovers
the complete authorized source manifest, reuses fragments whose category, locator,
fingerprint, policy, and extractor identity are unchanged, and publishes a completed
revision through generation-based compare and set. Removed sources are absent from the next
revision. A failed or racing build cannot replace the previous publication.

Tracked specifications expose structure and approved Outcome as separate fingerprinted
sources. This lets an Outcome-only edit reuse unchanged specification and milestone facts.
The extractor records the H1 specification title, stable milestone IDs and completion,
approved Outcome content, and explicitly titled decision, deviation, validation, and
follow-up subsections. It never writes to the specification.

Archive discovery starts only from the registered project data directory. Archive manifests
are sanitized to archive identity, repository-relative spec path, timestamp, counts, task
IDs, and milestone IDs before extraction. Runtime provenance opens the registered
`ferrus.db` read-only and retains only terminal task IDs, milestone IDs, statuses, run IDs,
run statuses, and bounded check-event identities. Raw payloads, task paths, agents, PIDs,
workspace paths, failure reasons, and archived artifact bodies do not cross the adapter.

## Repository link resolution

Repository links are stored in immutable link sets separate from semantic memory revisions.
Each set is identified by the memory revision, repository, selected repository snapshot, and
resolver identity. Reindexing unchanged memory against a new repository snapshot therefore
does not rewrite memory facts or change `memory_revision_id`.

The local resolver accepts only evidence with an explicit authority boundary:

- the tracked specification path carried by source provenance;
- the repository-relative spec path in an approved archive manifest;
- curated inline references written as `` `path:src/lib.rs` `` or
  `` `symbol:rust:function:src/lib.rs:run` ``;
- baseline and materialized repository snapshot IDs attached to terminal task/run metadata.

Changed-path evidence is calculated from file identities in the two authorized graph
snapshots. Raw patches and artifact bodies are not read. Task paths are also attributed to
their stable milestone when the runtime record supplies that origin.

An exact path or unique semantic-key match in the selected snapshot is `resolved`. If the
evidence resolved in an immutable origin or earlier link set but no longer matches the
selected snapshot, it remains `stale` and retains the historical snapshot target. A path
that never matched, an unavailable origin, or an ambiguous semantic key remains
`unresolved`. These states carry bounded diagnostic codes and are never promoted through
text similarity or LLM inference.

## Local query and federation

`SqliteMemoryQuery` reads one immutable memory revision without creating, migrating, or mutating
`project-memory.db`. Caller budgets are clamped to the configured service limits. SQLite progress handlers enforce
the duration deadline during scans and counts; result, byte, snippet, depth, and diagnostic limits are enforced
again while assembling responses. Memory cursors bind the operation, exact request shape, effective depth, and
memory revision. Optional snippets are requested only through `MemoryContent` with the stored locator and expected
fingerprint.

`FederatedContextService` keeps repository and memory queries separate and routes every request according to its
tagged `repository`, `memory`, or `all` target. Combined search uses deterministic score, domain, and identity
ordering before applying the common result and byte budget. Combined context resolves the selected repository and
memory revisions independently, loads only their exact repository link set, and crosses domains only through
resolved evidence-backed links. The response retains memory relationships, cross-domain link evidence, independent
freshness and diagnostics, and one cursor bound to both domain revisions. Work across sequential domain calls uses
the remaining common duration budget rather than restarting the deadline.
