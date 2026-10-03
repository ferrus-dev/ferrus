# Nano Chat Completions Provider

The first inference adapter targets **LM Studio's OpenAI-compatible
`POST /v1/chat/completions` streaming API**. It is opt-in through the `nano-openai`
Cargo feature. This implements #75; [CLI/HQ launch](ferrus-nano-launch.md) is implemented in #80.
An OpenAI-compatible label alone does not establish endpoint or model compatibility.

## Host configuration

The native launcher explicitly loads `nano::config::Config` and constructs `OpenAi`.
Selecting Nano in HQ or registration validates its host-local settings and credential.
Other Ferrus backends, MCP, graph, and memory paths do not initialize this provider.
No model is selected automatically.

Keep the settings file outside the project, for example in your private Ferrus
configuration directory. The loader opens inputs read-only and requires an existing
owner-only file (0400 or 0600 on Unix; protected owner-only DACL on Windows). Example:

```toml
base_url = "http://127.0.0.1:1234/v1"
model = "your-loaded-tool-capable-model"
context_tokens = 32768
max_output_tokens = 4096
temperature = 0.0
request_timeout_ms = 120000
include_usage = true

# Optional; omitted by default. Endpoint/model support determines valid levels.
# reasoning_effort = "none"

# Independent evaluation ablations; both default to true.
# native_context_enabled = false
# working_set_enabled = false

# Optional: omit entirely when LM Studio authentication is disabled.
# This file contains only the token, not a shell assignment or JSON object.
# api_key_file = "/absolute/private/host/path/lm-studio-token"
```

The key file must also be owner-only, at most 4096 bytes, and contain a nonempty
single token. It is read directly into a sensitive Authorization header. With no
key file, requests omit Authorization entirely; no dummy key is necessary.
The adapter neither reads API keys from environment variables nor exports them to
tool/MCP child environments. It never stores key contents or credential paths in
session records. Unknown settings, including inline `api_key`, fail explicitly.

HTTP is permitted only on loopback; other hosts require HTTPS. The base URL must
use `/v1` and cannot contain userinfo, a query, or a fragment. Redirects, automatic
HTTP retries, and environment proxy discovery are disabled. Authentication and
unsupported endpoint errors do not trigger protocol fallback.

`reasoning_effort` forwards an explicit Chat Completions reasoning level: `none`,
`minimal`, `low`, `medium`, `high`, `xhigh`, or `max`. Omit it to use the server's
default. An endpoint may reject levels it does not support; Nano does not silently
substitute a different level. The effective value is recorded in the journal and
included in the settings digest. For a local thinking model, `none` can disable
reasoning when supported, allowing the output budget to go to tool calls and text.

LM Studio normally allows unauthenticated requests; its authentication option uses
Bearer tokens. See [LM Studio authentication](https://lmstudio.ai/docs/developer/core/authentication).

## Protocol and budgets

- Text-only Chat Completions messages, JSON function schemas, one choice, `max_tokens`,
  and optional `stream_options.include_usage` are supported. A tool-capable loaded
  model is required. Responses API, audio, images, deprecated function calls,
  refusal/content-filter completion, and unknown delta features fail explicitly.
- SSE framing supports LF/CRLF, split UTF-8, split function names/arguments, multiple
  indexed calls, comments, and a separate trailing usage event. Calls become executable
  only after a finish reason **and `[DONE]`**. EOF before that is a failed attempt.
- `reasoning_content` and `reasoning` string deltas are preserved as opaque continuation
  data and projected back unchanged. Other signed/reasoning formats are unsupported;
  the adapter does not silently discard them.
  Reasoning consumes the same `max_output_tokens` allowance as visible text and tool arguments.
  A thinking model can exhaust the default 4096 tokens without producing a call. A `length`
  finish fails the managed work phase as `nano_provider_truncated`; raise the output allowance
  and configure `context_tokens` to match the server's loaded context, or adjust thinking on
  the server. Nano never retries a truncated response as though it were a complete tool call.
- Default limits are 4 MiB wire bytes per attempt, 256 KiB per SSE event, 64 tool calls,
  and 120 seconds of HTTP read inactivity. `request_timeout_ms` bounds waiting for
  response headers and each subsequent read; receiving bytes resets that timeout.
  An active stream can exceed it while the model generates a long response or patch;
  connection establishment is capped at the smaller of 10 seconds and that timeout.
  The engine's independent byte, total elapsed time, turn, token, and tool budgets still apply.
- Context admission conservatively counts serialized request bytes as tokens and
  reserves output space. Configure `context_tokens` to match the loaded model's
  actual context. Known server context-overflow codes end with `Limit(ContextTokens)`;
  automatic compaction is later work. Set `include_usage = false` explicitly if the
  chosen server does not support streamed usage; absent usage is estimated by the engine.
- 429, transient 5xx/transport errors, timeout, and incomplete streams use the engine's
  existing retry budget. Backoff starts at 500 ms and is capped at 30 seconds, including
  numeric `Retry-After`. Waiting is cancellable and consumes the same elapsed allowance.
  Failed requests consume their reserved input/output budget as estimates; completed
  provider usage remains separate. No adapter-local retries can duplicate effects.
- Started records contain the validated model, API identity, base URL, and effective
  provider settings. Model failure records contain typed error codes and optional
  non-secret transport diagnostics: HTTP status, unexpected content type, or SSE failure.
  The same evidence is logged to stderr and captured in HQ's scoped agent log, without
  response bodies, headers, or credentials. For HTTP 400/422, check the server's request
  validation diagnostics, including the selected model and tool schemas. Existing
  version-1 journals without these optional fields remain readable.

Sources: [LM Studio Chat Completions](https://lmstudio.ai/docs/developer/openai-compat/chat-completions),
[LM Studio tool use](https://lmstudio.ai/docs/developer/openai-compat/tools), and
[Chat Completions streaming schema](https://developers.openai.com/api/reference/resources/chat/subresources/completions/streaming-events).

## Verification

Deterministic fixtures and loopback HTTP tests require no model, credentials, or paid
inference. They exercise anonymous/Bearer requests, ordered calls and continuation,
partial streams, usage, retry accounting, context admission, timeout, and cancellation.

To replay a recorded timeout without executing tools, set `FERRUS_NANO_SMOKE_CONFIG` and
`FERRUS_NANO_SMOKE_JOURNAL`, then run the ignored
`live_lm_studio_replays_a_timed_out_request_without_executing_tools` test. It uses only a
loopback endpoint, reconstructs the journal's full context projection, and verifies the current
native catalog against the recorded request size. `FERRUS_NANO_SMOKE_OUTPUT_TOKENS` optionally
changes the request's output allowance; the private smoke config may also override reasoning
effort. `FERRUS_NANO_SMOKE_REQUEST_OUTPUT` exports the reconstructed request to a new private
file instead of contacting the model. Server debug logs may truncate
message bodies and cannot substitute for the durable journal when replaying a request.

```sh
cargo test --locked --features nano-openai nano::
cargo clippy --locked --features nano-openai -- -D warnings
cargo test --locked --no-default-features --features nano-openai nano::
```

For the live smoke test, start LM Studio on `127.0.0.1:1234`, load a model supporting
tool calls, and prepare the private host settings file with its exact model ID:

```sh
FERRUS_NANO_SMOKE_CONFIG=/absolute/private/host/path/nano.toml \
  cargo test --locked --features nano-openai live_lm_studio_tool_session -- --ignored --nocapture
```

The test runs a bounded session with a pure `lookup` tool, requires a successful tool
call before final completion, and reports the configured model and reported/estimated
usage. The temporary journal is removed after the test. CI never runs this test.
Live compatibility is model-specific; run this test against the configured model
before enabling that endpoint for managed execution.

For a native managed smoke with graph retrieval, a file edit, check, and submission:

```sh
FERRUS_NANO_SMOKE_CONFIG=/absolute/private/host/path/nano.toml \
  cargo test --locked --features nano-openai live_lm_studio_managed_graph_edit_check_and_submit -- --ignored --nocapture
```

This test creates a temporary SQLite project and Git repository, indexes its baseline,
and runs a small rename task under a three-minute session allowance. It requires native
`repository_search`, `apply_patch`, `check`, and `submit` calls, the expected source change,
and a durable `Submitted` outcome with the task in Reviewing. External MCP peers are
excluded. The configured check runs only inside the temporary workspace. CI ignores
this test; it does not certify a particular real task or model's general coding quality.
