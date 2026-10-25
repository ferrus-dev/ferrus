# Nano instructions and native context

Status: implemented for #78. Managed lifecycle is implemented in #79; HQ launch is wired in #80.

`NativeTools` composes the bounded workspace and command tools with instruction loading and
seven context operations. It does not expose the entire Ferrus tool catalog. The host supplies
one validated `FerrusSession` and coding tools for that session's workspace; model arguments
cannot select a project, agent, task, run, database, baseline, or execution backend.

## Instructions

`Instructions::load(paths, skills)` rereads the current task and applicable guidance. Paths are
intended workspace file targets, including files that do not exist yet. For `src/api/new.rs`,
it loads root `AGENTS.md`, `src/AGENTS.md`, and `src/api/AGENTS.md` when present. It does not load
sibling directories. A selected skill name such as `rust` resolves only to
`.agents/skills/rust/SKILL.md`; neither a skill catalog nor unselected skill bodies are loaded.

Each document carries its kind, origin (`host`, `project`, or `workspace`), relative source path,
applicable directory, SHA-256 digest, and text. The host policy and current task precede supporting
guidance; nested guidance applies within its directory. Supporting documents never grant runtime
authority. Task intent comes from the exact canonical `.ferrus/tasks/<task-id>.md` artifact.
Addressing sessions and sessions with review history require the scoped `REVIEW.md`. Missing or
unsafe required input fails instead of silently omitting a constraint.

`InstructionSet::constraint_text` creates a bounded constraint block for the host's initial
context. `load_instructions` makes explicit reloads available as a tool. Every load returns a
replacement set, so edits and deleted optional guidance do not accumulate stale instructions.
The host must retain this entire set through later context projection or reject the projection;
the [working set](ferrus-nano-working-set.md) preserves it during turn-time evidence selection.
Token-budgeted compaction remains #82.

Defaults are 32 KiB per file, 96 KiB per encoded set, and 32 documents. Selection is limited to
16 file targets, 16 path components, and eight skills. Limits are host-configurable within hard
bounds. Reads reuse no-follow workspace traversal and reject symlinks, reparse points, hardlinks,
non-regular files, invalid UTF-8, and NUL bytes. Context overflow fails closed. The tool boundary
also enforces a 32 KiB encoded result cap; a larger host-loaded set may not fit a tool response.

## Retrieval

| Tool | Native behavior |
| --- | --- |
| `repository_graph_status` | Bound task view availability, baseline/overlay, snapshot and freshness |
| `repository_search` | Bounded text, repository path and node-kind search |
| `repository_context` | Structural context from node, symbol or path seeds; optional verified snippets |
| `project_memory_status` | Independent memory revision, freshness and source policy |
| `project_context_search` | Explicit `repository`, `memory`, or `all` search |
| `project_context` | Explicit-domain context with evidence-backed cross-domain links |
| `repository_fallback` | Explicit bounded workspace read/search with a coverage reason |

Queries use `LocalGraphContext` and `LocalProjectContext` directly. Responses retain the typed
domain payload up to final model-tool encoding; no MCP server/client, subprocess, or serialized
MCP response parsing is involved. The native response envelope identifies the operation and
preserves domain query errors. The first surface includes direction, unresolved/stale inclusion,
source snippets, cursors and budgets; additional filter options can be added without changing the
underlying adapters.

Native request caps are 64 results, 24 KiB of query data, depth eight, one second, 16 diagnostics,
and eight KiB of snippets. Existing server limits can tighten them. Final encoded tool results
are capped independently at 32 KiB, including response envelopes. Oversized results return an
output-limit error. Freshness remains conservative: a latency-bounded read does not claim a live
workspace scan. Repository and memory freshness/revisions remain independent.

Each operation revalidates the exact managed run. Graph adapters receive explicit root, project,
data directory, and task-view inputs; changing process cwd or ambient agent identity cannot
retarget them. Invalid runtime routing fails before retrieval or fallback. The task baseline and
last published overlay determine the repository view; a new canonical publication does not
retarget the task. Source snippets retain the adapters' snapshot/hash and memory-policy checks.
Memory-only operations do not require a valid or enabled repository graph.

Reads never build or migrate sidecars, author project memory, change task/run lifecycle or events,
or inject retrieved content into persisted task/review artifacts. Missing graph relationships are
unknown, not proof of absence.

## Workspace fallback

For incomplete coverage, use `repository_fallback` with `operation` (`read` or `search`), `input`
(the corresponding workspace tool arguments), and `reason` (`missing`, `disabled`, `stale`,
`ambiguous`, or `unsupported`). This is an explicit selection, not a semantic equivalence between
graph search and text search. No automatic broad repository scan runs after a failed query.

The response has `kind: workspace_fallback`, the caller's `requested_reason`, the observed graph
status, and bounded workspace evidence. The reason is not a claim that the adapter diagnosed that
condition. File evidence retains workspace identity, generation, path and digest; it is never
promoted to a graph fact. Workspace protected-path/content rules still apply. Routing/configuration
failures remain errors and cannot trigger a canonical fallback.
