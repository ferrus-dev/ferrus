---
name: neva
description: Build, review and debug MCP (Model Context Protocol) servers and clients in Rust with the neva crate — tools, prompts, resources, elicitation and multi round-trip requests, Streamable HTTP and stdio transports, auth, DI and deployment. Use whenever Rust code imports `neva`, whenever the task is to expose something as an MCP server or to talk to one from Rust, and when upgrading such code across neva or MCP-spec versions.
license: MIT
metadata:
  neva-version: "0.5.2"
  mcp-protocol: "2026-07-28"
  docs: "https://romanemreis.github.io/neva-docs/"
  api-reference: "https://docs.rs/neva"
---

# neva — MCP servers and clients in Rust

`neva` is a Rust SDK for the Model Context Protocol. One crate covers both
sides: `App` builds servers, `Client` builds clients, and a single process
can run both.

**This skill describes neva 0.5.x, which speaks MCP `2026-07-28` by
default.** That protocol generation broke compatibility with everything
before it. Most MCP knowledge in circulation — and every other MCP SDK —
describes the *older* generation, so the failure mode here is not "you
forget an API", it is "you confidently write the previous protocol". The
[Non-negotiables](#non-negotiables) section below is the list of places
where that happens. Read it before writing code, every time.

## Step 1 — establish the version and the profile

Do this first; it changes which API is correct.

```bash
cargo add neva --features server-full   # server
cargo add neva --features client-full   # client
cargo add neva --features full          # both
```

In an existing project, read `Cargo.toml`:

| What you find | What it means |
|---|---|
| `neva = "0.5"` and no `legacy-spec` | Default profile, MCP 2026-07-28. This skill applies as written |
| `features = [… "legacy-spec" …]` | **Legacy profile**, MCP 2024-11-05 … 2025-11-25. A *different* API. Read `references/legacy.md` before touching anything |
| `neva = "0.4"` or older | Pre-2026-07-28 by default. Read `references/legacy.md` for the upgrade |
| `proto-2026-07-28-rc` | A flag that no longer exists — remove it |

`legacy-spec` is a **switch, not an addition**: it compiles the 2026-07-28
surface out. The two generations never coexist in one build. Because Cargo
features are additive, `--all-features` turns `legacy-spec` **on** and
therefore tests the legacy profile — use `--features "server-full
client-full"` to exercise the default one.

## Step 2 — route to the reference you need

Load only what the task calls for; each file is self-contained.

| The task | Read |
|---|---|
| Tools, prompts, resources, schemas, content types, completion | `references/server.md` |
| DI, middleware, logging, progress, subscriptions on the server | `references/server.md` |
| Connecting, calling tools, batching, subscribing, client handlers | `references/client.md` |
| Asking the user for input mid-handler; `input_required`; re-run safety | `references/mrtr.md` |
| Long-running work, `tasks/get` polling | `references/mrtr.md` |
| Transport choice, TLS, JWT/OAuth, DNS-rebinding, multi-instance deploy | `references/http.md` |
| A custom HTTP stack (axum, hyper, actix-web) | `references/http.md` |
| An error code, a `-320xx` on the wire, or "why is this rejected" | `references/troubleshooting.md` |
| `legacy-spec`, MCP ≤ 2025-11-25, upgrading from 0.4.x | `references/legacy.md` |

## A server that works

```rust
use neva::prelude::*;

#[tool(descr = "Greets a person by name")]
async fn greet(name: String) -> String {
    format!("Hello, {name}!")
}

#[tokio::main]
async fn main() {
    App::new()
        .with_options(|opt| opt
            .with_stdio()
            .with_name("my-mcp-server")
            .with_version("1.0.0"))
        .run()
        .await;
}
```

`#[tool]` derives the name from the function, the JSON Schema 2020-12
`inputSchema` from the parameters, and the `outputSchema` from the return
type. Swap `.with_stdio()` for `.with_default_http()` to serve Streamable
HTTP on `127.0.0.1:3000/mcp` instead — nothing else changes.

## A client that works

```rust
use neva::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let mut client = Client::new()
        .with_options(|opt| opt.with_stdio("my-server-binary", ["--flag"]));

    client.connect().await?;

    let tools = client.list_tools(None).await?;
    for tool in &tools.tools {
        println!("{}", tool.name);
    }

    let result = client.call_tool("greet", ("name", "World")).await?;
    println!("{:?}", result.content);

    client.disconnect().await
}
```

## Non-negotiables

Each of these is a real difference between MCP 2026-07-28 and the
generation most training data describes. Getting one wrong produces code
that compiles and then fails on the wire.

1. **There is no `initialize` handshake.** A client opens with one
   `server/discover` request. `Client::connect()` does it; you never write
   an initialize/initialized exchange. Server side: nothing to implement,
   neva answers discovery from what you registered.

2. **The HTTP transport is stateless.** No `Mcp-Session-Id`, no session
   `DELETE`, no standalone SSE `GET` stream. Never write code that opens a
   `GET` stream to receive notifications — a client asks for them with
   `Client::listen(filter)` and they arrive on that request's own stream.

3. **`ping` is gone.** No `Client::ping`, no `BatchBuilder::ping`. A
   liveness probe is a `#[handler(command = "…")]` under your own method
   name.

4. **`logging/setLevel` is gone**, and with it `with_logging` and
   `set_log_level`. Logging is request-scoped: the client puts a level in
   the request's `_meta` and the notifications ride that request's response
   stream.

5. **Elicitation is a re-run, not a suspend.** `ctx.elicit(key, params)`
   takes a **replay key**, the handler unwinds, and the whole handler
   **runs again from the top** when the client answers. Everything above an
   elicit point executes on every round — wrap side effects in `ctx.memo`,
   `ctx.once` or `ctx.on_commit`. See `references/mrtr.md`. A handler
   written as if `elicit` merely awaits will double-charge cards.

6. **Sampling and roots are deprecated on arrival.** They still exist as
   MRTR input-request kinds, `#[deprecated]`, needing `#[allow(deprecated)]`
   at call sites. Do not build new features on them; do not delete them
   from code that has them.

7. **Tool arguments are extracted by name.** The handler's parameter names
   and the published `inputSchema` property names must match, or `App::run`
   refuses to start. `#[tool]` handles this. A **bare closure** passed to
   `map_tool` does not — Rust drops closure parameter names — so it
   publishes `arg0`, `arg1`, …; use the `map_tool!` macro or
   `.with_arg_names([...])`.

8. **Optional arguments are `Option<T>`**, not a hand-written schema
   without `required`. An `Option<T>` parameter is published as its inner
   type, left out of `required`, and arrives as `None` when omitted.

9. **Capabilities are declared per request, not once per connection.** Ask
   `ctx.client_capabilities()` before requesting input; asking for a kind
   the caller did not declare ends the call with
   `MissingRequiredClientCapability` (`-32021`) rather than degrading.

10. **A tool handler's `Err` is not a protocol error.** For `#[tool]`, any
    error becomes a *tool error* — a successful response with
    `is_error: true` that the model reads and can recover from. For
    `#[resource]` and `#[prompt]`, an `Err` is a JSON-RPC error and the
    request fails. Choose the handler kind accordingly.

## Verify before you claim it works

```bash
cargo check --features "server-full"     # or client-full / full
```

`cargo check` catches most of it, but two classes of mistake compile
cleanly and fail at runtime:

* an argument-name or schema disagreement — caught at **startup**, so
  actually run the binary once;
* a protocol-generation mistake — caught only by a peer.

Drive a real client against a server with the MCP Inspector:

```bash
npx @modelcontextprotocol/inspector cargo run
```

For HTTP, start the server and point the Inspector at
`http://127.0.0.1:3000/mcp`.

## Conventions worth keeping

* `use neva::prelude::*;` is the intended import — it carries `App`,
  `Client`, `Context`, the macros, the types and the error model.
* Handlers are `async fn`. Returning a plain `String`, `&str`, `Json<T>`,
  `Content` or `CallToolResponse` all work; prefer the simplest that fits.
* Registration order does not matter, and listings are sorted by name —
  the registries are `BTreeMap`-backed, which is what makes cursor
  pagination safe.
* Do not hand-write JSON-RPC envelopes, `_meta` keys or routing headers.
  neva builds them, and a hand-built one that disagrees with the body is
  rejected with `HeaderMismatch` (`-32020`).
