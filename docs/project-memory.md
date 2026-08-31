# Project Memory

Ferrus project memory is a local, rebuildable index of curated project history. It is stored in
`project-memory.db` beside `ferrus.db`, but it is not orchestration state. Removing the sidecar does
not remove tasks, runs, archives, specifications, or repository graph snapshots.

Project memory works offline. Indexing and retrieval do not call a network service, an LLM, or an
embedding model.

## What is indexed

The default policy enables four source categories:

| Source category | Content boundary | Sensitivity |
| --- | --- | --- |
| Specification structure | Tracked headings and milestone metadata | Curated |
| Approved outcome | The approved `## Outcome` section | Curated |
| Archive manifest | Archive identity and counts | Operational metadata |
| Runtime provenance | Terminal task, run, milestone, status, and check identities | Operational metadata |

Raw task descriptions, submissions, reviews, patches, check logs, questions, answers,
consultations, and integration-error bodies are disabled and treated as sensitive. Their presence
under `.ferrus/` or in an archive does not authorize ingestion.

Every returned memory fact includes a portable source locator, source fingerprint, extractor
identity, evidence, resolution state, confidence, and timestamps. Repository links are confirmed
only from explicit path, symbol, task, milestone, or archive evidence. A missing or ambiguous link
is `unresolved`; a previously resolved target missing from the selected repository snapshot is
`stale`. Neither state is presented as a current-code relationship.

## Build and inspect memory

Enable the repository graph first, because the same strict graph configuration provides local
query limits and repository link resolution:

```toml
[repository_graph]
enabled = true
```

Then build and inspect project memory:

```sh
ferrus graph memory index
ferrus graph memory status
ferrus graph memory status --json
```

Indexing is incremental. Unchanged authorized sources reuse cached fragments, and a no-change run
does not advance the published generation. Publication is atomic: a failed or interrupted build
leaves the prior revision readable.

Use `--full` to bypass fragment reuse. If status reports incompatible storage, `--full` also
replaces the rebuildable memory sidecar:

```sh
ferrus graph memory index --full
```

Status reports source policy, freshness, current fact counts, stale links, and retention counts.
Retention counts include total and historical revisions, terminal builds that do not back the
current publication, and repository link sets. They are diagnostics only; Ferrus does not silently
delete historical memory revisions. If the local sidecar grows unexpectedly, a full rebuild is the
explicit cleanup path.

## Query memory and current source

Repository-only behavior remains the default:

```sh
ferrus graph search RuntimeTaskContext
```

Select memory or explicit federation when historical context is wanted:

```sh
ferrus graph search "bounded retrieval" --domain memory
ferrus graph context --milestone rg4.6 --domain memory
ferrus graph context --milestone rg4.6 --domain all
```

`--domain all` does not merge storage or turn history into source fact. It reports the repository
snapshot and memory revision independently and crosses domains only through the exact link set for
those revisions. Current repository evidence wins equal-rank deterministic tie-breaking, so
historical context supplements rather than displaces current source.

Supervisor and Executor agents have the same read-only flow through `project_memory_status`,
`project_context_search`, and `project_context`. MCP requests must state `repository`, `memory`, or
`all` explicitly. These tools never build an index or author an Outcome.

## Freshness and archive lifecycle

Specification archive is the only workflow that writes approved `## Outcome` content. After the
archive transaction commits, Ferrus attempts an incremental memory refresh. The refresh is outside
the archive critical path: failure cannot undo or fail a successful archive.

Repository and memory freshness are independent. After an authorized source changes, queries keep
the last published revision readable but report memory as stale and recommend:

```sh
ferrus graph memory index
```

CLI status compares the current authorized source manifest exactly. Latency-bounded MCP retrieval
may report freshness as unknown when it cannot perform that comparison safely. Unknown is not a
claim that the revision is fresh.

## Operational checks

- Use JSON status to audit all enabled and disabled source categories and their sensitivity.
- Treat stale or unresolved repository links as historical evidence only.
- Do not use `project-memory.db` as a backup; rebuild it from tracked specs and registered runtime
  metadata.
- Keep approved Outcomes concise and free of secrets. Curated does not mean public.
- Use the reproducible evaluation in
  [`project-memory-evaluations.md`](project-memory-evaluations.md) after changing extraction,
  ranking, privacy policy, or federation behavior.
