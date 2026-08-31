# Multi round-trip requests (MRTR)

MRTR is how a server handler asks the caller for something mid-execution —
a form, a URL to open, a model completion, a roots listing — and gets an
answer without a server→client push channel.

**This is the section models get wrong most often.** Read the execution
model before writing a handler that elicits.

## The execution model

```
client                              server
  |-- tools/call ------------------->|  handler runs
  |                                  |  ctx.elicit("k", params)  -> no answer yet
  |<-- result: input_required -------|  handler UNWINDS
  |   (+ sealed requestState)        |
  |                                  |
  |-- tools/call (same args, ------->|  handler runs AGAIN, from the top
  |   + inputResponses               |  ctx.elicit("k", params)  -> replays the answer
  |   + requestState)                |  handler continues to the end
  |<-- result: complete -------------|
```

Three consequences, all load-bearing:

1. **`elicit` takes a replay key.** `ctx.elicit(key, params)` — the key is
   how the answer is matched back to this call site on the next round. It
   must be stable across rounds.
2. **The handler re-runs from the top every round.** Code above an elicit
   point executes again. Side effects there happen again.
3. **State travels in an AEAD-sealed `requestState` blob** the client
   echoes back, so any round can land on any server instance.

On the client side neva drives the loop inside `call_tool`; the caller
sees one call.

## Guarding side effects

Three primitives, and you need them:

| Primitive | Guarantee |
|---|---|
| `ctx.memo(key, fut)` | Computed once; replayed from `requestState` on later rounds |
| `ctx.once(key, fut)` | Runs at most once across all rounds |
| `ctx.on_commit(fut)` | Runs exactly once, when the handler reaches its final result |

```rust
use neva::prelude::*;

#[json_schema(de)]
struct Shipping {
    full_name: String,
    address: String,
}

#[tool(descr = "Places an order")]
async fn place_order(mut ctx: Context) -> Result<String, Error> {
    // Fetched once; replayed on every later round.
    let quote_cents: u32 = ctx.memo("quote", async { Ok(1299) }).await?;

    let form = ElicitRequestParams::form(format!(
        "Shipping is ${:.2}. Your details?",
        quote_cents as f64 / 100.0
    ))
    .with_schema::<Shipping>();

    // Round 1 unwinds here; round 2 replays the answer.
    let ship: Shipping = ctx
        .elicit("shipping", form.into())
        .await?
        .content()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams, "declined"))?;

    // At most once across all rounds.
    ctx.once("charge", async { Ok(()) }).await?;

    // Exactly once, on the final round.
    let who = ship.full_name.clone();
    ctx.on_commit(async move {
        tracing::info!("receipt sent to {who}");
        Ok(())
    });

    Ok(format!("Order confirmed for {}", ship.full_name))
}
```

Rule of thumb: if a line above an elicit point talks to the outside world
— charges money, sends mail, writes a row, calls a paid API — it belongs
inside `memo`, `once` or `on_commit`. If it is a pure computation, leaving
it to re-run is fine but `memo` saves the work.

## Ask only for what the caller declared

Capabilities are per **request**, in its `_meta`, so they describe the
caller of *this* call and not some earlier handshake. Requesting a kind the
caller did not declare ends the call with `MissingRequiredClientCapability`
(`-32021`) — it does not degrade.

```rust
use neva::prelude::*;

#[tool(descr = "Greets, asking for a name when it can")]
async fn greet(mut ctx: Context) -> Result<String, Error> {
    if ctx.client_capabilities().elicitation.is_none() {
        return Ok("Hello, stranger!".to_string());
    }

    let params = ElicitRequestParams::form("Your name?")
        .with_required("name", "string")
        .into();

    let res = ctx.elicit("name", params).await?;
    Ok(format!("{:?}", res.content))
}
```

`elicitation` is not a flag but an `Option<ElicitationModes>` carrying
`form` and `url`:

* a client that **named modes** is listing what it can do — a mode missing
  from the list is one it cannot answer;
* a client that declared `elicitation` but named no mode (`{}`) has ruled
  nothing out (`unconstrained()` is `true`).

`modes.allows(&params)` answers the whole question for either shape:

```rust
use neva::prelude::*;

#[tool(descr = "Takes a payment")]
async fn pay(mut ctx: Context) -> Result<String, Error> {
    let params: ElicitRequestParams = ElicitRequestParams::url(
        "https://example.com/pay",
        "Please pay your bill",
    ).into();

    match ctx.client_capabilities().elicitation {
        Some(modes) if modes.allows(&params) => {
            ctx.elicit("payment", params).await?;
            Ok("Payment received".into())
        }
        // Declared elicitation, but not this mode — take the other path.
        _ => Ok("Invoice sent instead".into()),
    }
}
```

## Form elicitation

```rust
use neva::prelude::*;

#[json_schema(de)]
struct Contact {
    name: String,
    email: String,
    age: u32,
}

#[tool(descr = "Generates a business card")]
async fn generate_business_card(mut ctx: Context) -> Result<String, Error> {
    let params = ElicitRequestParams::form("Your contact information")
        .with_schema::<Contact>();

    let contact: Contact = ctx
        .elicit("contact", params.into())
        .await?
        .content()
        .ok_or_else(|| Error::new(ErrorCode::InvalidParams, "declined"))?;

    Ok(format!("{} <{}>, {}", contact.name, contact.email, contact.age))
}
```

`with_schema::<T>()` needs `T` to carry schema metadata — that is what
`#[json_schema(de)]` provides. `with_required(name, type)` builds a schema
field by field when a struct is overkill.

## URL elicitation

For an action the user performs elsewhere — a payment, an SSO redirect:

```rust
use neva::prelude::*;

#[tool(descr = "Pays a bill")]
async fn pay_a_bill(mut ctx: Context) -> Result<&'static str, Error> {
    let params = ElicitRequestParams::url(
        "https://example.com/pay",
        "Please pay your bill",
    );

    ctx.elicit("payment", params.into()).await?;
    Ok("Payment successful")
}
```

There is **no `elicitationId`** and no `notifications/elicitation/complete`
in this generation: answering the request is the completion signal. A
server that needs to track an elicitation across retries encodes its own
identifier in `requestState`, e.g. via `ctx.memo`.

## Sampling and roots

Both survive as MRTR input-request kinds and both are `#[deprecated]` on
arrival, matching the spec's own 12-month lifecycle. The mechanics are
identical — a replay key, an unwind, a re-run — so `memo` / `once` /
`on_commit` cover them unchanged. Call sites need `#[allow(deprecated)]`.

Do not build new functionality on them. Do not rip them out of code that
already uses them either; they work.

The Rust union is `mrtr::InputRequest` (`Elicitation` / `Sampling` /
`Roots`), and `mrtr::InputResponses` is
`HashMap<String, serde_json::Value>` — the result type depends on the kind,
so deserialize your own type out of the value.

## Tasks are the other model — they suspend

A task genuinely suspends rather than re-running, so
`ctx.task().elicit(params)` takes **no replay key**:

```rust
use neva::prelude::*;

#[tool(task_support = "required", descr = "Asks mid-task")]
async fn confirm_in_task(mut ctx: Context, task: Meta<RelatedTaskMetadata>) -> String {
    let params = ElicitRequestParams::form("Are you sure?")
        .with_related_task(task);

    let res = ctx.task().elicit(params.into()).await;
    format!("{:?}", res.map(|r| r.action))
}
```

`Meta<RelatedTaskMetadata>` is injected by the framework and passed to
`with_related_task` so the client can correlate the request with the
running task. Enable the extension with `opt.with_tasks()` on the server —
no closure, the capability is an empty object.

There is **no task-augmented sampling** in this generation.

## Multi-instance deployments

Because any round can land on any instance, two shared resources become
mandatory as soon as you run more than one:

```rust
use neva::prelude::*;

#[tokio::main]
async fn main() {
    App::new()
        // Without this, cross-instance retries cannot decrypt `requestState`.
        // neva warns at startup if it is missing.
        .with_request_state_secret(std::env::var("MCP_STATE_SECRET").unwrap().as_bytes())
        .with_options(|opt| opt.with_default_http())
        .run()
        .await;
}
```

Also set `App::with_request_state_store(<shared store>)` — the default
`InMemoryStateStore` is per-process, and without a shared one a
lost-response retry re-runs the handler and double-fires `on_commit`.
Implement `RequestStateStore` over Redis or similar.

`requestState` is **sealed** with ChaCha20-Poly1305, not merely signed:
`ctx.memo` writes server-computed values (an upstream response, a quoted
price, a downstream token) into it, and a signed blob would still be
readable. Treat the secret as a secret; rotate with
`App::with_request_state_keys`.

## Client side

One handler and a capability declaration — see `client.md`. Cap the
re-issues per slot with `McpOptions::with_max_mrtr_rounds`.
