# Building an MCP client with neva

Default profile (MCP 2026-07-28), `features = ["client-full"]`.

## Connecting

`connect()` opens with a single `server/discover` request. There is no
`initialize` / `initialized` handshake to write.

Over stdio — the client spawns the server process:

```rust
use neva::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let mut client = Client::new()
        .with_options(|opt| opt.with_stdio("npx", ["-y", "@modelcontextprotocol/server-everything"]));

    client.connect().await?;
    // ...
    client.disconnect().await
}
```

Over HTTP:

```rust
use neva::prelude::*;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let mut client = Client::new()
        .with_options(|opt| opt
            .with_http(|http| http.bind("127.0.0.1:3000").with_endpoint("/mcp"))
            .with_timeout(Duration::from_secs(5)));

    client.connect().await?;
    // ...
    client.disconnect().await
}
```

`with_default_http()` is the shorthand for `127.0.0.1:3000` + `/mcp`.

Against a protected server, add `.with_oauth(|oauth| oauth)` inside
`with_http(..)` and the client handles the whole authorization flow off the
first `401` — see `http.md`. Raise the timeout well past the default 10
seconds if the flow opens a browser.

`Client::discover()` is the explicit call behind `connect()`;
`Client::init()` survives as a back-compat alias. `Client::server_info` is
read from `_meta["io.modelcontextprotocol/serverInfo"]`, which every result
carries.

`disconnect()` is **local** — it shuts down the transport and sends
nothing. There is no goodbye message in the protocol.

### A `connect` that failed

`connect()` starts the transport, so a transport that will not start reports
there: a stdio command that cannot be spawned, a rejected OAuth or TLS
configuration, a failed bind. Before 0.6.1 both HTTP `start` implementations
answered `Ok` after logging the failure, and the real cause surfaced as a
request timeout — if you are debugging exactly that against 0.6.0, upgrade
before reading any further into it.

A failure on the way up leaves the client's configuration intact, so the same
client can be retried or pointed elsewhere (0.6.1+; on 0.6.0 the second attempt
reports `Transport protocol must be specified`, and needs a fresh `Client`):

```rust
use neva::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let mut client = Client::new()
        .with_options(|opt| opt.with_stdio("weather-mcp", ["--stdio"]));

    if client.connect().await.is_err() {
        client = client.with_options(|opt| opt
            .with_stdio("cargo", ["run", "-p", "weather-mcp"]));
        client.connect().await?;
    }

    client.disconnect().await
}
```

Retryable is exactly what the transport refuses **as a whole** — nothing has
been consumed, so the next attempt is a real one. Once the transport is
running, a later failure is not undone by retrying: build a new `Client`, since
a stdio server needs a fresh child process anyway.

A client that was never given a transport is told so by `connect`, not by its
first call.

### Talking to an older server

The client is **dual-mode**. If `server/discover` is rejected at the wire
phase (`MethodNotFound`, `InvalidRequest`, a non-JSON-RPC or unknown-code
reply), it falls back to the legacy `initialize` handshake and speaks
legacy to that peer for the rest of the connection. Network failures do
*not* trigger the fallback; the switch is per-connection, monotonic, and
decided before any other traffic.

So **you do not need a `legacy-spec` build just to reach an old server**.
`with_mcp_version(...)` only picks which legacy version the fallback
negotiates.

The version the client offers is a proposal: a server answering with a
different one it does speak is fine, and only a version neva does not know
at all ends the connection.

## The namespaces

Since **0.7.0** a server's primitives are reached one namespace per MCP
method prefix — `client.tools()` is `tools/*`:

| Namespace | Calls |
|---|---|
| `client.tools()` | `list(cursor)`, `list_all()`, `call(name, args)`, `call_raw(params)`, `as_task()` |
| `client.resources()` | `list(cursor)`, `list_all()`, `templates(cursor)`, `read(uri)` — and the legacy `subscribe(uri)` / `unsubscribe(uri)` |
| `client.prompts()` | `list(cursor)`, `list_all()`, `get(name, args)` |
| `client.tasks()` | `get(id)`, `update(id, responses)`, `cancel(id)` (feature `tasks`) |

Each is a `Copy` view borrowed from the client, so several can be held at
once. The flat 0.6 methods — `call_tool`, `list_tools`, `read_resource`,
`get_prompt`, `list_resources`, `list_resource_templates`, `list_prompts`,
`call_tool_raw`, `call_tool_as_task`, `task()` — still compile but are
`#[deprecated]`; write the namespaced form. On a 0.6 crate the namespaces do
not exist and the flat form is the only one.

### Sharing a connected client

Request methods take `&self`, so an `Arc<Client>` serves any number of tasks.
Only setup — `connect`, `map_*`, `on_*`, roots — takes `&mut self`, so do it
before wrapping:

```rust
use neva::prelude::*;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let mut client = Client::new().with_options(|opt| opt.with_default_http());
    client.connect().await?;

    let client = Arc::new(client);
    let calls: Vec<_> = ["London", "Paris"]
        .into_iter()
        .map(|city| {
            let client = client.clone();
            tokio::spawn(async move { client.tools().call("weather", ("city", city)).await })
        })
        .collect();

    for call in calls {
        let result = call.await.map_err(|e| Error::new(ErrorCode::InternalError, e.to_string()))??;
        println!("{:?}", result.content);
    }
    Ok(())
}
```

`disconnect(self)` consumes the client, so it needs the `Arc` back
(`Arc::into_inner`) — or simply drop the last handle.

### Timeouts and cancellation

A request the client stops waiting for is **cancelled** on the server: when it
outlives `with_timeout` (10 s by default), and when its future is dropped —
`tokio::time::timeout`, a losing `select!` branch, an aborted task. Over
Streamable HTTP under 2026-07-28 the client closes the request's stream, which
*is* the cancellation there, and sends no `notifications/cancelled`; over stdio
and to a legacy peer it sends the notification. An abandoned `call_batch`
cancels every request in it; the legacy `initialize` is never cancelled. A late
answer to a cancelled request is dropped.

So a timeout is not "stop waiting while the server finishes" — the server stops
too. Raise the timeout for a call that is meant to run long, or make it a
[task](#tasks).

## Calling tools

```rust
use neva::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let mut client = Client::new().with_options(|opt| opt.with_default_http());
    client.connect().await?;

    // One argument
    let result = client.tools().call("greet", ("name", "John")).await?;

    // Several
    let result = client.tools().call("greet", [("name", "John"), ("say", "Hi")]).await?;

    // None
    let result = client.tools().call("now", ()).await?;

    println!("{:?}", result.content);
    client.disconnect().await
}
```

Arguments accept a `(name, value)` tuple, an array/`Vec` of them, or a
`HashMap`. Mixed value types need a `HashMap<&str, serde_json::Value>` or
one call per type.

`client.tools().call_raw(CallToolRequestParams::new(name).with_args(..))`
returns the raw `Response`, error responses included, and sends the params'
own `_meta` (a `traceparent`, say) as given — only the progress token is
replaced by the client's.

### Structured results

```rust
use neva::prelude::*;

#[json_schema(de, debug)]
struct Weather {
    conditions: String,
    temperature: f32,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let mut client = Client::new().with_options(|opt| opt.with_default_http());
    client.connect().await?;

    let result = client.tools().call("weather", ("location", "London")).await?;

    // Raw structured content
    println!("{:?}", result.struct_content);

    // Or deserialized
    let weather: Weather = result.as_json()?;
    println!("{weather:?}");

    client.disconnect().await
}
```

Validate against the tool's own `outputSchema` before deserializing when
you do not control the server:

```rust
use neva::prelude::*;

#[json_schema(de, debug)]
struct Weather {
    conditions: String,
    temperature: f32,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let mut client = Client::new().with_options(|opt| opt.with_default_http());
    client.connect().await?;

    let tools = client.tools().list(None).await?;
    let tool = tools.get("weather")
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams, "no weather tool"))?;

    let result = client.tools().call(&tool.name, ("location", "London")).await?;
    let weather: Weather = tool.validate(&result).and_then(|res| res.as_json())?;
    println!("{weather:?}");

    client.disconnect().await
}
```

`#[json_schema]` derives schema metadata and, with `de` / `ser` / `serde`,
the matching serde derives; `debug` adds `Debug`.

## Listing, reading, prompting

```rust
use neva::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let mut client = Client::new().with_options(|opt| opt.with_default_http());
    client.connect().await?;

    let tools = client.tools().list_all().await?;
    let resources = client.resources().list_all().await?;
    let templates = client.resources().templates(None).await?;
    let prompts = client.prompts().list_all().await?;

    let resource = client.resources().read("res://config").await?;
    println!("{:?}", resource.contents);

    let prompt = client.prompts().get("greeting", ("name", "Neva")).await?;
    println!("{:?}", prompt.messages);

    client.disconnect().await
}
```

`list_all()` walks every page and returns the items together; a server still
paging after 64 pages is an **error**, not a truncated list. `list(cursor)`
asks for one page — `None` for the first, the previous result's
`next_cursor` for the next:

```rust
use neva::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let mut client = Client::new().with_options(|opt| opt.with_default_http());
    client.connect().await?;

    let mut cursor = None;
    loop {
        let page = client.resources().list(cursor).await?;
        for res in &page.resources {
            println!("{}", res.name);
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }

    client.disconnect().await
}
```

Listings are ordered by name and stable across calls, which is what makes
cursor pagination safe.

`ResourceContents`'s accessors — `uri`, `text`, `blob`, `json`, `mime`,
`title`, `annotations` — are available to a client build.
Before it they were server-only and a client had to match on the enum's
variants. The builders are still server-side.

A tool may carry MCP Apps metadata: `tool.ui()` reads the block, and
`tool.is_model_visible()` says whether the agent may see the tool at all —
a host has to filter, because the server lists app-only tools like any
other. See `apps.md`.

## Batching

```rust
use neva::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let mut client = Client::new().with_options(|opt| opt.with_default_http());
    client.connect().await?;

    let responses = client
        .batch()
        .list_tools()
        .list_resources()
        .call_tool("add", [("a", 40_i32), ("b", 2_i32)])
        .send()
        .await?;

    let tools = responses[0].clone().into_result::<ListToolsResult>()?;
    println!("{:?}", tools.tools);

    client.disconnect().await
}
```

`send()` returns `Vec<Response>` in request order; notifications added with
`.notify(method, params)` are fire-and-forget and produce no slot.
Available builders mirror the single calls: `list_tools`, `call_tool`,
`list_resources`, `read_resource`, `list_resource_templates`,
`list_prompts`, `get_prompt`, `notify`. These flat names are the batch
builder's own and are **not** deprecated.

`client.call_batch(envelopes)` is the layer underneath. It numbers every
request on the wire with ids of its own and puts the caller's ids back on the
responses, so an id of yours can never collide with one still in flight.

Two constraints: routing headers must not be sent with a batch (neva omits
them), and `subscriptions/listen` is rejected inside one — use
`Client::listen`.

## Subscriptions

Server-initiated notifications arrive on one long-lived
`subscriptions/listen` request. This replaces both the old standalone SSE
`GET` stream and the `resources/subscribe` RPC pair.

```rust
use neva::prelude::*;
use neva::types::notification::Notification;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let mut client = Client::new().with_options(|opt| opt.with_default_http());
    client.connect().await?;

    // Handlers first — after connect(), before listen().
    client.on_tools_changed(|_: Notification| async {
        println!("tool list changed");
    });
    client.on_resource_changed(|n: Notification| async move {
        if let Some(params) = n.params::<SubscribeRequestParams>() {
            println!("resource {} updated", params.uri);
        }
    });

    let mut subscription = client
        .listen(SubscriptionFilter::new()
            .with_tools_changed()
            .with_resource("res://config"))
        .await?;

    // ... work ...

    subscription.cancel().await?;
    client.disconnect().await
}
```

**Ordering matters.** Register handlers *after* `connect()` (the helpers
assert the server advertises the capability, which is unknown before
discovery) and *before* `listen()` (the acknowledgment is the first message
and notifications may follow immediately).

Filter builders: `with_tools_changed()`, `with_prompts_changed()`,
`with_resources_changed()`, `with_resource(uri)` / `with_resources(uris)`.
An omitted field means "not subscribed".

The server may **narrow** the filter to what it advertises. Check with
`subscription.is_fully_honored()`, and compare `requested()` against
`acknowledged()`. An acknowledgment *broader* than the request is a
protocol violation and `listen` rejects it.

`subscription.closed().await` reports how it ended: `Cancelled` (this
client), `Graceful(result)` (the server closed it), or `Abrupt` (stream
went away). Subscriptions are not resumable — call `listen` again.
Dropping the handle, or `disconnect()`, ends the subscription too.

A server shutting down owes you `Graceful`, and a neva server delivers it. Not
every peer does, so an `Abrupt` at shutdown reads as "the peer stopped" rather
than as a fault on this side. Over HTTP your own `cancel()` is always `Cancelled`, never
`Graceful`: cancelling closes the listen `POST`'s body, which *is* the
spec's cancellation mechanism there, and leaves no channel for a result.

Logging and progress need no subscription; they are request-scoped.

## Answering the server's input requests

A server can ask the client for input mid-call. The client answers and
re-issues the call — neva runs that whole loop inside `tools().call`, so the
caller still sees a single call. See `mrtr.md` for the model; the client
side is one handler plus a capability declaration.

```rust
use neva::prelude::*;

#[json_schema(ser)]
struct Contact {
    name: String,
    email: String,
}

#[elicitation]
async fn elicitation_handler(params: ElicitRequestParams) -> ElicitResult {
    match params {
        ElicitRequestParams::Url(_url) => {
            // open the URL, let the user do the external thing
            ElicitResult::accept()
        }
        ElicitRequestParams::Form(form) => {
            let contact = Contact {
                name: "John".into(),
                email: "john@example.com".into(),
            };
            elicitation::Validator::new(form)
                .validate(contact)
                .into()
        }
    }
}
```

Declaring the modes is what makes the server willing to ask:

```rust
use neva::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let mut client = Client::new()
        .with_options(|opt| opt
            .with_default_http()
            .with_elicitation(|e| e.with_form().with_url()));

    client.connect().await?;
    client.disconnect().await
}
```

Declaring a handler without `with_elicitation` enables **form** mode by
default. A mode you do not declare is one the server must not send — it
gets `MissingRequiredClientCapability` instead. Cap the retry loop with
`McpOptions::with_max_mrtr_rounds`.

### Handler shapes

Since **0.6.0** `Client::map_sampling`, `Client::map_elicitation`,
`#[sampling]` and `#[elicitation]` take a plain `fn` as well as an
`async fn`. A handler that puts a **blocking** dialog in front of a person —
a terminal prompt, a native modal — is what `blocking` is for: it runs on
Tokio's blocking pool instead of holding a runtime worker for as long as the
person takes to answer.

```rust
use neva::prelude::*;

#[elicitation(blocking)]
fn ask(params: ElicitRequestParams) -> ElicitResult {
    match params {
        ElicitRequestParams::Url(_url) => ElicitResult::accept(),
        ElicitRequestParams::Form(_form) => {
            let mut answer = String::new();
            // Blocks this thread until the user presses enter.
            let _ = std::io::stdin().read_line(&mut answer);
            ElicitResult::accept()
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let mut client = Client::new()
        .with_options(|opt| opt.with_default_http());

    client.connect().await?;
    client.disconnect().await
}
```

`blocking` on an `async fn` is a compile error. The two methods keep their
arity across 0.5 → 0.6, but their second generic parameter is now the shape
marker rather than the handler's future type.

### A sampling handler can fail

Since **0.7.0** `map_sampling` takes a handler returning
`Result<CreateMessageResult, Error>` as well as a bare `CreateMessageResult`.
Under 2026-07-28 an `Err` fails the call that asked for the sample; under
`legacy-spec` it answers the server's request. To answer with a real model,
`neva::svir::sampling_request` / `sampling_result` do the conversion — see
`svir.md`.

### What this client declares, per request

There is no handshake on 2026-07-28, so the client stamps its capabilities
onto **every request**'s `_meta` under
`io.modelcontextprotocol/clientCapabilities` — the MRTR modes above, and
since 0.6.0 the `extensions` map too. Nothing to call: declaring a handler
or calling `with_apps()` is what fills it in. A server reads it back with
`ctx.client_capabilities()`, `ctx.client_extension(id)` and
`ctx.supports_apps()`.

## Tasks

A long-running tool is called through the task builder. Declare the
capability with `with_tasks()` — it takes no closure, since advertising the
extension *is* the declaration:

```rust
use neva::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let mut client = Client::new()
        .with_options(|opt| opt
            .with_tasks()
            .with_default_http());

    client.connect().await?;

    let result = client
        .tools()
        .as_task()
        .with_ttl(10_000)                       // ms; omit for unlimited
        .call("slow_tool", ("input", "value"))
        .await;

    println!("{result:?}");

    client.disconnect().await
}
```

`call` on the builder drives the `tasks/get` polling loop for you and
resolves to the terminal outcome, so most code never issues the task
methods directly. When it must — a task id kept across a restart — they are on
`client.tasks()`: `get(id)` (`tasks/get`, the single polling method),
`update(id, responses)` (`tasks/update`, answering a task's input requests) and
`cancel(id)` (`tasks/cancel`, an acknowledgement — cancellation is cooperative,
so the outcome is learned by polling). A refused `update` or `cancel` is an
`Err`.

**There is no `tasks/list` and no `Client::list_tasks`.** A task id is a
durable handle you already hold; enumeration is your job. Task status is
learned by polling — `notifications/tasks` is a subscription category in
the spec but not in neva's filter yet.

## What is gone on the client

| Removed | Instead |
|---|---|
| `Client::ping`, `BatchBuilder::ping` | A `#[handler]` under your own method name on the server |
| `Client::on_elicitation_completed`, `ElicitationCompleteParams` | Answering the request *is* the completion signal |
| The standalone SSE `GET` stream | `Client::listen(filter)` |
| `resources().subscribe(uri)` / `unsubscribe(uri)` | `SubscriptionFilter::with_resource(uri)`. The methods still compile for the legacy fallback but a 2026-07-28 peer answers `MethodNotFound` |
| `set_log_level` | The client sets a level in the request's `_meta` |
