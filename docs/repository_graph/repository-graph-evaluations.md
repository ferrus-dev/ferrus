# Repository Graph Navigation Evaluations

This document defines the deterministic RG2 navigation evaluation and records the current usefulness decision.
The harness exercises the real filesystem source adapter, index coordinator, SQLite query implementation, search,
context assembly, freshness comparison, and missing-sidecar behavior. It does not execute an LLM, so timings are
retrieval/navigation proxies rather than end-to-end agent task durations.

## Running the evaluation

```sh
cargo run --example repository_graph_eval -- --output /tmp/ferrus-rg2-eval.json
```

The command exits unsuccessfully when a required quality gate fails. Its versioned JSON report contains every case,
expected and returned paths, graph-assisted and baseline metrics, cold/warm latency arrays, response-size arrays,
gate observations, the deterministic fixture digest, and an automation recommendation. Cross-machine latency is
recorded but is deliberately not compared with a fixed wall-clock threshold.

`cargo test --test repository_graph_eval` runs the same corpus in the normal test gate and verifies the stable
quality measurements. The corpus lives in `tests/fixtures/repository_graph_eval/cases.json`; the shared runner is
under `tests/support/`, so evaluation code and fixtures do not become part of the production `ferrus` binary.

## Corpus and baseline

Corpus `rg2.5-v1` contains 26 labeled cases covering:

- exact paths and exact unique symbols;
- ambiguous symbols and supported discovery;
- resolved dependencies, documentation, configuration, and malformed source;
- unsupported macro/comment-only capability;
- missing and stale indexes;
- result truncation and repeated-query determinism.

The fixture includes representative Rust modules, imports, duplicate names, Markdown, TOML, an executable entry
point, malformed Rust, an intentionally unsupported comment-only concept, and unrelated filler files. Its digest
includes both corpus JSON and all fixture paths/bodies.

Graph-assisted cases issue one bounded graph operation and read zero source files unless a future case explicitly
requests snippets. The deterministic baseline scans repository-relative paths/source files until all labeled
relevant paths are found. Both sides record success, time to first relevant result, tool calls, files read, context
bytes, graph-query bytes, and total measured duration.

## Quality gates and current result

The current deterministic run passes every initial RG2 gate:

| Gate | Threshold | Observed |
|---|---:|---:|
| Exact path Recall@1 | 100% | 100% (4/4) |
| Supported exact unique-symbol Recall@1 | 100% | 100% (4/4) |
| Supported discovery Recall@10 | at least 90% | 93.75% (15/16) |
| Repeated same-snapshot semantic determinism | 100% | 100% (2/2) |
| No correctness regression versus supported navigation baseline | 100% | 100% (21/21) |
| Median files-read or context-byte reduction | at least 20% | 100% files-read reduction |

The same navigation subset currently has a negative median context-byte reduction (approximately `-772%`): broad
`direction=both` context expansion can return substantially more serialized graph evidence than the baseline source
scan, even though it avoids source-file reads. This is a real limitation, not a gate failure, because the normative
threshold is files read **or** context bytes. It should guide ranking/policy work before any automatic injection.

## Product decision

The passing result makes stronger *optional* status/search guidance eligible. It does not enable automatic context
injection: that remains outside Phase 2 and would be premature while context-byte volume is high and task worktree
views are unavailable. The compact `ferrus://repository/summary` resource also remains unregistered; the evaluation
provides no evidence that duplicating status in every resource consumer would reduce navigation cost.

Future ranking or extractor revisions must rerun this corpus. A failed gate changes the report recommendation to
`keep_stronger_guidance_disabled`; raw latency distributions remain diagnostic until stable platform-specific
budgets are established.
