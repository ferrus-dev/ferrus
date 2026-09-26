# Building an MCP server with neva

Everything here assumes the default profile (MCP 2026-07-28) and
`features = ["server-full"]`. For `legacy-spec`, read `legacy.md` first.

## The app

```rust
use neva::prelude::*;

#[tokio::main]
async fn main() {
    App::new()
        .with_options(|opt| opt
            .with_stdio()                       // or .with_default_http()
            .with_name("my-mcp-server")
            .with_version("1.0.0"))
        .run()
        .await;
}
```

`run_blocking()` is the synchronous entry point when you cannot be in an
async context:

```rust
use neva::prelude::*;

fn main() {
    App::new()
        .with_options(|opt| opt.with_default_http())
        .run_blocking();
}
```

There is no discovery handler to write. neva answers `server/discover`
itself, advertising the versions it speaks plus the capabilities implied by
what you registered and configured. `with_name` / `with_version` are what
every result reports back under `_meta["io.modelcontextprotocol/serverInfo"]`.

## Handler shapes

Since **0.6.0** every registration point takes a handler in either shape,
and the shape is read off the signature. On 0.5.x, only the asynchronous one
exists.

```rust
use neva::{prelude::*, blocking};

// Awaits something -> async fn.
#[tool(descr = "Fetches a page")]
async fn fetch(url: String) -> String {
    format!("fetching {url}")
}

// Computation on data in hand -> plain fn, runs inline.
#[tool(descr = "Adds two numbers")]
fn add(a: i32, b: i32) -> i32 {
    a + b
}

// Blocks -> plain fn on Tokio's blocking pool.
#[tool(descr = "Reads a text file", blocking)]
fn read_file(path: String) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

#[tokio::main]
async fn main() {
    let mut app = App::new()
        .with_options(|opt| opt.with_stdio());

    // Closures take both shapes too. Name the arguments either way.
    app.map_tool("mul", |a: i32, b: i32| a * b)
        .with_arg_names(["a", "b"]);

    app.map_tool("read_other", blocking(|path: String| {
        std::fs::read_to_string(path).unwrap_or_default()
    }))
    .with_arg_names(["path"]);

    app.run().await;
}
```

Accepted at: `App::map_tool`, `map_prompt`, `map_resource`, `map_resources`,
`map_completion`, `map_handler`, `map_ui_resource`, `Tool::new`,
`Prompt::new`, the `map_tool!` / `map_prompt!` macros, and every attribute
macro. `blocking` is an attribute on all eight macros — `#[tool]`,
`#[prompt]`, `#[resource]`, `#[resources]`, `#[completion]`, `#[handler]`,
`#[sampling]`, `#[elicitation]`.

Rules:

* `blocking` on an `async fn` is a **compile error**.
* `blocking` on a short body is a pessimization — the hand-off costs more
  than the work.
* Schema, argument slots, `Result` handling and the response are identical
  across shapes. Nothing about the wire changes.
* A `blocking` body is **not cancelled** when the request is; a panic inside
  it propagates to the awaiting task.
* A handler that needs `Context` (elicitation, sampling, progress,
  `ctx.memo`) has to await, so it must be an `async fn`.

The shape is a defaulted type-level marker — `neva::marker::Async` or
`marker::Immediate` — on the handler traits
(`ToolHandler<Args, M = marker::Async>`). It is inferred at the registration
site and never written in handler code. Bounds written as
`ToolHandler<Args>` mean what they always meant. A call site that spells its
generics out needs one more argument on 0.6:

```rust
// 0.5.x
// app.map_tool::<_, _, (String,)>("greet", greet);
// 0.6.0 — E0107 until the marker parameter is added
// app.map_tool::<_, _, (String,), _>("greet", greet);
```

## Tools

### The macro form

```rust
use neva::prelude::*;

#[tool(descr = "Greets a person by name")]
async fn greet(name: String) -> String {
    format!("Hello, {name}!")
}
```

`#[tool]` accepts:

| Parameter | Purpose |
|---|---|
| `descr` | Human-readable description |
| `title` | Display title |
| `input_schema` | JSON string overriding the generated `inputSchema` |
| `output_schema` | JSON string overriding the generated `outputSchema` |
| `annotations` | `ToolAnnotations` as JSON: `title`, `readOnlyHint`, `destructiveHint`, `idempotentHint`, `openWorldHint` |
| `roles` / `permissions` | Access gates, checked against JWT claims (HTTP only) |
| `middleware` | `[fn, fn]` — per-handler middleware |
| `task_support` | `"required"` marks a tool that must be called as a task |
| `ui` | A `ui://` resource that renders this tool — [MCP Apps](apps.md), `apps` feature |
| `visibility` | `["model"]` / `["app"]` / both — who may call a UI-bound tool. `apps` feature |

**An unknown attribute is a compile error**, not a warning — on `#[tool]`,
`#[resource]`, `#[resources]`, `#[prompt]` and `#[handler]` alike. That is
deliberate: a misspelled `visibility` that was merely ignored would publish an
app-only tool to the agent.

### The builder form

```rust
use neva::prelude::*;

async fn greet(name: String) -> String {
    format!("Hello, {name}!")
}

#[tokio::main]
async fn main() {
    let mut app = App::new().with_options(|opt| opt.with_stdio());

    app.map_tool("greet", greet)
        .with_description("Greets a person by name")
        .with_arg_names(["name"]);

    app.run().await;
}
```

### Argument names — the one real trap

A call's `arguments` are read **by name**, and the published `inputSchema`
has to name the same ones. `App::run` panics at startup when they disagree,
so this is a mistake you find immediately if you run the binary.

* `#[tool]` takes the function's parameter names. Nothing to do.
* A **bare closure** has no parameter names to take — Rust does not keep
  them — so it publishes and reads `arg0`, `arg1`, …

Two ways to fix a closure:

```rust
use neva::{App, map_tool};

#[tokio::main]
async fn main() {
    let mut app = App::new();

    // Reads the names off the closure at expansion time.
    map_tool!(app, "greet", |name: String, age: i32| async move {
        format!("Hello, {name}! You are {age}.")
    })
    .with_description("Greets a person");

    app.run().await;
}
```

```rust
use neva::App;

#[tokio::main]
async fn main() {
    let mut app = App::new();

    app.map_tool("greet", |name: String, age: i32| async move {
        format!("Hello, {name}! You are {age}.")
    })
    .with_arg_names(["name", "age"]);

    app.run().await;
}
```

Only value-carrying parameters are named. `Context`, `Meta<_>` and a
DI-injected `Dc<T>` occupy no argument slot and are skipped in both the
schema and the name list. An `Option<T>` *does* occupy a slot.

A schema you supplied yourself — via `input_schema = "…"` or
`with_input_schema(…)` — is taken **verbatim** and never renamed. Name its
properties exactly as you name the arguments.

### Optional arguments

```rust
use neva::prelude::*;

#[tool(descr = "Greets a person, by nickname when there is one")]
async fn greet(name: String, alias: Option<String>) -> String {
    format!("Hello, {}!", alias.unwrap_or(name))
}
```

`Option<T>` is published as its inner `T` and left out of `required`; an
omitted argument arrives as `None`. A tool whose arguments are all optional
publishes no `required` key. Works through type aliases, and
`Option<Json<T>>` still describes `T` in full.

### Schemas

Generated schemas are full **JSON Schema 2020-12** documents. Override
either one with a JSON string:

```rust
use neva::prelude::*;

#[tool(
    descr = "Fetches a tenant's dashboard",
    input_schema = r#"{
        "properties": {
            "tenant": { "type": "string", "description": "Tenant identifier" }
        },
        "required": ["tenant"]
    }"#
)]
async fn dashboard(tenant: String) -> String {
    format!("Dashboard for {tenant}")
}
```

A schema you write is published **verbatim** — `default`, `pattern`,
`examples`, `$schema`, `$defs`, `$ref`, `additionalProperties`,
`allOf`/`anyOf`, `if`/`then`/`else` all survive, at the root and below it.
`"integer"` is its own type, not an alias for `"number"`: a field declared
`integer` rejects `1.5` and accepts `1.0`.

### Mirroring an argument into a header

Annotate a property with `x-mcp-header` and clients mirror the value into
`Mcp-Param-{name}` on `tools/call`, so proxies can route on it without
parsing the body:

```rust
use neva::prelude::*;

#[tool(
    descr = "Fetches a tenant's dashboard",
    input_schema = r#"{
        "properties": {
            "tenant": {
                "type": "string",
                "x-mcp-header": true
            }
        },
        "required": ["tenant"]
    }"#
)]
async fn dashboard(tenant: String) -> String {
    format!("Dashboard for {tenant}")
}
```

A definition that breaks the spec's constraints — non-token name,
duplicate, non-primitive type, or a property not statically reachable
through `properties` — drops the **whole tool** from the listing, so one
bad annotation cannot change what a good one sends. Clients cache the
annotations for the listing's `ttlMs` (an absent `ttlMs` reads as `0`) and
re-list once on a `HeaderMismatch`.

## Prompts

```rust
use neva::prelude::*;

#[prompt(descr = "Asks for a hello-world program")]
async fn hello_world_code(lang: String, tone: Option<String>) -> PromptMessage {
    let tone = tone.unwrap_or_else(|| "neutral".into());
    PromptMessage::user()
        .with(format!("Write a hello-world function in {lang}, tone: {tone}"))
}
```

`Option<T>` publishes `"required": false`, same rule as tools. The closure
counterpart of `map_tool!` is `map_prompt!`, and `Prompt::with_args(...)`
is the explicit form — both set the published arguments and the extraction
names together.

Argument metadata can also be given as JSON:

```rust
use neva::prelude::*;

#[prompt(
    descr = "Asks for a hello-world program",
    args = r#"[
        { "name": "lang", "description": "A language to use", "required": true }
    ]"#
)]
async fn hello_world_code(lang: String) -> PromptMessage {
    PromptMessage::user().with(format!("Write a hello-world function in {lang}"))
}
```

An `Err` from a prompt handler is a **JSON-RPC error**, not a tool error.

## Resources

A resource template, with URI parameters bound to handler parameters:

```rust
use neva::prelude::*;

#[resource(
    uri = "res://{name}",
    title = "Read resource",
    descr = "Some details about a resource",
    mime = "text/plain"
)]
async fn get_res(uri: Uri, name: String) -> ResourceContents {
    ResourceContents::new(uri).with_text(format!("contents of {name}"))
}
```

A static listing, with `#[resources]`:

```rust
use neva::prelude::*;

#[resources]
async fn list_resources(_params: ListResourcesRequestParams) -> impl Into<ListResourcesResult> {
    [
        Resource::new("res://one", "one").with_descr("first").with_mime("text/plain"),
        Resource::new("res://two", "two").with_descr("second").with_mime("text/plain"),
    ]
}
```

`ResourceContents` carries text, JSON or binary:

```rust
use neva::prelude::*;
use serde_json::json;

#[resource(uri = "config://{key}", title = "Read config")]
async fn get_config(uri: Uri, key: String) -> ResourceContents {
    ResourceContents::new(uri).with_json(json!({ "key": key, "enabled": true }))
}

#[resource(uri = "file://{path}", title = "Read file", mime = "application/octet-stream")]
async fn get_file(uri: Uri, path: String) -> ResourceContents {
    let bytes = std::fs::read(&path).unwrap_or_default();
    ResourceContents::new(uri).with_blob(bytes)
}
```

Like prompts, an `Err` from a resource handler is a JSON-RPC error.

A resource whose URI starts with `ui://` is an **MCP App** document: the
scheme is reserved, the macro supplies the `text/html;profile=mcp-app` MIME
type, and `ui_meta` carries the security block. See `apps.md`.

## Return types for tools

| Return | Produces |
|---|---|
| `String` / `&str` | Text content, no `outputSchema` |
| `Json<T>` | Structured content plus an `outputSchema` derived from `T` |
| `Result<T, Error>` | `Ok` as above; `Err` becomes a **tool error**. No `outputSchema` |
| `CallToolResponse` | Full control — but see the trap below |

### Two rules the compiler enforces

**`Json<T>` needs `T` to carry schema metadata**, because the macro derives
the `outputSchema` from it. `#[derive(Serialize)]` alone is not enough —
use `#[json_schema(ser)]` (or a `schemars::JsonSchema` derive):

```rust
use neva::prelude::*;

#[json_schema(ser)]
struct WeatherReport {
    city: String,
    temperature_c: f64,
}

#[tool(descr = "Returns weather for a city")]
async fn get_weather(city: String) -> Json<WeatherReport> {
    WeatherReport { city, temperature_c: 22.5 }.into()
}
```

**A bare `-> CallToolResponse` does not compile.** The macro treats any
unrecognised return type as an object and tries to derive an `outputSchema`
from it, and `CallToolResponse` has no schema. Wrap it in a `Result` —
which the macro reads as "no output schema" — or give an explicit
`output_schema`:

```rust
use neva::prelude::*;

#[tool(descr = "Returns a chart")]
async fn chart(data: String) -> Result<CallToolResponse, Error> {
    let png: Vec<u8> = render(&data);
    Ok(CallToolResponse::from(
        ImageContent::new(png).with_mime("image/png"),
    ))
}

fn render(_data: &str) -> Vec<u8> { Vec::new() }
```

The same applies to any other type that does not derive `JsonSchema`.
`String`, `&str`, numbers, `bool`, `Vec<_>`, `Option<_>` and `Result<_, _>`
are all recognised and produce no output schema.

### Content items

`with_mime` lives on the concrete content struct, not on the `Content`
enum, so build the struct and let it convert:

* `ImageContent::new(bytes)` — default MIME `image/jpg`
* `AudioContent::new(bytes)` — default MIME `audio/wav`
* `Content::text(s)`, `Content::json(v)`, `Content::resource(contents)`,
  `Content::link(resource)`
* `CallToolResponse::array([a, b])` for several items at once

`CallToolResponse::error` takes an `Error`, not a string:

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

## The Context

Inject `Context` into any handler to reach the server itself:

```rust
use neva::prelude::*;

#[tool(descr = "Reads a resource this server also exposes")]
async fn read_resource(ctx: Context, res: Uri) -> Result<Content, Error> {
    let result = ctx.resource(res).await?;
    let resource = result.contents.into_iter().next()
        .ok_or_else(|| Error::new(ErrorCode::InternalError, "no contents"))?;
    Ok(Content::resource(resource))
}
```

`Context` also mutates the registries at runtime — `add_tool`,
`remove_tool`, `add_prompt`, `remove_prompt`, `add_resource`,
`remove_resource`, `resource_updated` — and each mutation notifies every
subscriber that asked for that category. `add_tool` / `add_prompt` run the
same argument-name check `App::run` does and return an error rather than
publishing something no peer could call.

For asking the *client* for input (`elicit`, `sample`, `list_roots`) and
the re-run primitives (`memo`, `once`, `on_commit`), read `mrtr.md`.

### What this caller declared

There is no handshake, so a client declares its capabilities on **every
request**, in `_meta` under `io.modelcontextprotocol/clientCapabilities`.
`Context` reads back what the caller of *this* request declared:

| Ask | Answers |
|---|---|
| `ctx.client_capabilities()` | The MRTR kinds — which input requests this caller can answer |
| `ctx.client_extension(id)` | Settings this caller declared for extension `id`, or `None` |
| `ctx.supports_apps()` | Whether this caller can render MCP Apps (`apps` feature) |

```rust
use neva::prelude::*;

#[tool(descr = "Searches the corpus")]
async fn search(ctx: Context, query: String) -> String {
    let fuzzy = ctx
        .client_extension("com.example/search")
        .and_then(|settings| settings["fuzzy"].as_bool())
        .unwrap_or(false);

    format!("searching for {query} (fuzzy: {fuzzy})")
}
```

`client_extension` and `supports_apps` are **new in 0.6.0** and are not
available under `legacy-spec`, where capabilities ride `initialize` instead.
Presence of an id is the declaration; what counts as *supporting* the
extension is the extension's own rule — MCP Apps requires its settings to
name the content types the client renders, which is what `supports_apps()`
checks and `client_extension()` does not. A malformed `extensions` value
reads as none declared rather than failing the request.

## Custom JSON-RPC methods

```rust
use neva::prelude::*;

#[handler(command = "custom/status")]
async fn status_handler(ctx: Context, req: Request) -> String {
    format!("method={}", req.method)
}
```

Handler parameters may be `Context`, `Request`, `RequestId` or
`RuntimeMcpOptions`. Pick a method name outside the standard namespace —
this is also how you replace the removed `ping`.

## Completion

```rust
use neva::prelude::*;

#[completion]
async fn complete_language(params: CompleteRequestParams) -> Completion {
    let filter = params.arg.value.to_lowercase();
    let matched: Vec<String> = ["Rust", "Go", "Python"]
        .iter()
        .filter(|l| l.to_lowercase().starts_with(&filter))
        .map(|l| l.to_string())
        .collect();

    let total = matched.len();
    Completion::new(matched, total)
}
```

## Dependency injection

Enabled by `server-full`, or the `di` feature on its own. Three lifetimes:
**singleton** (once, shared everywhere), **scoped** (once per incoming
message), **transient** (every resolution).

```rust
use neva::prelude::*;

#[derive(Clone)]
struct AppConfig {
    greeting: String,
}

#[tool(descr = "Greets using the configured greeting")]
async fn hello(config: Dc<AppConfig>, name: String) -> String {
    format!("{}, {name}!", config.greeting)
}

#[tokio::main]
async fn main() {
    App::new()
        .with_options(|opt| opt.with_stdio())
        .add_singleton(AppConfig { greeting: "Hello".into() })
        .run()
        .await;
}
```

`Dc<T>` derefs to `T`; `.cloned()` gives an owned value. Registration
methods: `add_singleton(value)`, `add_scoped::<T>()` /
`add_scoped_factory(f)` / `add_scoped_default::<T>()`, and the three
`add_transient*` counterparts. Implement `Inject` when a service resolves
its own dependencies:

```rust
use neva::prelude::*;
use neva::di::{Container, DiError, Inject};

#[derive(Clone)]
struct AppConfig { api_url: String }

#[derive(Clone)]
struct ApiClient { base_url: String }

impl Inject for ApiClient {
    fn inject(container: &Container) -> Result<Self, DiError> {
        let config = container.resolve::<AppConfig>()?;
        Ok(Self { base_url: config.api_url.clone() })
    }
}
```

The prelude carries `Dc` but not `Inject` / `Container` / `DiError` —
import those from `neva::di`.

In middleware, use `ctx.resolve::<T>()` or `ctx.resolve_shared::<T>()`.

## Middleware

```rust
use neva::prelude::*;

async fn logging(ctx: MwContext, next: Next) -> Response {
    let id = ctx.id();
    tracing::info!("start {id:?}");
    let resp = next(ctx).await;
    tracing::info!("end {id:?}");
    resp
}

#[tokio::main]
async fn main() {
    App::new()
        .with_options(|opt| opt.with_stdio())
        .wrap(logging)          // every request
        .run()
        .await;
}
```

`wrap` is global, `wrap_tools` covers every `tools/call`, and
`middleware = [f]` on `#[tool]` / `#[prompt]` / `#[handler]` is
per-handler. They run in that order, handler last. Short-circuit by
returning `Response::error(ctx.id(), err)` instead of calling `next`.

## Logging and progress

Both are **request-scoped**: they ride the response stream of the request
that triggered them, and need no subscription. Install the layer once:

```rust
use neva::prelude::*;
use neva::types::notification;
use tracing_subscriber::prelude::*;

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(notification::fmt::layer())
        .init();

    App::new()
        .with_options(|opt| opt.with_default_http())
        .run()
        .await;
}
```

Then log normally from handlers; the optional `logger` field names the
source:

```rust
use neva::prelude::*;

#[tool]
async fn my_tool() {
    tracing::info!(logger = "my_tool", "processing started");
}
```

Progress needs the client to have sent a token, which arrives as
`Meta<ProgressToken>`:

```rust
use neva::prelude::*;

#[tool]
async fn long_running(token: Meta<ProgressToken>, command: String) {
    tracing::info!("starting {command}");
    tracing::info!(target: "progress", token = %token, progress = 50, total = 100);
}
```

Over HTTP, a `POST` whose `_meta` carries `logLevel` or a `progressToken`
gets a `text/event-stream` reply instead of a single JSON object. That is
automatic.

## Subscriptions

Server-initiated notifications ride a client-opened `subscriptions/listen`
request. **There is no handler to write** — neva answers it and fans your
existing `Context` calls out. Your only job is to advertise what you can
push:

```rust
use neva::prelude::*;

#[tokio::main]
async fn main() {
    App::new()
        .with_options(|opt| opt
            .with_default_http()
            .with_tools(|tools| tools.with_list_changed())
            .with_prompts(|prompts| prompts.with_list_changed())
            .with_resources(|res| res.with_list_changed().with_subscribe()))
        .run()
        .await;
}
```

| Capability | Notification it enables |
|---|---|
| `tools.listChanged` | `notifications/tools/list_changed` |
| `prompts.listChanged` | `notifications/prompts/list_changed` |
| `resources.listChanged` | `notifications/resources/list_changed` |
| `resources.subscribe` | `notifications/resources/updated` |

A category the client asks for but the server does not advertise is
**dropped from the acknowledgment**, not refused — the subscription still
opens.

`Context::is_subscribed(&uri)` answers from the live streams, so you can
skip **expensive local work** nobody will receive. Do not use it to decide
whether to notify: it is node-local, and it can only answer for the instance
running the handler. `Context::resource_updated` therefore does not pre-check
it — it publishes unconditionally and lets the subscription filters route the
result, which is what they already do.

### Fanning out across instances

A `subscriptions/listen` stream is a socket held by exactly one process, and
the stateless transport pins nothing — so the subscriber and the request
that mutates the server routinely land on different instances, and the
notification is silently lost. `App::with_notification_bus(..)` carries them
across:

```rust
use neva::prelude::*;
use neva::app::notification_bus::{BusNotification, NotificationBus};
use neva::shared::Stream;
use tokio::sync::broadcast::{Sender, channel, error::RecvError};

/// Stands in for Redis pub/sub or NATS: one channel every instance
/// publishes to and reads back from, echo included.
struct BroadcastBus(Sender<BusNotification>);

impl NotificationBus for BroadcastBus {
    async fn publish(&self, notification: BusNotification) {
        // Nobody draining yet is not worth failing a request over.
        let _ = self.0.send(notification);
    }

    fn subscribe(&self) -> impl Stream<Item = BusNotification> + Send + 'static {
        let rx = self.0.subscribe();
        futures_util::stream::unfold(rx, |mut rx| async move {
            loop {
                match rx.recv().await {
                    Ok(notification) => return Some((notification, rx)),
                    // At-most-once: skip what was missed rather than
                    // end delivery for good.
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => return None,
                }
            }
        })
    }
}

#[tokio::main]
async fn main() {
    let (tx, _) = channel(64);

    App::new()
        .with_notification_bus(BroadcastBus(tx))
        .with_options(|opt| opt.with_default_http())
        .run()
        .await;
}
```

Contract, all four of which matter:

* **No echo suppression.** `subscribe` must yield this instance's own
  publishes — local delivery goes through that stream and only through it,
  so a bus that hides them silences its own subscribers. Redis pub/sub,
  NATS and `tokio::sync::broadcast` all echo by default.
* **At-most-once is enough.** A full subscription buffer drops the
  notification with a warning rather than blocking the producing request;
  subscriptions are not resumable by spec anyway.
* **`publish` is awaited inside the producing request** — hand off to a
  background connection rather than waiting for a round trip.
* **A stream that ends stops delivery for the process's life** — reconnect
  *inside* the stream.

The subscriber table itself stays node-local by construction: half of every
entry is a handle to a socket on one node. Nothing changes without a bus —
there is none by default, and a single-instance server pays nothing.

### Shutdown answers the streams first

A server ending a subscription on its own initiative SHOULD send the empty
result first. Shutdown is two-phase for that reason — see `http.md` for
`App::with_shutdown()` and `with_shutdown_drain(..)`. `App::run` waits for the
transport writers to put those results on the wire before returning, so the
result survives a runtime dropped right after.

## Access control

`roles` and `permissions` on a primitive are checked against JWT claims and
answer `403` when unsatisfied. See `http.md` for configuring the auth that
produces those claims — either a locally minted JWT (`set_decoding_key`) or
an OAuth 2.1 issuer (`with_oauth(|o| o.with_issuer(..))`). The gates are the
same either way.

```rust
use neva::prelude::*;

#[tool(roles = ["admin"], permissions = ["read"])]
async fn admin_tool(name: String) {
    tracing::info!("admin tool for {name}");
}
```
