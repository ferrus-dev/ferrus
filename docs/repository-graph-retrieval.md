# Repository Graph Retrieval Contract

This document is the normative retrieval contract shared by the local CLI, role-scoped MCP adapters, and future
remote repository-graph backends. Backend-specific storage, task leases, and process working directories are not
part of this contract.

## Version and identity

Every request carries `scope.wire_version`, an explicit repository identity, a published-view or immutable-snapshot
selector, and non-zero client budgets. Every successful snapshot query returns the supported wire version, the
repository and snapshot identities, the indexed source-revision ID and manifest digest, freshness, diagnostics,
pagination/truncation state, and operation data. Status uses the same envelope fields, with optional snapshot and
source-revision identities when no compatible published snapshot exists.

Cursors are opaque. A cursor is bound to the wire format, operation, immutable snapshot, and normalized request
parameters. Reusing it for another operation, snapshot, search text, filter set, or lookup is `stale_cursor`.
Publishing a newer view does not invalidate a cursor explicitly pinned to its immutable snapshot.

## Orthogonal status

Availability, the newest build attempt, and published-snapshot freshness remain independent:

- availability is `not_built`, `available`, or `incompatible`;
- build state is `building`, `complete`, `published`, `failed`, or `superseded` when an attempt exists;
- freshness is `fresh`, `stale`, `unknown`, or `not_applicable`.

This permits an available published snapshot to be reported together with a newer failed refresh. Machine-readable
recommended actions are `index`, `wait_for_build`, `retry_index`, `refresh_index`, and `rebuild`. With no published
snapshot, ordinary queries distinguish `not_built`, `index_building`, and `index_failed`; status returns those
conditions as data so an agent can inspect them without treating optional graph absence as an internal failure.

## Search

Search considers normalized names, semantic keys, and repository-relative evidence paths. Node-kind filters are
exact and path filters match the exact repository path or descendants; path text is escaped before SQL `LIKE`
matching. The primary match classification and ordering are:

1. exact semantic key;
2. exact repository-relative path;
3. exact normalized name;
4. normalized-name prefix;
5. normalized-name substring;
6. semantic-key substring;
7. path substring.

Ties are ordered by normalized name and opaque node ID. Responses expose the primary `match_kind`, score, and all
matched fields. Repeating a request against the same snapshot produces semantically identical ordered results.

## Evidence and context contracts

Returned graph facts retain extractor identity and version, evidence content identity, repository-relative path,
optional half-open source span, resolution state, and confidence. Missing evidence remains explicit; it is never
reconstructed from the process current working directory. An absent relationship means only that the index does
not know it.

Context requests use one or more typed node, semantic-key, or path seeds plus an explicit direction, edge-kind
filter, and unresolved/external inclusion policy. Context items carry fact provenance and typed selection reasons.
Seed resolution is exact and may produce multiple nodes for a shared semantic key or evidence path; a missing seed
is an invalid request rather than a guessed match. Expansion is cycle-safe and deduplicates nodes reached through
multiple relationships while retaining their distinct selection reasons. Nodes without source evidence may guide
traversal but are not emitted as context items.

Context ranking is deterministic: best selection reason, expansion depth, evidence path and start offset, node
kind, semantic key, then opaque node ID. Selection priority is exact seed, containment, declaration, resolved
dependency, documentation, configuration, then other relationships. Local assembly additionally has a bounded
candidate/edge safety cap; exhausting it returns `capability` truncation rather than silently dropping facts.

## Diagnostics, pagination, and budgets

Diagnostics contain bounded machine codes, severity, and optional repository-relative locations, never source
bodies or free-form parser text. Summary counts describe the full snapshot diagnostic set; ordered diagnostic items
are independently capped and state whether items were truncated.

Service limits cap requested results, result bytes, depth, duration, and diagnostic items. Result and byte
exhaustion returns the deterministic prefix that fits. A first item larger than the byte budget returns an empty,
terminal truncated page rather than a cursor that cannot advance. Duration exhaustion returns a valid response
with `duration` truncation and any completed deterministic prefix; it is not a generic backend error. A continuation
cursor is issued only after at least one result was returned and only when that operation supports continuation.

Invalid versions, requests, selectors, or cursors remain typed errors. Storage corruption or unavailability is a
backend error; optional absence, active indexing, a failed build, incompatibility, staleness, and budget truncation
must not be collapsed into that category.
