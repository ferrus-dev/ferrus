# The legacy profile and upgrading

## What `legacy-spec` is

A feature flag selecting the **previous protocol generation** — MCP
2024-11-05 … 2025-11-25 — instead of the default 2026-07-28.

It is a **switch, not an addition**: enabling it compiles the 2026-07-28
surface out. The two generations never coexist in one build.

Because Cargo features are additive, `--all-features` turns it **on**, so
that command tests the legacy profile. Exercise the default one with an
explicit list: `--features "server-full client-full"`.

```toml
neva = { version = "0.6", features = ["server-full", "legacy-spec"] }
```

## You usually do not need it on the client

neva's default-build client is **dual-mode**: it opens with
`server/discover` and, if the peer clearly does not speak 2026-07-28, falls
back to the `initialize` handshake and speaks legacy to that peer for the
whole connection. So a modern build reaches old servers.

The **server** has no such fallback — it is compile-time pure. A server
that must serve pre-2026-07-28 clients needs the `legacy-spec` build.

## What the flag restores

| Area | Legacy behavior |
|---|---|
| Handshake | `initialize` / `initialized`, with `serverInfo` in `InitializeResult` |
| Transport | Session-bound Streamable HTTP: `Mcp-Session-Id`, session `DELETE`, SSE `GET` streams with `Last-Event-ID` replay — as many per session as the client opens, see below |
| Stream resumption | A dropped `POST` response stream resumes once via a `GET` with `Last-Event-ID`, after the pause the server asked for; each stream keeps its own cursor and its own `retry:`-derived delay |
| Version selection | `with_mcp_version(...)` on the **server** |
| Server→client requests | Capability-driven push for `sampling/createMessage`, `roots/list`, `elicitation/create` — no MRTR |
| Macros | The `#[sampling]` attribute macro |
| Logging | `logging/setLevel`, `with_logging(handle)`, a global `notifications/message` path |
| Tools | The legacy typed `ToolSchema`, not JSON Schema 2020-12 |
| Tasks | The 2025-11-25 surface: `tasks/list`, `tasks/result`, the `cancel`/`list`/`requests` capability sub-tree, `with_tasks(\|t\| …)`, client-hosted tasks |
| Notifications | `ping`, `notifications/roots/list_changed`, `notifications/elicitation/complete` |
| Subscriptions | `resources/subscribe` / `resources/unsubscribe`, `Context::subscribe_to_resource` / `unsubscribe_from_resource` |
| Requests | No mandatory `_meta` keys, no routing-header validation, no `resultType` |

Everything else — DI, middleware, content types, JWT auth, TLS, custom HTTP
engines, batch requests — is shared and behaves identically in both.

### Concurrent SSE streams (0.5.5)

A session holds a **map** of streams — the spec lets a client "remain
connected to multiple SSE streams simultaneously" — each with its own
sender, cursor and replay buffer. Every tracked event id names its stream:
`id: <stream>:<seq>`.

| `GET` on the endpoint | Result |
|---|---|
| With `Last-Event-ID` | Resumes the stream that id names, replayed that stream's backlog alone — never what went out on another |
| `Last-Event-ID` naming a stream the session does not hold | `404` |
| Without one, standalone stream free | That stream — the one carrying server-initiated traffic |
| Without one, standalone stream live | A second stream; the first stays open and the server-initiated traffic follows the newer one |
| Session already at 8 streams | `429` (a disconnected stream is dropped to make room first) |

Server-initiated traffic rides exactly one stream at a time (the spec's MUST
NOT), following the newest live one; with nothing live the role stays put,
so an ordinary reconnect takes that stream back and gets its replay.

Before 0.5.5 there was one sender per session: a second `GET` overwrote the
first and the displaced stream ended on a bare EOF. Old-shape ids (`<seq>`,
no stream) are still read as the standalone stream's cursor while the
session holds only that one, so a client resumes across a server upgrade.
neva's own client echoes back whatever id it was handed and is unaffected.

## Writing against the legacy profile

The differences that actually change handler code:

**Elicitation suspends instead of re-running.** `ctx.elicit(params)` takes
**no replay key**, the handler is not re-entered, and `memo` / `once` /
`on_commit` are not needed:

```rust
// legacy-spec only
// let result = ctx.elicit(params).await?;
```

**Sampling and roots are server→client push requests**, gated on the
client's declared capabilities from the handshake, and are not deprecated
in that generation. `#[sampling]` exists to register the client handler.

**Tool schemas are the typed `ToolSchema`**, with builder methods like
`with_prop` / `with_required`, rather than a `serde_json::Value`-shaped
2020-12 document. Closure bodies passed to `with_input_schema` therefore do
not port between profiles.

**Subscriptions belong to the server.** `ctx.subscribe_to_resource(uri)`
exists; the client's `subscriptions/listen` does not.

## Upgrading 0.4.x → 0.5.x

1. **Remove `proto-2026-07-28-rc`** from `Cargo.toml` — the flag is gone
   and the generation it selected is the default.
2. **Decide the profile.** To keep the old behavior, add
   `features = ["legacy-spec"]` and stop here. To move to 2026-07-28,
   continue.
3. **Delete the handshake assumptions.** No `initialize`; `Client::init()`
   still works as an alias for `discover()`.
4. **Rewrite every elicit call site** to take a replay key and to be
   re-run safe. This is the substantive part of the migration — read
   `mrtr.md` and audit every side effect above an elicit point.
5. **Replace `ping`** with a `#[handler]` under your own method name.
6. **Replace `with_logging` / `set_log_level`** with the request-scoped
   logging layer (`notification::fmt::layer()`).
7. **Replace `subscribe_to_resource`** on the client with
   `Client::listen(SubscriptionFilter::new().with_resource(uri))`; drop it
   from server handlers entirely.
8. **Drop `tasks/list` / `tasks/result` usage**; keep task ids and poll
   `tasks/get`.
9. **Check schemas.** `ToolSchema` builder code becomes a 2020-12 document;
   `#[tool]` emits one for you, so the simplest migration is often to
   delete the hand-written schema.
10. **Run the binary once.** Argument-name disagreements are a startup
    panic in 0.5.x, not a runtime surprise.

## Upgrading 0.5.0/0.5.1 → 0.5.2

Mostly additive. The breaking pieces:

* `App::map_tool` / `Tool::new` now take
  `Args: FromHandlerArgs<CallToolRequestParams>` and the prompt equivalents
  take `FromHandlerArgs<GetPromptRequestParams>`, replacing the
  `TryFrom<...>` bounds. Handlers are unaffected; a hand-written
  `impl TryFrom` needs porting.
* `ToolHandler::args` returns `Vec<ToolArg>` instead of
  `Option<HashMap<String, SchemaProperty>>`, ordered by argument slot.
* `PropertyType` gained an `Integer` variant, and `"integer"` no longer
  deserializes into `Number` — an exhaustive match needs the new arm.
* The schema structs in `neva::types::schema` gained an `extra` field, so
  an exhaustive struct literal needs it (or `..Default::default()`).
  `EnumOption` lost `Eq`; `PartialEq` remains.
* **Wire:** a tool registered from a bare closure now advertises `arg0`,
  `arg1`, … instead of the former type names, and `|a: i32, b: i32|`
  publishes two properties where it used to collapse into one. `#[tool]`
  tools are unaffected. Name them explicitly with `map_tool!` or
  `with_arg_names` to control what peers see.

## Upgrading 0.5.2 → 0.5.3/0.5.4

Additive throughout — no API breaks. What can change behavior:

* **The OAuth `TokenStore` key** is now `{issuer}|{client}|{resource}`.
  Entries written by 0.5.2 are not found under it and are left in place;
  those sessions re-authorize once. A custom store needs no code change.
* **A stored refresh token requires `OAuthClientConfig::with_issuer(..)`.**
  Without one the session re-authorizes interactively on every start,
  where 0.5.2 would have reused the token.
* **`Context::resource_updated` no longer pre-checks `is_subscribed`.** It
  publishes unconditionally. A handler that relied on the pre-check to
  suppress notifications must gate the call itself — but see `server.md`
  for why that is usually the wrong shape.
* **`bind("::1:3000")`** now gets DNS-rebinding protection instead of
  silently defaulting to `allow_any_origin`, so a deployment that was
  relying on that accidental opening starts answering `403`. State the
  names with `with_allowed_origins([..])`, or `allow_any_origin()`
  deliberately.
* **`App::with_request_state_audience`**, if you adopt it, seals state
  under wire version `v2.` — a mixed rollout refuses those states rather
  than running them unbound. Roll the binary out first, then the option.

New surface worth knowing: `App::with_notification_bus(..)` and
`with_request_state_audience(..)` (0.5.3); `App::with_shutdown()`,
`with_shutdown_signal(..)`, `with_shutdown_drain(..)`, the
`client-oauth-jwt` and `client-oauth-dpop` features, and the
client-authenticating OAuth grants (0.5.4). See `http.md` and `server.md`.

## Upgrading 0.5.4 → 0.5.5

Three traits change signature, all of them narrowly:

* **`AuthorizationHandler`** (`redirect_uri`, `authorize`) and
  **`RequestStateStore`** (`get`, `put`, `reserve`) drop `BoxFuture` for
  plain `async fn`s. Delete the `Box::pin(async move { .. })` wrapper and
  the lifetimes it needed. Users of the default `LoopbackHandler` /
  `InMemoryStateStore` change nothing. `neva::shared::BoxFuture` is no
  longer part of any trait neva asks you to implement — it stays public for
  the middleware `Next`.
* **`HttpEngine::tracked_event` takes an `EventId`** instead of a `u64`.
  `id.to_string()` renders `<stream>:<seq>`; the migration is the signature
  and nothing else. Only custom engines are affected.

Behavior that changes without a signature: `App::run` now waits for the
transport writers before returning, and the Volga engine stops on the
transport's token — see `http.md`, *Stopping a server*. Under the legacy
profile, the SSE session model above.

## Upgrading 0.5.5 → 0.5.6

Additive, with one change that can break a build on purpose:

* **Unknown macro attributes are now compile errors.** `#[tool]`,
  `#[resource]`, `#[resources]`, `#[prompt]` and `#[handler]` used to ignore
  an attribute they did not know. If something stops compiling, the
  attribute it names was never doing anything — fix the spelling or delete
  it. The motivating case was a misspelled `visibility`, which published an
  app-only tool to the agent.
* **MCP Apps** arrives behind the new `apps` feature — see `apps.md`. Under
  `legacy-spec` only the **client** half exists (`with_apps()`,
  `with_app_mime_types()`, `Tool::ui()`, `ResourceContents::ui()`); the
  server half is compiled out, since the extension rides
  `capabilities.extensions`.
* **`ClientCapabilities::extensions` is no longer gated on the protocol
  generation**, so a legacy `initialize` can carry it. This makes the legacy
  profile the *better*-covered one for MCP Apps negotiation right now: a
  2026-07-28 client advertised no extensions at all, having no handshake to
  put them on — **fixed in 0.6.0**, which puts them on each request's `_meta`.
  `ServerCapabilities::extensions` stays 2026-07-28-only.
* **`ResourceContents`'s accessors** (`uri`, `text`, `blob`, `json`, `mime`,
  `title`, `annotations`) are available to a client build. Purely additive;
  the builders stay server-side.

## Upgrading 0.5.6 → 0.6.0

Additive almost everywhere, with two source-breaking changes. Both fail the
build rather than changing behaviour silently.

* **`UiResource::with_permissions` is renamed `with_ui_permissions`.** On a
  `UiResource` the old name now means *who may read the resource*, as on
  every other resource, so a 0.5.6 `with_permissions(UiPermissions::..)` call
  no longer compiles. `UiResourceMeta::with_permissions` is unchanged. See
  `apps.md`.
* **Registration methods take one more generic parameter** — the handler's
  shape marker. It is always inferred, so only a call site that spells its
  generics out is affected: `map_tool::<_, _, (String,)>(..)` becomes
  `map_tool::<_, _, (String,), _>(..)` and fails with E0107 until it does.
  Bounds are unaffected; the marker is defaulted on the traits.

And, additively:

* **Synchronous handlers** at every registration point on both sides. A
  handler may return its value directly instead of a future; which shape it
  has is read off the signature, and the published schema, argument slots and
  response are unchanged. See the handler-shapes section of `server.md`.
* **`neva::blocking`** and `blocking` as an attribute on all eight macros,
  for a synchronous handler that really blocks.
* **Per-request client extensions.** `with_apps()` now reaches a 2026-07-28
  server, and `Context::client_extension(id)` / `Context::supports_apps()`
  read the declaration back. Not available under `legacy-spec`, where
  capabilities ride `initialize`.
* **Roles and permissions on `add_ui_resource`**: `UiResource::with_roles`
  and `with_permissions`, as on a resource template.

None of this reaches the legacy profile except the two breaking renames,
which apply to any build that uses those APIs. Synchronous handlers and
`blocking` work under `legacy-spec` too.

## Upgrading 0.6.0 → 0.6.1

Drop-in: nothing renamed, nothing removed, no call site to change. Bump the
version and keep going. What it adds is worth knowing about:

* **`registry`**, a new feature in `server-full`: `server.json` for the MCP
  Registry, generated from the app and the crate and validated locally. A
  hand-picked feature list needs `registry` added to it. See the registry
  section of `http.md`.
* **A failed `Client::connect` can be retried** on the same client. On 0.6.0
  the second attempt reported `Transport protocol must be specified`; a client
  that worked around it by rebuilding itself still works.
* **A transport that cannot start says so at `connect` / `run`** rather than as
  a later timeout — a rejected TLS or OAuth configuration, a failed bind, a
  client or server with no transport at all. Code that inferred "misconfigured"
  from a timeout can stop.
* An icon that names no theme leaves `theme` out instead of writing null.

This is also where the volga dependency moves to 0.11.x, which matters only if
you depend on volga directly alongside neva.

## Examples in the neva repository

Legacy variants live under a `legacy/` sub-directory, each its own Cargo
workspace — features unify across workspace members, so a shared workspace
would flip the generation for every crate in it:

* `examples/roots/legacy/{server,client}`
* `examples/sampling/legacy/{server,client}`
