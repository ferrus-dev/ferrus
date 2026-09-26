# Semantic Retrieval and Embeddings

## Current decision

Do not make embeddings part of the default Ferrus repository graph yet.

The structural graph and project memory already cover the highest-confidence navigation paths: exact paths,
symbols, relationships, approved outcomes, immutable revisions, freshness, provenance, and verified snippets.
Embeddings would improve a different problem - vague conceptual discovery when the caller does not know the path,
symbol, vocabulary, or historical decision name.

That benefit is useful, but it does not justify adding a mandatory model runtime, provider, network dependency, or
vector extension before a measured retrieval gap exists. Semantic retrieval should remain an optional derived
projection and must never become evidence or authority by itself.

## Where embeddings help

- map natural-language questions to likely repository symbols or memory entities;
- improve recall for synonyms and concepts that do not share exact tokens;
- rank a broad deterministic candidate set before bounded context assembly;
- find related approved outcomes when exact milestone or entity identifiers are unknown.

They should not replace exact path/symbol lookup, graph traversal, freshness comparison, provenance, authorization,
or evidence-backed repository-memory links.

## SQLite options

SQLite can store vectors as blobs or ordinary values, and Ferrus can score a small bounded corpus in process. For a
larger corpus, vector search is available through extensions rather than the ordinary SQL feature set.

SQLite Vec1 is an official approximate-nearest-neighbor extension with cosine and Euclidean distance support. Its
current documentation still calls out insufficient testing and optimization work. `sqlite-vec` is a portable
third-party alternative with Rust packaging, but it is pre-1.0 and explicitly allows breaking changes.

For Ferrus, a statically linked or bundled extension is preferable to arbitrary runtime extension loading. SQLite
disables runtime extension loading by default for security reasons, and shipping platform-specific shared libraries
would add Windows, macOS, Linux, architecture, signing, and upgrade work.

Primary references:

- https://sqlite.org/vec1/doc/trunk/doc/vec1.md
- https://www.sqlite.org/loadext.html
- https://github.com/asg017/sqlite-vec

## Architecture boundary

Define a semantic projection with its own identity:

```text
semantic_revision_id = hash(
  source_revision,
  embedding_model_and_version,
  chunking_version,
  normalization_version,
  semantic_policy
)
```

Changing an embedding model must not rebuild or rename a structural graph snapshot or memory revision. A semantic
record should point back to an exact node, memory entity, or evidence-bearing chunk from those revisions.

The local-first ports should separate:

- `EmbeddingProvider` - turns bounded sanitized text into a model-versioned vector;
- `SemanticProjectionStore` - persists projection revisions and vectors;
- `SemanticCandidateSearch` - returns bounded candidates with scores and exact origins;
- deterministic fusion - combines semantic candidates with exact structural and memory ranking.

The same contracts can later map to a remote embedding worker and tenant-scoped vector service without changing the
local structural graph.

## Suggested implementation sequence

1. Add an offline evaluation corpus of vague conceptual queries and measure the exact-search miss rate.
2. Build a local spike over approved memory records and declaration/documentation nodes only.
3. Store model, dimensions, chunking version, origin revision, and content digest with every vector.
4. Compare bounded in-process cosine scoring with a bundled SQLite vector extension.
5. Add hybrid ranking only if Recall@K improves without weakening exact-result precision or privacy.
6. Keep the feature disabled by default until packaging, deletion, rebuild, and cross-platform gates pass.

## Effort estimate

The storage call itself is not the expensive part.

- Evaluation plus a throwaway local spike: about 3-5 engineering days.
- A shippable optional local feature: about 2-4 weeks, including model configuration, chunking, incremental rebuild,
  packaging, privacy policy, diagnostics, migration, and cross-platform tests.
- Distributed multi-tenant support: another 4-8 weeks or more for quotas, model rollout, batching, worker scheduling,
  deletion, observability, and tenant isolation.

## Expected product impact

The primary gain is recall and easier discovery, not token reduction. Better ranking can indirectly reduce tokens by
returning fewer irrelevant candidates and smaller context packets. Embedding generation and vector metadata add
their own compute, storage, and operational cost, so token savings must be measured end to end rather than assumed.

Ferrus should revisit implementation when its evaluation shows that exact structural and memory retrieval misses a
material share of real conceptual queries or produces context packets that hybrid ranking can reliably shrink.
