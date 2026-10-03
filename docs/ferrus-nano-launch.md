# Nano headless launch and HQ events

Status: Nano is an opt-in headless Executor backend. Interactive sessions,
Supervisor support, and standalone packaging are deferred. Live crash recovery
and reconciliation are implemented for #84.
The process lifecycle is tested with a local mock API; live LM Studio validation remains opt-in.

## Configure and select

Prebuilt release archives include `nano-openai` and `nano-mcp`. For a Cargo installation from
crates.io, select the alpha version explicitly:

```sh
cargo install ferrus --version 0.5.0-alpha.1 --locked --profile dist --features nano-openai,nano-mcp
```

For a source build, use `cargo build --features nano-openai,nano-mcp` (or only `nano-openai` if
external MCP tools are not needed). Cargo's default feature set does not include Nano.

For a local LM Studio server on `http://127.0.0.1:1234`, register the exact loaded model ID:

```sh
ferrus register --executor nano --executor-model YOUR_LOADED_MODEL_ID
ferrus
```

Replace `YOUR_LOADED_MODEL_ID` with the model ID shown by LM Studio.
On first registration, Ferrus creates an owner-only provider file at
`<user-home>/.ferrus/nano.toml`, using the same home-directory resolution as the Ferrus
project registry. On Windows this is the user profile directory (for example,
`C:\Users\Alice\.ferrus\nano.toml`), not a literal `~` path; registration prints the
resolved path.
It contains the local `/v1` endpoint and the selected model. Ferrus does not contact the
provider during registration, and it never replaces an existing file. A model ID is required
when creating the file but is not stored as a separate HQ override. On later registrations,
omit `--executor-model` to clear any Nano override and use the file's model.

For a different endpoint, credentials, or other provider settings, edit the generated file or
set `FERRUS_NANO_CONFIG` to an absolute owner-only file before registration and HQ launch.
The [provider contract](ferrus-nano-provider.md) describes the fields:

```toml
base_url = "http://127.0.0.1:1234/v1"
model = "your-loaded-model"
# Optional; omit for a local server without authentication.
# api_key_file = "/absolute/private/path/lm-studio-key"
```

Replace `your-loaded-model` with the exact ID of the loaded model. Save the file outside the
repository. On Unix, use mode 0400 or 0600; Windows requires a protected owner-only DACL.
For an explicit path, set `FERRUS_NANO_CONFIG` in the same shell that runs both registration
and HQ:

```sh
export FERRUS_NANO_CONFIG=/absolute/private/path/nano.toml
ferrus register --executor nano
ferrus
```

To override an existing file's model, pass `--executor-model <model-id>` during registration.
The environment variable must remain set when HQ starts. Registration and launch validate the
settings and optional credential without contacting the provider.

Registration stores `[hq.executor]` in `ferrus.toml`. It creates no Ferrus MCP entry for Nano.
Existing external adapter registration is unchanged. Model overrides are trimmed; without
an override, Nano uses the model in its provider file. HQ displays the same selection.
`ferrus nano --version` reports the bundled Ferrus version without loading provider settings.

Use the ordinary HQ task/run workflow. `/executor` interactive launch and Supervisor selection
are rejected. A build without `nano-openai` reports the missing feature before task setup.
HQ checks launch configuration before preparing a worktree or consuming a dispatch attempt.

## Process protocol

HQ invokes `ferrus nano run --config <absolute-path> [--model <model>]` in its prepared workspace.
This is a managed entry point: it requires the existing `FERRUS_PROJECT_ROOT`, `FERRUS_AGENT_ID`,
`FERRUS_TASK_ID`, `FERRUS_RUN_ID`, and, for Git workspaces, `FERRUS_BASELINE_TREE` binding.
It does not allocate tasks, worktrees, or runs itself.

Stdin stays open for UTF-8 JSONL commands. Each newline-terminated frame, including its newline,
is at most 4,096 bytes. Unknown versions, fields, commands, malformed JSON, oversized frames,
and incomplete final frames fail explicitly. One start is accepted; subsequent input may cancel.
EOF after start also cancels. Closing stdin immediately after start is not a batch-run interface.

```json
{"version":1,"command":"start"}
{"version":1,"command":"cancel"}
```

Stdout carries only versioned JSONL events. Diagnostics use stderr, including in debug mode.
Tool and configured-check output stays in bounded tool/check storage and cannot become a frame.
The child announces readiness after configuration validation, then waits up to 30 seconds for
start. HQ sends start only after persisting the run and registering the process.

```json
{"version":1,"event":{"type":"ready"}}
{"version":1,"event":{"type":"progress","sequence":1,"phase":"started"}}
{"version":1,"event":{"type":"ended","reason":{"reason":"submitted"},"durable":true}}
```

Progress phases are `started`, `model`, `model_failed`, `model_truncated`, `tool`, and `tool_finished`;
optional typed details identify the model turn, tool name, or provider error kind.
Older version-1 events without details remain readable. Errors carry a bounded code.
They contain no task, model response, command output, question, or credential bodies. Progress
sequence numbers refer to durable journal records and may skip: delivery coalesces to one pending
event. A dedicated writer performs stdout I/O without blocking inference, lease renewal, or journal
commits. Shutdown allows two seconds to drain output after owned effects settle; an unresponsive
frontend may miss the last event. The journal and SQLite remain authoritative.

HQ shows the usual process-start announcement. With `--debug`, it also shows tool requests,
model failures, output-limit continuations, and terminal outcomes. The scoped agent log retains
all progress and terminal events in both modes, including internal model-start and tool-finish
markers.

## HQ ownership

HQ preserves worktree and baseline preparation, process guards, scoped logs, dispatch accounting,
and recovery. Startup errors stop and reap the child without charging a dispatch. Debug mode keeps
the native pipes and handshake instead of redirecting stdout to a raw text log. Native diagnostics
are bounded to 4 KiB per line and 256 KiB per session log; progress is rendered as coarse transcript
messages, with one latest event per process between scheduler ticks.

Task status and human questions continue through HQ's SQLite/artifact watcher. An event or model
sentence never marks a task complete. `/stop` first sends cancel and allows a two-second graceful
cleanup interval, then uses the existing process-group termination fallback. Review, approval,
consultation scheduling, and crash recovery remain owned by HQ.

A non-retryable provider failure stops owned effects, seals the session journal, and marks the
bound working task Failed with `nano_provider_protocol` or `nano_provider_failed`. Nano exits
with a nonzero status while preserving its structured `ended` event. HQ does not respawn that
task until a new work phase is explicitly created. Recovery reconciles a sealed provider stop
if the process exited before recording task failure. Transient provider errors retain their
existing retry behavior; cancellation and paused tasks do not become provider failures.
A response ending with `finish_reason = "length"` continues in the same session;
partial tool calls never execute. Nano retains text/reasoning and asks for the next action
or a fresh complete call. HQ reports that continuation, and diagnostics record the effective
output allowance. Reasoning shares that allowance with the answer and tool arguments.
`max_output_tokens` is an optional ceiling; omit it for automatic allocation from the
configured context and session budgets. Historical `nano_provider_truncated` terminal stops
remain readable and do not requeue an already failed task.
`session_tokens` controls cumulative input and output across the entire work phase, including
repeated prompts, retries, and summaries. Its default is 1,000,000 tokens. It is independent of
`context_tokens`, which limits one request, and `max_output_tokens`, which caps one response.
For example, add `session_tokens = 2000000` to the private provider file for longer tasks.
This remains a hard budget: admission reserves the conservatively estimated next input and
output before inference, so Nano can stop before reported usage reaches the configured total.
The terminal diagnostic reports consumption and the limit. A Failed task requires a new work
phase; changing settings does not silently restart it or erase recorded usage.
Exhausted token, model-turn, tool-call, retry, and no-progress budgets also fail the working
task after cleanup and journal sealing, with a matching `nano_limit_*` code and nonzero exit.
These counters belong to the work phase; HQ cannot replenish them by restarting Nano.
Recovery reconciles a sealed budget stop before allowing another inference. Interrupted
runs continue through ordinary effect reconciliation; elapsed time keeps its per-run allowance.
Inspect `Nano diagnostic` entries in `.ferrus/logs/executor_<task>_<timestamp>_<run>.log` and
`nano/sessions/<run>/events.jsonl` under the machine-local project data directory.

When HQ relaunches an answered human or consultation waiter, Nano derives the launch action from
its bound SQLite task instead of an external-agent prompt. Human waits require the question's
Executor; neither wait can take another agent's live lease or resume a non-Executor phase.
The host checks cancellation and context capacity before consuming the answer, then restores
Executing or Addressing and includes the human answer or Supervisor response in the first model input.
Missing answers or failed delivery checks leave the task waiting. This starts a fresh session;
recovery uses the #84 journal/effect reconciliation path when a prior run was interrupted.

Working-set selection and native context tools are enabled by default. The owner-only provider
file accepts `working_set_enabled` and `native_context_enabled` for managed evaluations. The
managed `nano run` entry point also accepts `--no-working-set`, `--no-native-context`, and
optional explicit `--prefetch-path` / `--prefetch-symbol` seeds;
see the [working-set contract](ferrus-nano-working-set.md). These options do not change HQ task
ownership, provider configuration, or the start/cancel protocol.
