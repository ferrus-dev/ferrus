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
- Default limits are 4 MiB wire bytes per attempt, 256 KiB per SSE event, 64 tool calls,
  and 120 seconds per HTTP request. The request timeout covers headers and body;
  connection establishment is capped at the smaller of 10 seconds and that timeout.
  The engine's independent byte, time, turn, token, and tool budgets still apply.
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
  provider settings. Model failure records contain only typed error codes. Existing
  version-1 journals without these optional fields remain readable.

Sources: [LM Studio Chat Completions](https://lmstudio.ai/docs/developer/openai-compat/chat-completions),
[LM Studio tool use](https://lmstudio.ai/docs/developer/openai-compat/tools), and
[Chat Completions streaming schema](https://developers.openai.com/api/reference/resources/chat/subresources/completions/streaming-events).

## Verification

Deterministic fixtures and loopback HTTP tests require no model, credentials, or paid
inference. They exercise anonymous/Bearer requests, ordered calls and continuation,
partial streams, usage, retry accounting, context admission, timeout, and cancellation.

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
Live compatibility has not yet been verified; run it against the configured model
before enabling that endpoint for managed execution.
