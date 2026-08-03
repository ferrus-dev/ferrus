# Repository Graph Phase 4: Federated Project Memory Specification

## Summary

This phase adds a local-first project memory index and a federated context layer that combines current repository
structure with curated historical knowledge from Ferrus specifications, milestones, approved `## Outcome`
sections, archive manifests, and selected runtime metadata.

Repository graph, task graph, and project memory remain separate logical domains with independent revisions and
freshness. Federation occurs only in bounded query and context assembly; memory never becomes source-code fact or
orchestration state.

## Goals

- Preserve curated implementation history so agents do not rediscover completed work from raw run artifacts.
- Index specifications, milestones, `## Outcome` sections, decisions, deviations, validation evidence, follow-up
  work, and archive metadata with explicit provenance.
- Link memory records to repository paths, symbols, tasks, and milestones only when evidence supports the link.
- Query repository and memory independently or as one bounded, deterministic context response.
- Keep memory ingestion local, offline, optional, incremental, and rebuildable.
- Apply privacy-safe defaults that exclude raw task/run content, patches, logs, questions, and conversations.
- Preserve backend-neutral memory and federation contracts for a later tenant-scoped cloud implementation.

## Non-Goals

- Replacing `ferrus.db` as the source of truth for task, run, lease, or archive state.
- Merging project memory into the task scheduler or task dependency graph.
- Treating inferred historical statements as authoritative current-code relationships.
- Indexing every raw task, review, submission, patch, log, question, answer, or consultation by default.
- Generating new outcomes, decisions, or summaries with an LLM during ingestion.
- Adding embeddings, vector search, or similarity-only authoritative links.
- Uploading or synchronizing project memory to a remote service.
- Guaranteeing stable symbol links after arbitrary refactors or extractor-version changes.

## Context

Phases 0 through 3 must be complete. This phase assumes immutable repository snapshots, task-aware views,
evidence-backed queries, safe snippets, independent freshness reporting, and bounded CLI/MCP retrieval.

Ferrus already stores several classes of history with different authority and sensitivity:

- tracked specifications and stable milestone IDs;
- curated `## Outcome` sections created during spec closure;
- task/run/event/archive metadata in `ferrus.db`;
- machine-local archive manifests;
- raw scoped task, submission, review, patch, check, question, answer, consultation, and integration artifacts.

A checked-in approved outcome is compact project memory. A raw patch or conversation is forensic material and may
contain sensitive source, secrets, or transient reasoning. These sources must not receive the same default policy.

Repository snapshots and project memory advance independently. Every federated response therefore needs both a
repository snapshot identity and a memory revision identity with separate freshness.

## Requirements

- Define a `ProjectMemory` domain and `MemoryStore` interface separate from `RepositoryGraph`, `GraphStore`, and
  orchestration runtime types. A local backend may use namespaced tables in `repo-graph.db`, but that physical
  choice must not leak into domain APIs.
- Define versioned memory entities for specifications, milestones, outcomes, decisions, deviations, validation
  evidence, follow-up work, and task/run references.
- Define typed memory relationships including `contains`, `implements`, `validates`, `supersedes`, `concerns`,
  `touches`, and `follows_up` without reusing code-dependency meanings accidentally.
- Every memory entity and relationship must carry project scope, memory revision, source type, source locator,
  source revision/fingerprint, extractor ID/version, evidence span or record ID, resolution state or confidence,
  and indexing timestamps.
- Generate a deterministic `memory_revision_id` from authorized source fingerprints, memory policy, schema/model
  version, and extractor versions.
- Default authorized sources must be limited to:
  - tracked specification structure and milestone metadata;
  - approved `## Outcome` content;
  - archive manifests and archive identity/count metadata;
  - task/run/milestone/status/check identities required to cite validation provenance.
- Exclude raw task descriptions, submissions, reviews, patches, questions, answers, consultations, logs, and
  integration-error bodies unless a later explicit per-project policy enables a source category.
- Enabled source categories and their sensitivity must be visible in status output and query provenance.
- Do not copy full source files or raw archived artifact bodies when a locator, fingerprint, and evidence span are
  sufficient.
- Ingestion must be deterministic and incremental. Reuse unchanged sources, re-extract changed sources, and remove
  or tombstone records derived from removed or unauthorized sources.
- Failed or interrupted memory builds must not replace the last published memory revision.
- Parse specification structure, stable milestone IDs, completion state, and `## Outcome` sections without
  modifying the specification or inventing missing outcomes.
- Read machine-local archive manifests through the registered project data directory with explicit project scope;
  do not discover archives from arbitrary filesystem paths.
- Initial repository cross-links may use explicit repository-relative paths, explicit semantic keys, milestone or
  task origin metadata, approved archive manifests, and authorized changed-path lists.
- LLM-inferred or similarity-only links must not be stored as authoritative relationships.
- Preserve unresolved or stale links with diagnostics rather than silently discarding them or returning them as
  confirmed current-code facts.
- Repository graph nodes must not be rewritten to contain memory payloads. Federation happens in a
  `ContextService` that queries both stores.
- Federated search and context must support repository-only, memory-only, and combined scopes.
- Combined results must rank deterministically, preserve selection reasons, expand only evidence-backed links,
  deduplicate evidence, and enforce depth, result, byte, duration, snippet, and diagnostic budgets.
- Every federated response must report repository snapshot ID, task overlay revision when applicable,
  `memory_revision_id`, freshness for each domain, provenance, unresolved-link diagnostics, and truncation.
- Source or memory snippets may be read only after validating their locator and content fingerprint.
- Add exact local CLI operations:
  - `ferrus graph memory index [--full] [--json]`;
  - `ferrus graph memory status [--json]`;
  - extend `ferrus graph search` and `ferrus graph context` with
    `--domain <repository|memory|all>`, defaulting to `repository` for backward compatibility.
- Keep Phase 2 `repository_graph_status`, `repository_search`, and `repository_context` repository-only and
  backward compatible.
- Add these read-only MCP tools for Supervisor and Executor roles:
  - `project_memory_status`;
  - `project_context_search`;
  - `project_context`.
- Federated MCP requests must require an explicit `domain` of `repository`, `memory`, or `all`; they must not
  silently broaden an existing repository-only request.
- Agents must not receive tools that mutate curated memory, approve outcomes, change source policy, or launch an
  unbounded rebuild.
- Archive/spec closure remains the only workflow that creates or updates approved `## Outcome` project memory.
- Successful spec archive or outcome update must invalidate memory freshness and may schedule a best-effort
  incremental memory refresh outside the archive critical path.
- Memory indexing and query failures must never change task/run/lease/review/archive state or block the core
  Supervisor-Executor state machine.
- Record privacy-safe counts, durations, source categories, revision IDs, stale-link counts, and errors without
  source bodies, raw memory text, secrets, or absolute local paths.
- Incompatible derived memory schemas must support a rebuild rather than requiring lossless migration of every
  historical derived record.

## Milestones

- [x] #4.0 Define project memory, provenance, privacy, and federation contracts

ID: rg4.0
Depends on: none

Specify memory entities and relationships, independent revision identity, authorized source categories, privacy
defaults, link evidence, store/query interfaces, freshness, and federation semantics.

Normative contract: [Project Memory and Federation Contracts](../project-memory-architecture.md).

Implemented in `src/project_memory/` as backend-neutral domain, policy, diagnostics, store/query ports, bounded
wire DTOs, and explicit repository/memory federation targets. Storage and ingestion remain in later milestones.

- [x] #4.1 Implement deterministic specification and outcome ingestion

ID: rg4.1
Depends on: rg4.0

Parse tracked specs, stable milestones, completion state, and approved `## Outcome` sections into revisioned memory
records with incremental reuse, deletion/tombstone handling, and atomic publication.

Implemented by the tracked-spec source, deterministic specification extractor, incremental fragment cache, and
atomic `project-memory.db` revision publication in `src/project_memory/`.

- [x] #4.2 Add archive manifest and runtime provenance adapters

ID: rg4.2
Depends on: rg4.0

Read project-scoped machine-local archive manifests and the minimum authorized task/run/status/check metadata
needed for citations without importing raw artifact bodies.

Implemented by registered-project discovery plus sanitized archive and read-only `ferrus.db` adapters. Runtime
payloads are reduced to task, run, milestone, status, and check-event identities before extraction.

- [ ] #4.3 Build evidence-backed links between memory and repository context

ID: rg4.3
Depends on: rg4.1, rg4.2

Resolve explicit paths, semantic keys, task/milestone origins, archive records, and authorized changed-path
evidence against repository snapshots while preserving stale and unresolved links honestly.

- [ ] #4.4 Implement bounded federated search and context assembly

ID: rg4.4
Depends on: rg4.3

Add repository-only, memory-only, and combined search/context scopes, deterministic ranking and deduplication,
independent freshness, evidence-preserving cross-link expansion, and hard query budgets.

- [ ] #4.5 Expose memory lifecycle and federation through CLI and read-only MCP

ID: rg4.5
Depends on: rg4.4

Add the specified memory lifecycle CLI, domain-scoped search/context options, `project_memory_status`,
`project_context_search`, and `project_context` tools, archive/outcome invalidation hooks, guidance, and
privacy-safe metrics without creating another memory-authoring workflow.

- [ ] #4.6 Validate privacy defaults, freshness, lifecycle, and retrieval usefulness

ID: rg4.6
Depends on: rg4.5

Add deterministic fixtures, deletion and stale-link tests, archive lifecycle coverage, sensitive-source tests,
federated retrieval evals, retention diagnostics, and user documentation.

## Acceptance Criteria

- Ferrus builds and queries project memory locally without network access, cloud configuration, or an LLM.
- Identical authorized sources, policy, schema/model, and extractor versions produce the same memory revision and
  equivalent records.
- A no-change refresh performs no memory extraction and does not publish a duplicate semantic revision.
- Adding, changing, removing, or replacing an `## Outcome` updates only affected memory records and leaves no stale
  authoritative links.
- A failed or interrupted build leaves the prior published memory revision readable.
- Default policy excludes raw task/run artifact bodies, patches, logs, questions, answers, consultations, and
  integration-error bodies.
- Status clearly reports every enabled source category and whether it may contain sensitive content.
- Every memory result includes source locator, source revision/fingerprint, provenance, evidence, and resolution
  state or confidence.
- Every federated response identifies repository and memory revisions and reports their freshness independently.
- Unresolved or stale repository links are labeled and never returned as confirmed current-code relationships.
- Removing or disabling an authorized source removes or tombstones its derived records after the next successful
  build.
- Federated retrieval respects server-side depth, result, byte, duration, snippet, and diagnostic caps.
- Existing repository-only CLI and MCP requests preserve Phase 2 behavior; federated CLI/MCP requests use the
  specified domain selectors and never broaden scope implicitly.
- Memory ingestion and queries do not alter task, run, lease, review, approval, archive, or scheduler state.
- Spec closure remains the only workflow that writes approved `## Outcome` sections.
- Outcome/archive refresh failure does not fail successful spec closure; it leaves memory stale with diagnostics.
- Logs, events, and metrics contain IDs, counts, timings, categories, and errors without raw source or memory bodies.
- Tests cover deterministic extraction, no-op refresh, revision publication, deletion/tombstones, stale links,
  privacy defaults, archive scope, query truncation, lifecycle hooks, and incompatible-schema rebuilds.
- Retrieval evaluations demonstrate at least one scenario where approved historical context reduces raw artifact
  reading without displacing current source evidence.
- `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test` pass.

## Risks and Open Questions

- Outcome quality varies and may preserve conclusions that become obsolete after later refactors.
- Explicit paths and semantic keys can become stale across renames and extractor upgrades.
- Even curated outcomes may contain secrets, customer information, or proprietary design details.
- Runtime metadata and machine-local archives have different deletion and retention semantics from tracked specs.
- Federated ranking may over-prioritize historical memory and distract an agent from current source evidence.
- Memory revisions may grow without bound if old revisions and tombstones are retained indefinitely.
- It remains open whether any raw task/run category should ever be indexable, even through explicit project opt-in.
- It remains open whether users need field-level redaction in addition to source-category controls.
- A future cloud product may need separate consent for repository source and project memory upload.
