# Errors, symptoms and fixes

## The error model in one table

| Handler kind | An `Err` becomes |
|---|---|
| `#[tool]` | A **tool error** — a successful JSON-RPC response with `is_error: true`. The model reads it and can retry or fall back |
| `#[prompt]` | A JSON-RPC error. The request fails |
| `#[resource]` | A JSON-RPC error. The request fails |
| `#[handler]` | Whatever your return type maps to |

That difference is deliberate: a tool is something a model *tries*, so its
failures are content. A prompt or resource read that fails is a protocol
failure.

```rust
use neva::prelude::*;

#[tool(descr = "Reads a record")]
async fn get_record(id: String) -> Result<String, Error> {
    if id.is_empty() {
        // A tool error the model can recover from.
        return Err(Error::new(ErrorCode::InvalidParams, "id must not be empty"));
    }
    Ok(format!("record {id}"))
}
```

`?` works with anything that is `Into<Error>` — `serde_json::Error`,
`std::io::Error` and friends convert already.

To stay on the response path instead — note that
`CallToolResponse::error` takes an `Error`, and that the return type has to
be a `Result` for the macro not to demand an `outputSchema` (see
`server.md`):

```rust
use neva::prelude::*;

#[tool(descr = "Searches the catalog")]
async fn search(query: String) -> Result<CallToolResponse, Error> {
    if query.is_empty() {
        return Ok(CallToolResponse::error(
            Error::new(ErrorCode::InvalidParams, "empty query"),
        ));
    }
    Ok(CallToolResponse::new(format!("results for {query}")))
}
```

## Error codes

| Variant | Code | Meaning |
|---|---|---|
| `ParseError` | -32700 | Malformed JSON |
| `InvalidRequest` | -32600 | Not a valid JSON-RPC object |
| `MethodNotFound` | -32601 | Unknown method or unregistered tool |
| `InvalidParams` | -32602 | Missing or wrongly typed params |
| `InternalError` | -32603 | Unexpected server-side failure |
| `HeaderMismatch` | -32020 | A routing header disagrees with the body |
| `MissingRequiredClientCapability` | -32021 | The request needs a capability the caller did not declare |
| `UnsupportedProtocolVersion` | -32022 | The named protocol version is not supported |
| `UrlElicitationRequiredError` | -32042 | The interaction requires URL elicitation |

The three MCP 2026-07-28 codes answer HTTP `400` and carry structured
`data`:

```rust
use neva::prelude::*;
use serde_json::json;

fn main() {
    let err = Error::new(ErrorCode::UnsupportedProtocolVersion, "unsupported version")
        .with_data(json!({
            "supported": ["2026-07-28"],
            "requested": "2025-06-18"
        }));
    let _ = err;
}
```

`HeaderMismatch` carries none, `MissingRequiredClientCapability` carries
`requiredCapabilities`, `UnsupportedProtocolVersion` carries `supported`
and `requested`.

**`ResourceNotFound` is deprecated.** MCP 2026-07-28 dropped the dedicated
`-32002`; "resource not found" is `InvalidParams` now. Use the
version-dependent constant so the wire code follows the active generation:

```rust
use neva::prelude::*;

fn main() {
    let err = Error::new(ErrorCode::RESOURCE_NOT_FOUND, "no such resource");
    let _ = err;
}
```

## Symptom → cause

### The server panics at startup naming an argument

`App::run` refuses to start when a tool or prompt publishes arguments its
handler does not read — a wrong count of declared names, a duplicate name,
or a schema property the handler never looks for. The message names the
primitive and the argument.

Cause: almost always a bare closure registered with `map_tool` (which
publishes `arg0`, `arg1`, …) or a hand-written `input_schema` whose
property names differ from the parameter names. Fix with `map_tool!`,
`.with_arg_names([...])`, or by renaming the schema properties. See
`server.md`.

### `-32602 invalid type: map, expected a boolean`

An old neva reading a conformant client's per-request capabilities. Upgrade
to 0.5.2 or later; both shapes are accepted there.

### `HeaderMismatch` (-32020) out of nowhere

A proxy in front of the server is rewriting or injecting `Mcp-Method`,
`Mcp-Name` or `Mcp-Param-{name}`. Those must mirror the body exactly, and
must not be present at all on a batch. neva builds them correctly, so
suspect the intermediary.

It also fires when a body's protocol version disagrees with the
`MCP-Protocol-Version` header, and when a client's cached `x-mcp-header`
annotations have expired — in that last case the client re-lists and
retries once by itself.

### `MissingRequiredClientCapability` (-32021)

A handler asked for an input kind the caller did not declare in *this*
request's `_meta`. Either the client should declare it
(`with_elicitation(|e| e.with_form().with_url())`) or the handler should
check `ctx.client_capabilities()` first and take another path. Note that
elicitation is reported down to the mode — a client that declared `form`
must not be sent a `url` request.

### A tool is missing from `tools/list`

An `x-mcp-header` annotation that breaks the spec's constraints drops the
**whole tool** from the listing, deliberately: a non-token name, a
duplicate, a non-primitive type, or a property not statically reachable
through `properties`.

### The elicitation flow charges twice / repeats side effects

The handler re-runs from the top on every MRTR round. Anything externally
visible above an elicit point must be wrapped in `ctx.memo`, `ctx.once` or
`ctx.on_commit`. See `mrtr.md`.

### Notifications never arrive over HTTP

There is no standalone SSE `GET` stream in this generation. The client must
open one with `Client::listen(filter)`, *after* `connect()` and *after*
registering the handlers. Also check the server actually advertises the
category — an unadvertised one is silently dropped from the acknowledgment,
which `subscription.is_fully_honored()` reports.

### A cross-instance retry fails to decrypt `requestState`

The instances do not share a state secret. Set
`App::with_request_state_secret(...)` to the same value everywhere; neva
warns at startup when it is missing. A doubled `on_commit` across
instances means the state *store* is not shared either.

### `--all-features` behaves like a different SDK

It is one: `legacy-spec` is additive to Cargo, so `--all-features` compiles
the legacy protocol generation and the 2026-07-28 surface out. Use
`--features "server-full client-full"`.

### `cargo check` fails on `sampling` / `roots` types

They are `#[deprecated]` in this generation. Add `#[allow(deprecated)]` at
the call site. Also note `neva::types` re-exports the sampling types only
under `client` / `legacy-spec`, so a server-only build needs
`use neva::types::sampling::...` explicitly.

### `proto-2026-07-28-rc` is not a known feature

That flag existed only during the release candidate. Remove it — the
generation it selected is now the default.

## Things that no longer exist

Reaching for one of these is the clearest sign that code (or a suggestion)
predates MCP 2026-07-28:

| Gone | Replacement |
|---|---|
| `initialize` / `initialized` handshake | `server/discover`, done by `Client::connect()` |
| `ping`, `Client::ping`, `BatchBuilder::ping` | A `#[handler]` under your own method name |
| `logging/setLevel`, `with_logging`, `set_log_level` | Request-scoped logging via `_meta` |
| `tasks/list`, `tasks/result`, `Client::list_tasks` | Poll `tasks/get` with ids you kept |
| `notifications/roots/list_changed` | — |
| `notifications/elicitation/complete`, `Context::complete_elicitation`, `Client::on_elicitation_completed` | Answering the request is the signal |
| `elicitationId` on URL elicitation | Your own id in `requestState` |
| `with_mcp_version` on the **server** | `legacy-spec` build |
| `resources/subscribe` / `resources/unsubscribe` as RPC | `SubscriptionFilter::with_resource(uri)` |
| `Mcp-Session-Id`, session `DELETE`, standalone SSE `GET` | Stateless transport + `subscriptions/listen` |
| `ErrorCode::ResourceNotFound` (-32002) | `ErrorCode::RESOURCE_NOT_FOUND` |

All of them come back under `legacy-spec` — see `legacy.md`.
