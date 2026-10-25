# Nano headless evaluation

This is an opt-in evaluation workflow. CI uses a local mock provider and pinned
fixtures; it never starts LM Studio or makes paid inference calls. There are no
measured Nano-versus-external performance claims yet.

## Fixed workload and preparation

The seven source trees in [the fixture suite](../tests/fixtures/nano_eval/suite.json)
cover a local bug, a cross-file Rust change, a refactor, config/docs, Python
without Rust graph coverage, graph-disabled work, and stale-context recovery.
[Task text and checks](../tests/fixtures/nano_eval/tasks.md) live outside the
starting trees. A test recomputes every pinned Git tree ID on each platform.

Create one fresh project per case, variant, and sample. This command refuses an
existing destination and prints its verified starting tree:

```sh
python3 tests/fixtures/nano_eval/prepare.py local_bug_fix /absolute/new/eval-1
cd /absolute/new/eval-1
ferrus init
```

Set the case's check command in `ferrus.toml`. The canonical task artifact must
contain exactly the case's task sentence from `suite.json`, followed by one
newline. Start HQ with `FERRUS_NANO_EVAL_CASE=<case-id>` in its environment,
then create the task through `/task --manual` using that exact content. Ferrus
records the task and check digests and graph-index condition before spawning
the Executor. Let the configured Executor, Reviewer, and approval flow finish.
Do not count a submit, process exit, or
passing check alone as accepted. Keep each sample's `.ferrus` data directory
and `ferrus.db` until its report has been collected. The initial check fails
by design. The reviewer should inspect the resulting patch and run the case
check independently before approval.

## Variants

Keep the model ID/version, context/output limits, temperature, check command,
task text, permissions, and initial tree aligned. Record any unavoidable
difference in `harness_notes`. Use the same index and cache condition for
paired runs. `cold` means the enabled graph has no canonical snapshot before
the Executor starts; `warm` means it has a fresh published snapshot; `disabled`
means `[repository_graph].enabled = false`. An ambiguous or stale graph state
does not qualify for any label. Pre-index each fresh warm project before HQ
dispatch. Time initial `ferrus graph index --full`, later overlay refreshes,
and the complete task separately where available. Run repeated cold and warm
samples; use a fresh project for each cold sample. Report failures too.

| Report variant | Nano provider settings | Graph path |
| --- | --- | --- |
| `external_mcp` | Not applicable | External Executor with Ferrus MCP |
| `nano_mcp` | `native_context_enabled = false`, `working_set_enabled = false` | Explicit test MCP graph peer |
| `nano_native` | `native_context_enabled = true`, `working_set_enabled = false` | Native graph calls, no working set |
| `nano_working_set` | Both `true` | Native graph calls plus selection/reuse |
| `nano_graph_disabled` | Both `false` | Workspace tools only; no graph peer |

These settings belong in the owner-only provider file selected by
`FERRUS_NANO_CONFIG`, so HQ-managed launches receive them. The managed
`ferrus nano run` command also accepts `--no-native-context` and
`--no-working-set` as one-way overrides. Native prefetch requires native
context. `--no-native-context` hides native graph, memory, and fallback tools;
it does not remove external MCP tools. With the working set still enabled,
host-side evidence selection may use graph revisions, so the transport-only
comparison disables the working set too. Graph-disabled runs must also omit
MCP graph peers and disable `[repository_graph]` in `ferrus.toml`.

The `nano_mcp` case uses the explicit Ferrus graph peer in
[external stdio tools](ferrus-nano-mcp.md). It inherits only the managed
binding, invokes the same task view through Ferrus MCP, and normalizes the
three graph tool argument/result shapes for model input. Its `mcp_<peer-id>_*`
tool names, MCP transport cost, and any output mismatch still need disclosure
in `harness_notes`; exclude mismatched samples from a transport-only claim.

## Machine-readable report

After each attempt, record the database path, task and Executor run IDs,
measured wall time, and (for Nano) its `nano/sessions/<run-id>/events.jsonl`
journal. The reporter reads the database in read-only mode and replays the
bounded Nano journal. It requires the Executor's persisted run-start baseline
to match the case tree and rejects tasks with multiple Executor runs, whose
later worktree state may no longer match the pinned start. For Nano, the
journal's launch baseline, effective native-context and working-set flags,
discovered graph-peer mode and ID, and provider model must also match the manifest.
Every attempt needs the pre-spawn HQ record with the pinned task digest,
check-command digest, and observed cache condition. The submission transaction
records the final gate's command digest, which must match the pinned workload.
`nano_mcp` requires exactly one managed MCP peer exposing only the three Ferrus
graph tools; the other Nano variants require no MCP peers. Older runs without
this evidence cannot be used as pinned samples. The reporter rejects reuse of a database as another
sample, including under a different model or settings group. It reports
acceptance only when the task is `complete`
and the latest submission committed with the Reviewing transition belongs to
that attempt, in the current review cycle, with `check_gate: passed`. The
`submitted` and `approved` diagnostic events are best-effort and do not gate
acceptance.
Its grouped output includes sample counts and total-time min/p50/p95/max;
groups separate case, variant, cache state, model, and settings digest.

Example manifest for one Nano attempt (repeat `attempts` for other samples):

```json
{
  "version": 1,
  "attempts": [{
    "case_id": "local_bug_fix",
    "variant": "nano_native",
    "cache": "cold",
    "sample": 1,
    "start_tree": "fa7c122d4263bca6d5aa54df8ed7a437409b6237",
    "model": "exact-loaded-model-id",
    "settings_sha256": "<sha256-of-non-secret-effective-settings>",
    "native_context_enabled": true,
    "working_set_enabled": false,
    "database": "/absolute/project-data/ferrus.db",
    "task_id": "t-001",
    "run_id": "executor-run-id",
    "journal": "/absolute/project-data/nano/sessions/executor-run-id/events.jsonl",
    "external_usage": null,
    "timing": {
      "source": "wall-clock wrapper around the complete task",
      "total_ms": 12345,
      "index_ms": null,
      "refresh_ms": null,
      "peak_rss_bytes": null
    },
    "harness_notes": ["Same model and checks as paired external attempt"]
  }]
}
```

For Nano, copy `settings_sha256` from the journal's `Started.launch_evidence`.
Nano hashes a versioned serialization of the effective provider settings,
session limits, native-context and working-set flags, and graph-peer mode.
The baseline tree and API key are excluded. The reporter recomputes the hash
from the journal before grouping samples. For external runs, compute the digest
from an archived non-secret effective settings record; the reporter cannot
verify external settings. For `external_mcp`, set both ablation fields and
`journal` to `null`. Use a fresh Ferrus project database for every sample;
the reporter rejects database reuse even with different task or run IDs.
`external_usage` may be `null`, or may contain `source`, `input_tokens`,
`output_tokens`, `cached_tokens`, `cost_usd`, `model_turns`, and `tool_calls`
from the external harness. The reporter does not treat self-reported external
usage as a verified Nano measurement.

```sh
ferrus nano eval --manifest /absolute/evaluation-manifest.json > report.json
```

Nano input/output tokens are split into provider-reported and estimated
amounts, with any inherited budget excluded from the current attempt. Cached
tokens, cost, and peak RSS remain `null` unless measured by
the chosen provider/process wrapper. Duplicate source bytes count only exact
repeat `read_file` text ranges in one Nano journal; overlapping ranges and
external-tool source bytes are not inferred. Tool latency uses bounded journal
elapsed-time deltas. Context assembly latency is measured from before Nano
prepares the working set through request composition and is persisted with the
composition event. Older journals without that measurement report it as `null`.
Missing data stays `null`, and no percentage improvement is computed.

## Release gates and recovery

Run `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test`, then
the `nano-openai,nano-mcp` feature suite on the target platforms. The E2E mock
provider exercises edit, check, submit, external MCP, consultation/human
relaunch, and the independent ablation switches. The pinned-tree and report
tests reject drift and submit-only success. Existing external adapters run
through the unchanged Ferrus task/check/review workflow.

For an optional live LM Studio smoke test, load a tool-capable model at
`127.0.0.1:1234` and follow the ignored test command in
[the provider guide](ferrus-nano-provider.md). A live full-suite comparison
needs a running provider and explicit external Executor selection. On an
interrupted Nano attempt, retain its journal and Ferrus database; HQ relaunch
uses the [recovery contract](ferrus-nano-sessions.md), and a report must keep
the failed or unresolved attempt rather than selecting the fastest survivor.
