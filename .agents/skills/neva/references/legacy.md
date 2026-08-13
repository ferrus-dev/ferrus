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
neva = { version = "0.5", features = ["server-full", "legacy-spec"] }
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
| Transport | Session-bound Streamable HTTP: `Mcp-Session-Id`, session `DELETE`, standalone SSE `GET` with `Last-Event-ID` replay |
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

## Examples in the neva repository

Legacy variants live under a `legacy/` sub-directory, each its own Cargo
workspace — features unify across workspace members, so a shared workspace
would flip the generation for every crate in it:

* `examples/roots/legacy/{server,client}`
* `examples/sampling/legacy/{server,client}`
