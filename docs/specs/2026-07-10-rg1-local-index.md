# Repository Graph Phase 1: Local Repository Index Specification

## Summary

This phase delivers the first useful local repository graph for Ferrus. It discovers a repository snapshot,
extracts deterministic structural facts, resolves conservative cross-file relationships, incrementally reuses
unchanged work, and exposes explicit CLI commands for indexing, status, search, node inspection, and bounded
neighborhood traversal.

The first language-aware vertical slice is Rust-first for dogfooding on Ferrus itself, while every extractor
remains behind the Phase 0 contracts so additional languages or precise semantic index formats can be added
without changing storage and query APIs.

## Goals

- Index the canonical local workspace without executing repository code.
- Build a useful generic graph of directories, files, documents, manifests, configuration, and entry points.
- Extract Cargo packages, targets, declared dependencies, and Rust modules and symbols.
- Preserve unresolved and heuristic relationships honestly instead of fabricating semantic certainty.
- Reuse unchanged file fragments by content identity and update add/change/delete/rename cases correctly.
- Publish complete snapshots atomically while keeping the prior snapshot queryable during a build.
- Provide minimal, scriptable `ferrus graph` CLI commands with JSON output where appropriate.
- Establish performance and usefulness baselines on Ferrus and representative fixture repositories.

## Non-Goals

- Exposing repository graph operations through MCP.
- Automatically injecting graph content into agent prompts.
- Building Executor worktree overlays or pinning snapshots to tasks and runs.
- Providing a complete call graph, type inference, macro expansion, data flow, or compile-time semantics.
- Supporting every programming language in the initial release.
- Running build scripts, proc macros, package hooks, or arbitrary repository commands.
- Adding embeddings, vector databases, generated summaries, background file watchers, or a graph database.
- Implementing distributed indexing or remote storage.

## Context

Phase 0 must be complete: repository and snapshot identities, graph facts, evidence, bounded query DTOs,
sidecar migrations, and atomic publication are prerequisites for this phase.

Ferrus is currently a Rust repository of roughly one hundred tracked files, which makes it a useful dogfooding
corpus but not a sufficient scalability benchmark. The implementation must therefore use content identities,
extractor capabilities, and bounded queries rather than relying on the current repository being small.

Git repositories should use Git-native discovery and revision information. Non-Git projects must retain a
filesystem fallback so repository graph support does not become a hidden requirement for the core Ferrus
orchestration loop.

## Requirements

- Implement a `RepositorySource` for Git workspaces and a filesystem fallback for non-Git workspaces.
- Git discovery must include tracked files and non-ignored untracked files with NUL-safe path handling. Derive
  directory nodes from discovered paths rather than walking `.git` internals.
- A source manifest must identify the base tree or revision, repository-relative paths, content identities,
  file modes, dirty state, index configuration, and extractor set.
- Re-check the source manifest before publication. If relevant source content changed during indexing, do not
  publish the build as fresh.
- Never follow a symlink outside the repository root. Represent or skip symlinks according to a documented policy.
- Exclude `.git`, Ferrus runtime artifacts, configured sensitive patterns, generated/vendor paths, binary files,
  and oversized files according to explicit policy. Skips must produce diagnostics, not silent omissions.
- Apply limits for file count, aggregate bytes, per-file bytes, parser time, and diagnostic volume.
- Provide a generic extractor for repository, directory, file, document, manifest, configuration, and entry-point
  nodes and their containment or classification edges.
- Provide a Cargo extractor for workspaces, packages, targets, manifests, and declared internal or external
  dependencies without running project code.
- Provide a Rust syntax extractor for modules, structs, enums, traits, functions, constants, type aliases,
  implementation blocks, `mod`, `use`, and `pub use` declarations with source spans.
- Prefer a parser that tolerates incomplete source and can support later languages; parser choice must remain an
  extractor implementation detail. Parsing must not invoke macros, compilers, build scripts, or language servers.
- Resolve Rust module paths, imports, and re-exports conservatively. Preserve unresolved targets with evidence and
  diagnostics; do not emit `calls`, complete `references`, or `implements` edges without semantic proof.
- Extractors must produce deterministic per-file graph fragments independent of SQLite.
- Incremental indexing must skip unchanged fragments and correctly invalidate path-sensitive relationships after
  add, change, delete, rename, manifest, module-layout, or extractor-version changes.
- Cross-file resolution must run against one immutable source manifest and write only to its building snapshot.
- Implement indexed lookup for paths, normalized names, semantic keys, outgoing edges, and incoming edges.
- Add the following CLI surface:
  - `ferrus graph index [--full] [--json]`
  - `ferrus graph status [--json]`
  - `ferrus graph search <query> [--kind <kind>] [--path <prefix>] [--limit <n>] [--json]`
  - `ferrus graph show (--node <id> | --symbol <key> | --path <path>) [--json]`
  - `ferrus graph neighbors <node-id> [--direction <direction>] [--kind <kind>] [--depth <n>] [--limit <n>] [--json]`
- Phase 1 CLI traversal is a low-level bounded graph inspection surface. Ranked multi-source context packets and
  the shared `ferrus graph context` command belong to Phase 2 and must consume these query abstractions rather than
  creating another graph reader.
- `ferrus init` must remain lightweight; automatic indexing is deferred.
- CLI output must include snapshot ID, freshness, diagnostics summary, truncation, and evidence locations.
- Record build metrics including discovered, reused, parsed, skipped, failed, node, edge, byte, and duration counts.

## Milestones

- [x] #1.0 Implement repository discovery and deterministic source manifests

ID: rg1.0
Depends on: none

Add Git and filesystem source adapters, repository-relative path normalization, content identities, dirty-state
tracking, explicit exclusions, resource limits, and deterministic source-manifest hashing.

Implemented in `src/repository_graph/source/`, with source configuration and adapter contracts in
`src/repository_graph/config.rs` and `src/repository_graph/ports.rs`.

- [x] #1.1 Build the generic structural and document extractor

ID: rg1.1
Depends on: rg1.0

Extract repository, directory, file, document, manifest, configuration, and entry-point facts with containment,
classification, spans where available, and skip diagnostics.

Implemented in `src/repository_graph/extractors/generic.rs` with deterministic per-file fragments and a
repository-level root fragment.

- [x] #1.2 Build the Cargo package and dependency extractor

ID: rg1.2
Depends on: rg1.0

Parse Cargo workspace and package manifests without executing repository code, and emit package, target,
entry-point, internal dependency, and external dependency facts.

Implemented in `src/repository_graph/extractors/cargo.rs` with conservative unresolved candidates for later
cross-file resolution.

- [x] #1.3 Build the Rust syntax extractor

ID: rg1.3
Depends on: rg1.0

Parse Rust source through the extractor interface and emit module, symbol, declaration, import, re-export,
containment, signature, visibility, and source-span facts while tolerating incomplete files.

Implemented in `src/repository_graph/extractors/rust.rs` using a resource-bounded Tree-sitter syntax parser.

- [x] #1.4 Resolve conservative cross-file module and dependency relationships

ID: rg1.4
Depends on: rg1.1, rg1.2, rg1.3

Resolve Cargo package membership, Rust module paths, imports, and re-exports against one source manifest;
preserve unresolved and external targets explicitly and avoid unsupported semantic claims.

Implemented in `src/repository_graph/resolution.rs` as a storage-independent, resource-bounded pass over one
immutable source manifest, with conservative Cargo workspace/package/dependency and Rust module/import resolution.

- [x] #1.5 Implement incremental indexing and snapshot publication

ID: rg1.5
Depends on: rg1.4

Coordinate extraction, unchanged-fragment reuse, invalidation, cross-file resolution, manifest revalidation,
diagnostics, build metrics, and atomic publication of a complete snapshot.

Implemented in `src/repository_graph/index.rs` and `src/repository_graph/index_store.rs`, with a versioned SQLite
fragment cache and build metrics, deterministic snapshot identities, complete-snapshot transactions, source
revalidation, and compare-and-set publication that preserves the previously published view on failure.

- [x] #1.6 Add the repository graph CLI and local benchmarks

ID: rg1.6
Depends on: rg1.5

Expose index, status, search, show, and neighbors commands; add JSON output, help and user documentation, dogfood
the index on Ferrus, and record cold-build, no-op update, changed-file update, and query baselines.

Implemented in `src/cli/commands/graph.rs` and `src/repository_graph/query_sqlite.rs`, with indexed and bounded
SQLite lookup/traversal, human and JSON output, an explicit Criterion medium-fixture harness in
`benches/repository_graph.rs`, Ferrus dogfood results in `docs/repository-graph-benchmarks.md`, and user-facing
command documentation in `README.md`.

## Acceptance Criteria

- Ferrus can index its own canonical workspace and answer where a Rust symbol is declared, what contains it,
  and which resolved modules import or re-export it.
- The same source manifest, index configuration, graph model, and extractor set produce the same snapshot identity
  and deterministic graph facts.
- A no-op update invokes no file extractors and does not publish a semantically duplicate snapshot.
- Add, modify, delete, rename, manifest-change, module-layout-change, and extractor-version fixtures produce no
  stale nodes or resolved edges.
- A source change during a build prevents that build from being published as fresh.
- A failed, cancelled, or crashed build leaves the previous published snapshot readable.
- Ignored, sensitive, binary, symlink, generated/vendor, malformed, and oversized fixtures are handled according
  to documented policy and produce bounded diagnostics.
- Rust syntax errors do not abort the entire repository index.
- No extractor executes repository code, build scripts, proc macros, package hooks, or arbitrary configured commands.
- Search, show, and neighborhood results contain snapshot identity, freshness, repository-relative path, source
  span when available, provenance, resolution state, confidence, and truncation status.
- Query depth, result count, byte size, and duration are hard-capped by the service, regardless of CLI arguments.
- CLI commands behave consistently in human-readable and JSON modes and return actionable exit statuses.
- Benchmarks cover Ferrus plus fixtures large enough to reveal accidental full re-extraction and unbounded queries.
- The normal `ferrus init` path does not perform indexing or become dependent on parser availability.
- `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test` pass on supported platforms.

## Risks and Open Questions

- Tree-sitter offers tolerant multi-language syntax parsing, while a Rust-native AST parser may simplify builds;
  the extractor contract should make this choice reversible after a focused spike.
- Rust module resolution is affected by `cfg`, path attributes, generated code, and macros. The MVP must expose
  incomplete resolution rather than imply compiler-level accuracy.
- Content hashing every dirty or non-Git file may dominate no-op performance on very large repositories; staged
  metadata checks and Git blob identities may be necessary.
- User, repository, and global ignore rules can conflict with reproducibility. The manifest must record the
  effective index policy.
- Sensitive tracked files require an explicit default policy; Git tracking alone is not sufficient consent for
  future cloud upload.
- Copying unchanged rows between immutable SQLite generations is acceptable initially but may require
  content-addressed fragment storage after measurement.
