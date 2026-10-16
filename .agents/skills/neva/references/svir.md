# Handing MCP tools to a model — the svir bridge

[svir](https://docs.rs/svir) is the model side of the same family: function
calling (tool descriptors, calls, results) and a `Toolbox` trait a request
takes its tools from. It has no MCP. neva's `svir` feature implements
`Toolbox` over MCP, so a server's tools are handed to a model as they are.
**New in 0.7.0** — on 0.6 there is no bridge.

## Setup — the feature is in no preset

```toml
neva = { version = "0.8", features = ["full", "svir"] }   # or server-full / client-full
svir = "0.1.6"                                          # the model client itself
```

* `svir` is **not** in `full`, `server-full` or `client-full` (svir is `0.x`).
  Name it.
* neva depends on svir **without default features** — types and `Toolbox`,
  no HTTP client. To call a model, depend on svir directly.
* `svir::Error` converts into `neva::error::Error`, so `?` works across. The
  message is svir's own account; the provider's message (which can name the
  account) stays in the source and never reaches an MCP peer.

## Which toolbox

| Toolbox | Offers | A call is |
|---|---|---|
| `neva::svir::RemoteTools::new(client)` | A connected server's tools | `tools/call` over the client |
| `app.into_toolbox()` | This server's tools; consumes the app, **no transport started** | In-process, through the server's middleware pipeline |
| `app.with_toolbox()` → `(app, tools)` | The same, while `app.run()` still serves MCP | In-process |
| `ctx.tools().toolbox()` | Inside a handler: the server's *other* tools, with this request's claims | In-process |

All four are configured, then `.load().await?` takes a snapshot of the
descriptors (`Toolbox::tools` is synchronous). `.refresh().await?` renews it —
after `notifications/tools/list_changed`, or `ctx.tools().add(..)`. Only tools
in the snapshot can be called.

## The loop

The bridge offers tools and answers calls; the loop is yours. Always bound it.

<!-- snippet: features="full svir" -->
```rust
use neva::{Client, svir::RemoteTools};
use svir::Toolbox;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = Client::new().with_options(|opt| opt.with_default_http());
    client.connect().await?;

    let tools = RemoteTools::new(client).with_prefix("board_").load().await?;

    let model = svir::Client::openai("http://127.0.0.1:1234").build()?;
    let mut request = svir::Request::new("qwen3")
        .system("You keep the user's task board with the tools you have.")
        .tools(&tools)
        .user("Put three tasks on the board, then show it to me.");

    for _ in 0..8 {
        let done = model.complete(&request).await?;
        if done.calls.is_empty() {
            println!("{}", done.text);
            return Ok(());
        }
        // `done.calls`, not `tool_calls`. Calls of one turn run concurrently.
        let results = tools.call_all(&done.calls).await;
        request = request.assistant(done).tool_results(results);
    }
    Err("the model kept calling tools".into())
}
```

`Toolbox` must be in scope (`use svir::Toolbox;`) for `call_all`.
`RemoteTools::new` takes a `Client` or an `Arc<Client>`; `tools.client()`
reaches it again for prompts and resources.

## Choosing what is offered

`.filter(|tool| ..)`, `.rename(|name| ..)`, `.with_prefix("x_")` — on every
toolbox kind.

* Filters add up; each sees the **server's** name, whatever the renames.
* Renames apply in order. A call goes out under the server's name.
* A name must be `[a-zA-Z0-9_-]{1,64}` after the renames, and unique. A tool
  that fails this **fails the whole snapshot** — no silent renaming. MCP names
  may contain `.`: `.rename(|n| n.replace('.', "_"))`. Two servers in one
  request: a prefix each.
* Never offered: task-only tools (`task_support = "required"`); with `apps`,
  tools hidden from the model (`visibility = ["app"]`); in-process, tools
  needing roles/permissions/scopes the caller lacks.

## What the model is told

A tool result is one string: text blocks joined with a blank line; structured
content as JSON **only** when there is no text; an embedded text/JSON resource
as its text/JSON; a resource link as `name: uri`. **Images, audio and binary
resources become an error result** naming what could not be passed on — return
text for a model-facing tool. A tool error (`is_error`) or a refused call is an
error result with its message. Model arguments of `""`, `null` or `{}` are "no
arguments"; any other non-object is told back to the model as its mistake.

## In-process: your own `#[tool]`s

<!-- snippet: features="full svir" -->
```rust
use neva::prelude::*;

#[tool(descr = "Adds two numbers")]
fn add(a: i64, b: i64) -> i64 {
    a + b
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    // Serve over HTTP *and* call in-process.
    let app = App::new().with_options(|opt| opt.with_default_http());
    let (app, tools) = app.with_toolbox();
    tokio::spawn(app.run());

    // Waits for `run` to build the server; fails if it never runs.
    let tools = tools.load().await?;
    let _ = tools;
    Ok(())
}
```

`App::into_toolbox()` is the same without serving anything. Either way a call
runs through the server's **middleware** (auth, rate limits, audit apply to the
model) and the tool gets its `Context` and `Dc<T>` as over MCP. There is **no
peer**: a tool that elicits, samples or lists roots is refused at once, and its
progress/log notifications go nowhere. A toolbox from `into_toolbox` /
`with_toolbox` holds **no claims**, so tools with `roles` / `permissions` /
`scopes` are not offered by it.

## A tool that drives a model

<!-- snippet: features="full svir" -->
```rust
use neva::{di::Dc, prelude::*};
use svir::Toolbox;

#[tool(descr = "Plans the week with the other tools")]
async fn plan_week(ctx: Context, model: Dc<svir::Client>, focus: String) -> Result<String, Error> {
    // This server's other tools, holding this request's claims.
    let tools = ctx.tools().toolbox().load().await?;

    let mut request = svir::Request::new("qwen3")
        .tools(&tools)
        .user(format!("Plan my week around {focus}."));

    for _ in 0..8 {
        let done = model.complete(&request).await?; // svir::Error -> Error
        if done.calls.is_empty() {
            return Ok(done.text);
        }
        let results = tools.call_all(&done.calls).await;
        request = request.assistant(done).tool_results(results);
    }
    Err(Error::new(ErrorCode::InternalError, "the model kept calling tools"))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    App::new()
        .with_options(|opt| opt.with_default_http())
        .add_singleton(svir::Client::openai("http://127.0.0.1:1234").build()?)
        .run()
        .await;
    Ok(())
}
```

* The tool it is taken in is **left out** (a model calling `plan_week` inside
  `plan_week` restarts the loop). `.with_caller()` keeps it, for deliberate
  recursion.
* In-process calls nest **at most 4 deep** across the chain;
  `.with_max_depth(n)` changes it. Past it the call is not made and the model is
  told why.
* `svir::Client` is a cheap `Clone` (an `Arc` inside) — share it through DI as
  above, or clone it into closures.

## Prompts and resources are not tools

A user picks a prompt, an application picks a resource — neither is the
model's to call. Turn them into conversation content:

<!-- snippet: features="full svir" -->
```rust
use neva::Client;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = Client::new().with_options(|opt| opt.with_default_http());
    client.connect().await?;

    let prompt = client.prompts().get("plan_week", [("focus", "the release")]).await?;
    let request = neva::svir::prompt_messages(prompt)?
        .into_iter()
        .fold(svir::Request::new("qwen3"), svir::Request::message);

    let notes = client.resources().read("file:///notes.md").await?;
    let question = neva::svir::resource_parts(notes)?
        .into_iter()
        .fold(svir::Message::user("Summarise these notes."), svir::Message::with);

    let _request = request.message(question);
    client.disconnect().await?;
    Ok(())
}
```

Consecutive same-role prompt messages merge into one turn. A resource's text
or JSON becomes a text file named by its URI; an image blob an image. Audio
and non-image binaries are an **error**, not dropped. A prompt's description
is left out.

## Answering sampling with a model

<!-- snippet: features="full svir" -->
```rust
use neva::prelude::*;
use neva::types::sampling::CreateMessageRequestParams;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let model = svir::Client::openai("http://127.0.0.1:1234").build()?;
    let mut client = Client::new().with_options(|opt| opt.with_default_http());

    #[allow(deprecated)] // sampling is deprecated in MCP 2026-07-28
    client.map_sampling(move |params: CreateMessageRequestParams| {
        let model = model.clone();
        async move {
            let request = neva::svir::sampling_request("qwen3", params)?;
            let done = model.complete(&request).await?;
            neva::svir::sampling_result(&request, done)
        }
    });

    client.connect().await?;
    client.disconnect().await?;
    Ok(())
}
```

The handler returns `Result<CreateMessageResult, Error>` (0.7.0): under
2026-07-28 an `Err` fails the call that asked for the sample; under
`legacy-spec` it answers the server. `sampling_request` carries the system
prompt, messages, tools, `maxTokens`, `temperature`; `toolChoice: none` drops
the tools, and `toolChoice: required` (0.7.1) asks the model for a call, an
`InvalidParams` error with no tools given. A model server may take it and
answer without a call. **Audio and stop sequences are an error.**
`includeContext`, `modelPreferences` and `metadata` are left out — the model is
the client's choice. `sampling_result` gives text, then a `tool_use` per call
(ids kept), with `stopReason` `toolUse` / `endTurn` / `maxTokens` /
`contentFilter` / `refusal` (0.7.1; the text is then the refusal).

## Reference

* Example: `examples/svir` in the neva repository — one `#[tool]` task board,
  over MCP (`RemoteTools`) and in-process (`into_toolbox`).
* svir itself: <https://romanemreis.github.io/svir-docs/> and its own Agent
  Skill there.
