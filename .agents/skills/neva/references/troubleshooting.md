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

An old neva reading a conformant client's per-request capabilities. Upgrade —
current releases accept both shapes.

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

If it started after adding `with_request_state_audience`: the value
must be identical on every instance, and states in flight when it was
turned on are refused until they lapse (5 minutes). A mixed rollout also
refuses — an audience-bound state is sealed under wire version `v2.`, which
a binary predating the option cannot read, deliberately.

### A subscriber hears nothing about a change made on another instance

The stateless transport pins nothing, so the `subscriptions/listen` stream
and the request that mutated the server landed on different processes.
Configure `App::with_notification_bus(..)`. If a bus is installed
and it still happens, check the bus does not suppress echo — local delivery
goes through `subscribe()` too, so an implementation that hides an
instance's own publishes silences that instance's own subscribers.

Also: do not gate the notification on `ctx.is_subscribed(..)`. It is
node-local and answers only for the instance running the handler.

### `SubscriptionEnd::Abrupt` when the server shuts down

Owed a `Graceful`. A neva server sends the empty result first and `run` waits
for the transport writers to put it on the wire, so check what is actually
on the other end: an older neva, or a peer that does not send it, produces an
`Abrupt` that is not a fault on this side.
`App::with_shutdown_drain(Duration::ZERO)` opts out of the graceful close
deliberately, and the two teardown phases share that one budget rather than
each taking it afresh.

### The server "stops" but the port stays bound

The engine did not bring its listener down. An `HttpEngine` is handed a
`CancellationToken` and must wire it to the framework's graceful shutdown;
`run` returning is what neva waits for. An engine that takes the token and only
reports its own failures leaves the endpoint serving after `App::run` returned.

### `Box::pin(async move { .. })` no longer compiles in a trait impl

`AuthorizationHandler` (`redirect_uri`, `authorize`) and `RequestStateStore`
(`get`, `put`, `reserve`) take plain `async fn`s. Drop the wrapper and the
explicit lifetimes; the body is unchanged. `neva::shared::BoxFuture`
is still public — the middleware `Next` returns one — it is just no longer
part of any trait you implement.

### A custom engine fails to compile on `tracked_event`

The parameter is an `EventId` (re-exported from `neva::prelude`), not a `u64`.
Take the id and write `id.to_string()` where a sequence number would go: it
renders `<stream>:<seq>`, because an event id is a cursor within one SSE stream
rather than within the session.

### A resumed SSE stream replays the wrong events, or is answered `404`

The engine is writing out a trimmed id — the `seq` half alone, or its own
counter. A `Last-Event-ID` has to name the stream it resumes, so neva
refuses one that names a stream the session does not hold rather than
serving it from whatever stream is at hand. Write the whole `EventId`.
(An id that names no stream is read as the standalone stream's cursor while the
session holds only that one, so an older client is not stranded.)

### A second `GET` on the same session gets `429`

A session hosts at most **8** SSE streams. A disconnected stream is dropped
to make room before a `GET` is refused, so the cap is spent on live ones —
seeing this means eight are genuinely connected. Legacy profile only.

### The OAuth client re-authorizes on every start

A stored refresh token is only read back under the authorization server
that minted it, and nothing records which one that was without
`OAuthClientConfig::with_issuer(..)`. Set it. Dynamically registered
clients never reuse a token either. The reason is that the server a flow
discovers is vouched for by the resource alone, which is exactly what an
attacker controlling the resource rewrites.

Relatedly, the `TokenStore` key is `{issuer}|{client}|{resource}` — the whole
identity a credential belongs to — so two servers never share a slot.

### The OAuth flow registers, then fails at the token request

The registration response named no `token_endpoint_auth_method`, RFC 7591
fills that silence with `client_secret_basic`, and the server advertises
only `none`. The server's own metadata decides, and a secret is presented the
way `token_endpoint_auth_methods_supported` says it is accepted rather than
always as HTTP Basic — so check what the server actually advertises.

### A DPoP request fails on a `3xx`

By design. A proof covers one method and one URL, nothing can re-sign it
mid-chain, so a DPoP connection does not follow redirects and the `3xx` is
surfaced as itself. Point the client at the final URL. Bearer connections
are unaffected.

### An OIDC-strict server refuses the redirect URI

A loopback redirect outside the literal `127.0.0.1` used to register the
client as `web`, which such a server refuses for a plain-http redirect.
Fixed in 0.5.4 — the whole `127.0.0.0/8` range registers a native client
(RFC 8252 §7.3). `localhost` and `[::1]` were never affected.

Separately: an authorization server validates the redirect it is sent
against a registered list, so a `LoopbackHandler` on an **ephemeral** port
cannot be described by a pre-registration or a Client ID Metadata Document.
Pin it with `LoopbackHandler::new().with_port(8919)` and register both the
`127.0.0.1` and `localhost` spellings.

### A loopback server accepts any `Origin`

Check the bind string: a non-loopback bind defaults to `allow_any_origin`,
because the legitimate names are not knowable from there. Bind strings are read
the way `std` reads them, so `bind("::1:3000")` does listen on `[::1]:3000` and
is treated as loopback — write `[::1]:3000` anyway, so it does not depend on the
last-colon rule.

### `--all-features` behaves like a different SDK

It is one: `legacy-spec` is additive to Cargo, so `--all-features` compiles
the legacy protocol generation and the 2026-07-28 surface out. Use
`--features "server-full client-full"`.

### `cargo check` fails on `sampling` / `roots` types

They are `#[deprecated]` in this generation. Add `#[allow(deprecated)]` at
the call site. Also note `neva::types` re-exports the sampling types only
under `client` / `legacy-spec`, so a server-only build needs
`use neva::types::sampling::...` explicitly.

### An unknown attribute on `#[tool]` / `#[resource]` / `#[prompt]`

These macros reject an attribute they do not know rather than ignoring it —
a dropped attribute is how a misspelled `visibility` would publish an app-only
tool to the agent. Fix the spelling or delete the attribute.

### An MCP App renders nothing, or as unstyled plain text

The MIME type is not `text/html;profile=mcp-app`, or the tool's
`_meta.ui.resourceUri` names a resource nothing serves — the server logs a
warning about the second at startup. A hand-built `TextResourceContents` on
a non-`ui://` URI ships `text/plain`, which no host renders. See `apps.md`.

### An MCP App loads but sits on its placeholder forever

The View never completed the handshake: `ui/initialize`, *then*
`ui/notifications/initialized`. A conforming host holds the tool result back
until it has seen both. Also check that the `ui/notifications/tool-result`
listener is registered **before** the handshake — the host may push the
result the instant it sees `initialized`.

### A `_meta.ui` CSP or permission setting is silently ignored

A snake_case key. The wire is camelCase (`prefersBorder`,
`connectDomains`), `_meta` is an open map, and hosts ignore what they do not
recognise. The macro's `ui_meta` rejects this at compile time; a hand-built
`serde_json::json!` block does not.

### `with_apps` / `add_ui_resource` / `map_ui_resource` not found

Either the `apps` feature is off, or the build has `legacy-spec` on — the
server half of MCP Apps is 2026-07-28 only. Remember `--all-features`
enables `legacy-spec`.

### E0107 on `map_tool` / `map_prompt` / `Tool::new` — wrong number of generics

0.6.0 gave every registration point a **handler-shape marker** as a generic
parameter, which is what lets one call take both an `async fn` and a plain
`fn`. It is always inferred, so only a call site that spells its generics
out is affected:

```rust
// 0.5.x
// app.map_tool::<_, _, (String,)>("greet", greet);
// 0.6.0
// app.map_tool::<_, _, (String,), _>("greet", greet);
```

Affected: `App::map_tool`, `map_prompt`, `map_resource`, `map_ui_resource`,
`map_handler`, `map_resources`, `map_completion`, `Tool::new`,
`Prompt::new`. `Client::map_sampling` and `map_elicitation` keep their
arity. **Bounds are unaffected** — the marker is defaulted on the traits, so
`where F: ToolHandler<Args, Output = R>` still means what it meant. The fix
is to add `_`, or to delete the turbofish and let inference work.

### `blocking` rejected on an `async fn`

`#[tool(blocking)]` and `neva::blocking(..)` take a **synchronous** handler.
An asynchronous one has nothing to offload — it already yields — so this is
a compile error by design. Either drop `async` and the `.await`s, or drop
`blocking`.

The reverse mistake does not fail the build: a plain `fn` that blocks,
registered **without** `blocking`, compiles and runs inline on the runtime
thread that dispatched the request, holding a worker for its whole duration.
`std::fs`, `Command::output` and synchronous DB or HTTP drivers all belong
behind `blocking`. Short, non-blocking bodies do not — the hand-off costs
more than the work.

### `supports_apps` / `client_extension` not found, or always false

Both are **0.6.0** and are compiled out under `legacy-spec`, which has no
per-request `_meta` channel for capabilities. `supports_apps` additionally
needs the `apps` feature.

Present but always `false` means the caller declared nothing usable:
`supports_apps()` requires `mimeTypes` to name
`text/html;profile=mcp-app` — the spec makes `mimeTypes` mandatory, so a
declaration without it does not count. A 0.5.x neva client speaking
2026-07-28 sent no extensions at all.

### `with_permissions` no longer accepts `UiPermissions`

0.6.0 renamed the `UiResource` builder for the iframe's browser permissions
to **`with_ui_permissions`**, and gave `with_permissions` the meaning it has
on every other resource: **who may read it**. Rename the call.
`UiResourceMeta::with_permissions` is unchanged.

### `Transport protocol must be specified` on a second `connect`

The first `connect` failed — typically a stdio command that could not be
spawned — and took the configured transport with it, so the retry had nothing
to start and blamed the caller for a step they did take. Fixed in **0.6.1**:
the options keep the transport until `start` succeeds, and the retry reports
the spawn failure again. On 0.6.0, build a fresh `Client` for the second
attempt.

Still not retryable on any version: a failure *after* the transport started.
That needs a new `Client` — a stdio server needs a new child process.

### An HTTP client times out instead of naming a bad TLS or OAuth config

Before **0.6.1** both HTTP `start` implementations logged the failure and
answered `Ok`, so a rejected configuration surfaced as an unanswered handshake
and a failed bind as an already-cancelled token. Upgrade; `connect()` / `run()`
then report the real cause. A client with no transport at all is likewise
reported at `connect` rather than at the first send.

### `description` is N characters; the registry allows 100

A `server.json` limit, not a crates.io one, so a crate description filled in by
`with_cargo` is the usual cause. Say a shorter one with `with_description`
**before** the `with_cargo*` call — what is already set is never overwritten.

### A registry name is refused: "no `/`"

`ServerManifest`'s `name` is the registry identifier —
`io.github.<user>/<server>`, inside a namespace the publisher has proved they
own — and not the MCP server name that `with_name` sets. Passing the MCP name
produces a manifest that fails namespace verification, so it is refused here
instead.

### "this manifest came from an app with no transport"

`App::server_manifest` reads the transport off the app, and an app that was
never given one cannot start, so no package can honestly say how to reach it.
Configure `with_stdio()` / `with_http(..)` before calling `server_manifest`, or
build the manifest with `ServerManifest::new` and state the package yourself.

### The registry rejects a manifest that `to_json()` accepted

Expected. `validate` checks the document against the schema it is written for;
a registry adds rules of its own — which hosts it fetches archives from, which
base URLs it takes, what it makes of a loopback address — and reports which one
was broken. Act on that message, not on the local `Ok`.

### `neva::registry` / `server_manifest!` / `cargo_env!` not found

The `registry` feature is off, or the crate is 0.6.0 or older. It is in
`server-full` from **0.6.1**; add `registry` explicitly to a hand-picked
feature list.

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
| `Mcp-Session-Id`, session `DELETE`, SSE `GET` streams | Stateless transport + `subscriptions/listen` |
| `ErrorCode::ResourceNotFound` (-32002) | `ErrorCode::RESOURCE_NOT_FOUND` |

All of them come back under `legacy-spec` — see `legacy.md`.
