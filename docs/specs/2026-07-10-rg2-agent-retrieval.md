# Repository Graph Phase 2: Agent Retrieval and Evaluation Specification

## Summary

This phase exposes the published local repository graph to Ferrus Supervisor and Executor agents through a
small, read-only, snapshot-aware MCP retrieval surface. It adds deterministic search and bounded context assembly,
freshness and evidence reporting, safe on-demand snippets, and evaluation tooling that measures whether repository
context actually reduces navigation work, latency, and context volume.

The graph remains optional. Missing, stale, or failed indexes must be visible and actionable without weakening
the Supervisor–Executor workflow or mutating task state.

## Goals

- Give both Ferrus roles explicit read-only access to graph status, search, and bounded context.
- Return evidence-backed results with snapshot identity and honest freshness on every request.
- Assemble compact deterministic context packets instead of exposing unbounded graph traversal.
- Read source snippets safely on demand without storing complete source bodies in the graph database.
- Keep indexing lifecycle under CLI/HQ control rather than agent control.
- Advertise repository retrieval capabilities without injecting the whole graph into prompts.
- Measure navigation success, context bytes, tool calls, latency, and stale-result behavior with repeatable evals.

## Non-Goals

- Letting agents build, rebuild, clear, configure, or mutate the repository index.
- Automatically adding repository graph content to every Executor or Supervisor prompt.
- Adding Executor worktree overlays or task snapshot pinning.
- Implementing embeddings, vector search, learned ranking, or LLM-generated summaries.
- Expanding Rust semantic coverage beyond the facts produced by Phase 1.
- Indexing project memory, raw task/run artifacts, or archived outcomes.
- Changing task leases, state transitions, checks, submission, review, or approval behavior.
- Providing remote or distributed query services.

## Context

Phase 0 and Phase 1 must be complete. This phase assumes a versioned local sidecar, immutable published snapshots,
freshness diagnostics, generic and Rust facts, bounded query DTOs, and working CLI search and context commands.

Ferrus MCP servers are role-scoped and can resolve task/run context. Repository retrieval is different from task
coordination: it is read-only, project-scoped, and useful before or after an agent has claimed a task. Tool
registration must therefore avoid coupling graph reads to task lease ownership.

The central product question is not whether the graph contains many facts, but whether an agent can find relevant
code and supporting structure with less repository rescanning. Evaluation and telemetry are part of the feature,
not optional polish.

## Requirements

- Expose these read-only MCP tools to both Supervisor and Executor role-scoped servers:
  - `repository_graph_status`;
  - `repository_search`;
  - `repository_context`.
- Add a shared CLI adapter:
  - `ferrus graph context (--symbol <key> | --path <path> | --node <id>) [--depth <n>]`
    `[--max-results <n>] [--max-bytes <n>] [--json]`.
- The CLI and MCP must use the same context service. Phase 2 must consume Phase 1 search/show/neighborhood
  abstractions rather than create a second graph reader.
- Add a compact `ferrus://repository/summary` resource only if it can be generated deterministically within a
  strict size budget. Dynamic filtering and traversal must remain tools, not resource URI conventions.
- Graph tools must not require or reclaim a task lease and must never update task or run state.
- Agents must not receive graph tools that build, rebuild, clear, publish, configure, or garbage-collect indexes.
- Every response must use a versioned envelope containing:
  - query API version;
  - repository and snapshot identity;
  - freshness and source revision information;
  - results and evidence;
  - diagnostics;
  - truncation state;
  - continuation cursor when supported.
- Missing indexes must return an actionable `not_built` result that points the operator to `ferrus graph index`.
- Building, failed, incompatible, and stale indexes must have distinct machine-readable states and concise
  human-readable guidance.
- `repository_search` must support exact, normalized-name, semantic-key, and repository-relative path lookup,
  filters by node kind and path prefix, deterministic ordering, and bounded pagination.
- `repository_context` must accept one or more path, node, or semantic-key seeds and bounded expansion policy.
- Context assembly must rank deterministically, prioritizing exact seeds, containment, declarations, resolved
  dependencies, and relevant documentation/configuration facts before weaker or unresolved relationships.
- Context responses must deduplicate repeated nodes and evidence while preserving why each result was selected.
- Server-side hard caps must override client-requested depth, results, bytes, duration, and diagnostic count.
- A timeout or budget exhaustion must return a valid truncated response rather than an unbounded operation or
  generic internal error.
- Every returned node or edge must include repository-relative path, source span when available, extractor
  provenance, resolution state, and confidence.
- Unresolved, heuristic, external, and stale facts must be clearly labeled in both JSON and human-readable output.
- Source snippets must be resolved exclusively through the Phase 0 `SnapshotContent` boundary for the response's
  immutable snapshot. Retrieval must not infer source identity from process current working directory.
- `SnapshotContent` must validate the repository-relative path, prevent symlink escape, and verify content identity
  before returning a snippet.
- If snapshot content is unavailable or has changed, return location and stale-content diagnostics without reading
  an unverified snippet.
- Snippet limits must be independent from total response limits and must exclude secrets or configured sensitive
  paths according to the index policy.
- Agent prompts and generated skill guidance may advertise the retrieval tools and recommend status-first usage,
  but must not embed repository summaries or require graph use for every task.
- Tool descriptions must explain that absence of a relationship means “not known by this index,” not proof that
  no relationship exists.
- Record privacy-safe query metrics: tool, snapshot, freshness, duration, result count, response bytes,
  truncation, diagnostics count, and error category. Do not record search text, source snippets, or source bodies
  by default.
- Build a deterministic evaluation corpus containing representative navigation, dependency, documentation,
  configuration, stale-index, malformed-source, and missing-index tasks.
- The corpus must contain at least twenty labeled cases, including exact path, exact unique symbol, ambiguous
  symbol, supported discovery, unsupported capability, stale, missing, and truncation cases.
- Compare graph-assisted and baseline navigation on success, time-to-first-relevant-file, tool calls, files read,
  context bytes, and total task duration where measurable.
- Initial quality gates are 100% Recall@1 for exact path and supported exact unique-symbol cases, at least 90%
  Recall@10 for labeled supported discovery cases, identical semantic output for repeated same-snapshot queries,
  no correctness regression versus baseline navigation, and at least 20% median reduction in repository context
  bytes or files read on the designated graph-assisted navigation subset.
- Record warm/cold latency distributions and response sizes in machine-readable output. Do not make CI depend on
  brittle cross-machine wall-clock thresholds until stable platform-specific budgets are established.
- Stronger retrieval guidance or future automatic context injection remains disabled if a quality gate fails.

## Milestones

- [x] #2.0 Stabilize repository retrieval semantics and response envelopes

ID: rg2.0
Depends on: none

Finalize MCP request/response schemas, freshness and error states, deterministic ordering, evidence rules,
pagination, and hard budget behavior using the Phase 1 CLI queries as executable reference behavior.

Normative contract: [Repository Graph Retrieval Contract](../repository-graph-retrieval.md). Implemented in
`src/repository_graph/query.rs` and `src/repository_graph/query_sqlite.rs`, including typed context seeds, explicit
source-revision envelopes, orthogonal index/build/freshness states, deterministic match classification,
snapshot-bound pagination, bounded diagnostics, and valid truncation responses for hard budget exhaustion.

- [x] #2.1 Add graph status and bounded search MCP tools

ID: rg2.1
Depends on: rg2.0

Register read-only status and search tools for both roles, implement missing/stale/building/failed behavior,
filters and pagination, and verify that graph reads require no task lease and mutate no runtime state.

Implemented by the shared local adapter in `src/repository_graph_runtime.rs` and the role-visible
`repository_graph_status` and `repository_search` handlers in `src/server/tools/`. The MCP boundary preserves
Phase 1 status/search envelopes, bounded filters and cursors, uses no task context or lease helpers, and has a
runtime-isolation test proving that reads neither open nor alter `ferrus.db`.

- [x] #2.2 Implement deterministic bounded context assembly

ID: rg2.2
Depends on: rg2.0

Build seed resolution, evidence-preserving expansion, deterministic ranking, deduplication, diagnostics,
truncation, and continuation behavior for `repository_context`.

Implemented by `SqliteGraphQuery::context`, with exact typed seed resolution, policy-aware cycle-safe expansion,
evidence-preserving deterministic ranking, snapshot/parameter-bound cursors, diagnostics, and explicit
result/byte/depth/duration/capability truncation. `ferrus graph context` consumes the same machine-local runtime
adapter that the role-scoped MCP boundary will expose in RG2.4.

- [x] #2.3 Add hash-verified source snippets and repository summary

ID: rg2.3
Depends on: rg2.1, rg2.2

Implement safe on-demand snippet retrieval with content-identity verification and sensitive-path checks, plus a
strictly bounded deterministic summary resource if it remains useful after measurement.

Implemented by `LocalSnapshotContent` and the shared context runtime adapter. Opt-in snippets are root-confined,
symlink-safe, source-policy checked, SHA-256 verified against immutable snapshot file metadata, span sliced,
deduplicated, and independently byte bounded; changed/unavailable content produces location-bearing diagnostics.
The conditional summary resource remains unregistered until RG2.5 demonstrates value because it currently
duplicates status and would add unsolicited context volume.

- [x] #2.4 Integrate role-scoped MCP registration, guidance, and query telemetry

ID: rg2.4
Depends on: rg2.1, rg2.2

Complete server registration, tool schemas, tests, user and agent guidance, and privacy-safe query metrics without
injecting graph output into task or review prompts.

Implemented by the bounded `repository_context` schema/handler registered beside status and search for both roles,
with status-first generated skill guidance and structured opt-in query telemetry. Metrics record only tool,
snapshot, freshness, duration, counts, response bytes, truncation, and error category; their type cannot contain
queries, paths, snippets, or source bodies, and repository reads remain independent from task leases/runtime state.

- [x] #2.5 Build repository navigation evaluations and establish usefulness gates

ID: rg2.5
Depends on: rg2.3, rg2.4

Create deterministic fixtures and graph-assisted versus baseline evaluation scenarios, record performance and
context-volume results, and document thresholds for retrieval quality and later automation decisions.

Implemented by the versioned 26-case corpus in `tests/fixtures/repository_graph_eval`, a shared real-index/query
harness, `cargo test --test repository_graph_eval`, and the machine-readable
`cargo run --example repository_graph_eval -- --output <path>` runner. Current gates achieve 100% exact-path and
unique-symbol Recall@1, 93.75% supported-discovery Recall@10, 100% repeated-query determinism, no supported baseline
regression, and 100% median files-read reduction. Context bytes remain substantially higher for broad traversal, so
optional retrieval guidance is eligible while automatic injection and the summary resource remain disabled.

## Acceptance Criteria

- Supervisor and Executor role-scoped MCP servers expose the same read-only repository status, search, and context
  capabilities without exposing index mutation tools.
- Graph reads work without a claimed task and do not alter tasks, runs, leases, retries, events that drive state,
  or scoped artifacts.
- Missing, building, stale, failed, incompatible, and fresh index states return distinct actionable responses.
- Every successful response includes query API version, snapshot identity, freshness, evidence, diagnostics,
  and truncation state.
- Exact path and symbol searches return deterministic bounded results consistent with equivalent CLI queries.
- CLI and MCP context operations use the same context service and produce equivalent semantic output for the same
  snapshot, seeds, and budgets.
- Context expansion cannot exceed server-side depth, result, byte, duration, snippet, or diagnostic caps.
- Budget exhaustion returns a valid truncated response with no partial JSON or generic internal error.
- Every result explains its repository-relative location, provenance, confidence, resolution state, and selection
  reason where context ranking is involved.
- Unresolved or heuristic relationships are never presented as exact semantic facts.
- Snippets are returned only when path confinement and content identity are verified against the referenced
  snapshot; changed or unavailable content produces a diagnostic instead.
- Sensitive paths and source bodies never appear in query telemetry or ordinary lifecycle events.
- Existing Executor and Supervisor prompts do not grow with repository-sized automatically injected context.
- At least twenty labeled evaluation fixtures cover navigation, imports/dependencies, docs/config entry points,
  malformed files, unsupported capabilities, missing indexes, stale snapshots, truncation, and ambiguous names.
- Evaluation output reports success, latency, tool calls, files read, context bytes, and graph query bytes in a
  form that can be compared across extractor or ranking revisions.
- Exact path and supported exact unique-symbol cases achieve 100% Recall@1; supported discovery cases achieve at
  least 90% Recall@10; repeated same-snapshot queries are semantically deterministic.
- Graph-assisted navigation has no correctness regression and reduces median repository context bytes or files
  read by at least 20% on the designated navigation subset. If any gate fails, stronger integration remains
  disabled and the limitation is recorded.
- `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test` pass.

## Risks and Open Questions

- Tool availability alone does not guarantee that external agent backends will use graph retrieval effectively;
  guidance and evals must distinguish retrieval quality from agent tool-selection behavior.
- Deterministic structural ranking may underperform semantic search for vague conceptual queries, but adding
  embeddings before measuring exact navigation would create unnecessary cost and privacy surface.
- Source snippets can become stale immediately in a dirty checkout; task-aware overlays are deferred to Phase 3.
- Token counting differs across agent models. Byte and character caps should remain authoritative even when an
  estimated token budget is reported.
- Query metrics must be useful without logging sensitive user searches or source content.
- RG2.5 found no evidence that `ferrus://repository/summary` adds value beyond status and on-demand tools, so it
  remains unregistered; reevaluate only with a measured navigation benefit.
- Cursor stability across snapshot publication must be defined; the safest default is to scope every cursor to
  one immutable snapshot.
