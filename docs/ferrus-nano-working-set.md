# Nano repository working set

Status: implemented for #81. The managed host selects repository evidence before each model
attempt. Task lifecycle, graph publication, and memory source policies retain their existing
owners. Token-based compaction and live session resume remain #82 and #84.

## Journal, selection, and projection

The append-only journal retains original model responses and tool outcomes. A separate
`context_prepared` record stores host observations and the exact replacements used for the next
request. Commit succeeds before inference starts. Pure replay can reconstruct that projection
without querying a provider, filesystem, graph, or tool. User constraints, assistant messages,
opaque provider continuations, failed tools, and non-retrieval results are never replaced.

Evidence handles bind project, task, run, workspace, repository snapshot and task baseline/overlay,
and, where returned, memory revision and exact-pair cross-domain links. They retain source paths,
digests, spans, and the originating retrieval operation. Workspace fallback has distinct provenance;
it never becomes a graph fact. Source hashes are observed evidence, not a workspace freshness proof.

Selection visits newer tool results first. Native graph ranking, explicit seeds, and stable domain
tie-breakers remain intact within each packet. Exact duplicates reference a retained tool-result
message. Overlapping complete line reads merge only for equal binding, provenance, path, and content
digest, with byte-for-byte agreement on the overlap. Graph snippets retain their native byte spans
and packets. They are not merged with workspace lines or evidence from another snapshot. A retained
reference always points to materialized content in the same request.

Bounds per preparation are 256 candidate observations, 64 selected handles, 128 KiB of selected
retrieval packets, 32 verified paths, and 8 MiB of source reads. The workspace per-file bound still
applies. Encoded preparation is limited to 256 KiB; merged reads to 24 KiB. Source verification uses
the existing no-follow workspace reader. Overflow fails closed. These bounds supplement the engine's
context and journal quotas; the raw transcript still grows until its existing limit. This is not
conversation compaction.

## Queries and invalidation

Repository queries resolve the task publication and use an immutable snapshot selector. Federated
queries pin repository and memory through one captured runtime binding. Structural repository
responses use a deterministic FIFO cache: 64 entries, 256 KiB total, 32 KiB per entry. Keys include
request shape/cursor, effective budgets, policy/configuration, runtime binding, task view, immutable
snapshot, and status. Immutable facts bind their source identities through that snapshot. Verified
snippets and federated memory/link responses are not cached. Native content verification and exact
revision-pair linking remain authoritative.

Before reusing source evidence, the host compares its digest with a bounded read from the bound
workspace. Changed, deleted, renamed, unsafe, oversized, or unverifiable paths are replaced by an
explicit `evidence_unavailable` result. Patch targets invalidate prior evidence for those paths.
Shell execution and checks conservatively invalidate preceding evidence regardless of exit status.
Potentially active owned command writers prevent reuse. New publications, task views, or memory
revisions invalidate old packets. External edits are detected when referenced content is rechecked;
there is no claim to detect every new file or to prove repository-wide freshness. Without a reliable
comparison, freshness remains `unknown`; absent graph relationships remain unknown.

Mutations also clear structural query reuse and mark the overlay dirty. A host scheduler waits up
to 250 ms from the first pending edit, defers while owned commands may write, and starts at most
four refresh attempts per session. It uses the existing explicit refresh coordinator and sidecar
leases outside retrieval.
The first pending edit fixes the debounce deadline. A completed mutation arms a timer, so refresh
does not depend on another model turn. Bound graph queries wait for an armed refresh; while an
owned writer keeps the overlay dirty, they return a bounded pending result and the current
workspace fallback remains available. Before each working-set assembly, the host schedules any
refresh deferred by a finished writer and waits before selecting graph evidence or prefetch.
Shutdown stops and joins owned writers before scheduling and settling a remaining dirty refresh.
Results are bounded host observations. Failed refresh preserves the last snapshot and
overlay identities and may mark that view stale under the existing contract; it does not fail or
complete the task. No Git baseline or a disabled graph skips maintenance. Read-only retrieval
never invokes refresh.

Graph-disabled and unavailable-memory sessions can still use ordinary workspace tools. Stale,
ambiguous, missing, or unsupported graph coverage retains the explicit `repository_fallback` path.
There are no background watchers, automatic memory writes, or inferred canonical fallbacks.

## Optional prefetch and evaluation

`ferrus nano run --no-working-set` disables selection, structural query reuse, and scheduled refresh.
HQ enables these by default. For explicit managed-host experiments, repeat `--prefetch-path` or
`--prefetch-symbol` to provide at most eight task paths or exact graph symbol keys. Prefetch is off
by default and independent of `--no-working-set`; paths are never guessed from prose.
After a workspace mutation with the working set disabled, prefetch reports unavailable until a
new session; Nano does not schedule a refresh in this mode.

Each assembly makes one bounded native context request for those seeds: eight results, depth one,
8 KiB response, 4 KiB verified snippets, and a 250 ms query budget. Active writers defer it. The
request, response, and evidence handles are journaled as a host observation. Its provider-neutral
encoding is an explicitly labeled untrusted-evidence user message, limited to 24 KiB. It waits for
an armed overlay refresh and verifies each referenced source against current workspace bytes.
Unverifiable or changed sources make prefetch unavailable. No assistant
tool call is fabricated, and task/review artifacts are never edited. The next assembly re-resolves
the view and rechecks snippets instead of reusing an unverifiable source body. Missing coverage or
an optional prefetch failure leaves normal tools available.

Tests cover deterministic selection/replay, durable-before-inference ordering, independent disable,
source and publication changes, stale cursors, exact-source overlap, bounded caches, active writers,
debounce/cancellation, successful refresh, failed refresh, and opt-in host observations. Quality,
token savings, and runtime gains require the comparative evaluations in #85.
