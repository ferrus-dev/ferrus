# The client

Configuring `Client`: the server, the key, timeouts, layers, a custom HTTP
backend, and tests that need no server.

## Contents

- [The builder](#the-builder)
- [The base URL](#the-base-url)
- [API keys](#api-keys)
- [Extra headers](#extra-headers)
- [Timeouts](#timeouts)
- [Listing models](#listing-models)
- [Layers](#layers)
- [A layer of one's own](#a-layer-of-ones-own)
- [Compatibility handling](#compatibility-handling)
- [TLS and the crypto provider](#tls-and-the-crypto-provider)
- [A custom HTTP backend](#a-custom-http-backend)
- [Tests without a server](#tests-without-a-server)

## The builder

`Client::openai(url)` returns a `ClientBuilder`; every method takes `self`
and returns it; `build()` returns `Result<Client, Error>`.

| Method | Effect | Default |
|---|---|---|
| `api_key(key)` / `api_key_env(name)` / `api_key_file(path)` | Bearer authentication | None |
| `header(name, value)` | A header sent with every request, for a gateway or a hosted endpoint | None |
| `allow_http()` | Plain HTTP to a host that is not loopback | Refused |
| `connect_timeout(d)` | How long connecting may take | 10 s |
| `idle_timeout(d)` / `no_idle_timeout()` | How long the server may send nothing | 5 min |
| `include_usage(bool)` | Ask for token counts when a request does not say | On |
| `lenient()` / `mode(Mode)` | How strictly responses are read | Strict |
| `limits(Limits)` | Bounds on one response | 64 MiB, 256 KiB per event, 64 tool calls |
| `think(Think)` | What to do with inline `<think>` tags | Split into reasoning |
| `context_tokens(n)` | Refuse a request that cannot fit, before sending it | Off |
| `http(backend)` | Another HTTP backend | hyper |
| `layer(l)` / `wrap(closure)` | Middleware around every call | None |

```rust
use std::time::Duration;

use svir::layer::{Retry, Timeout};
use svir::prelude::*;

fn client() -> Result<Client, Error> {
    Client::openai("https://models.example.com/v1")
        .api_key_env("MODELS_API_KEY")
        .connect_timeout(Duration::from_secs(5))
        .layer(Retry::connect(3))
        .layer(Timeout::first_token(Duration::from_secs(120)))
        .build()
}
```

`Client` is cheap to clone. Build one per server and clone it: clones share
the connection pool and what was learned about the server. Its `Debug` shows
the URL and never the key or a header's value.

`context_tokens(n)` counts the request body's bytes as tokens, which never
underestimates text but overestimates images by far. With image attachments,
leave it off.

## The base URL

Pass the server's base, with or without `/v1` and a trailing slash; svir
appends `/v1/chat/completions` and `/v1/models`. A path in front is kept:
`http://host/openai/v1` calls `http://host/openai/v1/chat/completions`.

`build()` fails with `ErrorKind::Config` for:

| The URL | Why |
|---|---|
| `http://` to a host that is not loopback | Keys and prompts would cross the network in the clear. Use `https://`, or `.allow_http()` for a trusted network |
| `https://` without the `tls` or the `tls-aws-lc` feature | Neither is on |
| Credentials, a query, or a fragment in the URL | A key belongs in `api_key`, not in the URL |
| No scheme (`127.0.0.1:1234`) | Write `http://127.0.0.1:1234` |

Loopback is `localhost`, `127.0.0.1`, and `[::1]`.

## API keys

| Source | Read |
|---|---|
| `.api_key("...")` | The value given |
| `.api_key_env("NAME")` | The variable, when the client is built; unset is a `Config` error |
| `.api_key_file(path)` | The file, when the client is built; surrounding whitespace is dropped |

svir reads no variable and no `.env` file unless told to. The key is sent as
`Authorization: Bearer ...` and never appears in `Debug`, `Display`, errors,
or events. A local server without authentication needs no key at all.

## Extra headers

A gateway or a hosted endpoint may ask for a header of its own: attribution,
an organization or a project, or a key under a name other than
`Authorization`. `header(name, value)` adds one to every request the client
sends, the model listing included. Requires svir 0.1.4.

```rust
use svir::prelude::*;

fn client() -> Result<Client, Error> {
    Client::openai("https://gateway.example.com/v1")
        .api_key_env("GATEWAY_KEY")
        .header("x-title", "My App")
        .build()
}
```

* Names are not case-sensitive and are sent lowercase; setting a name again
  replaces the earlier value.
* Every value is treated as a credential: withheld from `Debug`, errors, and
  events, and sent marked sensitive by the built-in backend.
* svir reads no environment variable for a header. Read it with
  `std::env::var(..)` and pass the value.
* `build()` fails with `ErrorKind::Config` for a name that is not a header
  name, a value with a line break, another control character, or text that
  is not ASCII, and the headers svir writes itself or that frame the
  request: `authorization`, `content-type`, `content-length`, `accept`,
  `host`, `transfer-encoding`, `connection`. **The Bearer key goes through
  `api_key`, never `.header("authorization", ..)`.** A key a gateway wants
  under another name, `api-key` or `x-api-key`, goes through `header`.
* The headers are the client's, the same on every request. There are no
  headers per request; a layer works above HTTP and cannot add one.

## Timeouts

| What | How | Default |
|---|---|---|
| Connecting | `connect_timeout(d)` | 10 s |
| Silence from the server, before the response and during it | `idle_timeout(d)` | 5 min |
| Time to the first piece of the answer | `Timeout::first_token(d)` layer | None |
| The whole answer | `Timeout::total(d)` layer | None |

There is no total timeout by default, on purpose: a long generation is not a
failure. The idle timeout is long because a local model reads the whole
prompt before its first token; shorten it for a hosted server, not for a
local one.

All of them fail with `ErrorKind::Timeout`.

## Listing models

```rust
use svir::prelude::*;

async fn models(client: &Client) -> Result<Vec<String>, Error> {
    let listed = client.list_models().await?;

    Ok(listed.into_iter().map(|model| model.id).collect())
}
```

The listing is what the server reports, unfiltered: embedding and speech
models are in it next to chat models. `Model` has `id` and an optional
`name`.

## Layers

A layer wraps the call itself, `Request -> Result<EventStream, Error>`. The
first layer added is the outermost: it sees the request first and the answer
last.

| Layer | Behavior |
|---|---|
| `Retry::connect(n)` | Retries a call whose request never reached the server: a refused or timed-out connection. Always safe |
| `Retry::transient(n)` | Also retries timeouts, rate limits, and server errors that came before the answer started |
| `.backoff(d)` on either | The wait before the first retry, 500 ms unless set; it doubles each time, up to 30 s. A `Retry-After` from the server wins |
| `Timeout::first_token(d)` | Nothing of the answer (text, reasoning, a piece of a tool call) within `d` of the call |
| `Timeout::idle(d)` | The server silent for `d` |
| `Timeout::total(d)` | The answer not complete within `d` of the call |
| `Trace` (feature `tracing`) | Reports each call through `tracing`: model, timings, finish, token counts, failures. Never text, reasoning, or the server's messages |

What to know before adding `Retry`:

* **Only a call that failed before the response started is retried.** Once
  the answer streams, nothing is: the caller has seen part of it.
* `Retry::transient` can repeat a request the server did receive. That is
  fine for a model call, which changes nothing on the server, and it costs
  tokens.
* Put `Retry` outside `Timeout` (add it first) so that each attempt gets its
  own deadline.
* Layers work on events. `client.send()` and `client.list_models()` do not
  pass through them.

`Trace` is a unit struct, added like any other layer. It is the place to
start before writing a logging layer of one's own:

```rust
use svir::layer::Trace;
use svir::prelude::*;

fn traced() -> Result<Client, Error> {
    Client::openai("http://127.0.0.1:1234").layer(Trace).build()
}
```

| Level | Message | Fields |
|---|---|---|
| `debug` | `request` | `model`, `messages` (how many) |
| `debug` | `response started` | `model`, `elapsed_ms` |
| `info` | `answer complete` | `model`, `finish`, `input_tokens`, `output_tokens`, `elapsed_ms` |
| `warn` | `request failed`, `answer failed` | `model`, `kind`, `elapsed_ms` |

Nothing is printed until the application installs a `tracing` subscriber.

A closure is a layer through `wrap`:

```rust
use std::time::Instant;

use svir::prelude::*;

fn client() -> Result<Client, Error> {
    Client::openai("http://127.0.0.1:1234")
        .wrap(|request, next| async move {
            let model = request.model.clone();
            let started = Instant::now();
            let answer = next.run(request).await;

            match &answer {
                Ok(_) => eprintln!("{model}: response after {:?}", started.elapsed()),
                Err(error) => eprintln!("{model}: {error}"),
            }
            answer
        })
        .build()
}
```

`next.run(request)` resolves when the response **starts**, not when the
answer is complete. To see the whole answer, watch the stream it returns.

## A layer of one's own

Implement `Layer` for a type that holds state. A layer cannot change the
type of the stream, so it shapes the stream through `EventStream`'s own
methods.

```rust
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use svir::layer::{Layer, Next};
use svir::prelude::*;

/// Counts the tokens every answer of a client took to generate.
struct Meter {
    generated: Arc<AtomicU64>,
}

impl Layer for Meter {
    async fn call(&self, request: Request, next: Next) -> Result<EventStream, Error> {
        let generated = self.generated.clone();
        let stream = next.run(request).await?;

        Ok(stream.inspect(move |item| {
            if let Ok(Event::Completed(done)) = item {
                let tokens = done.usage.map_or(0, |usage| usage.output);
                generated.fetch_add(tokens, Ordering::Relaxed);
            }
        }))
    }
}
```

| `EventStream` method | For a layer that |
|---|---|
| `inspect(closure)` | Watches every item, the last one included: metrics, logging, accounting |
| `first_token_by(instant)` | Fails the stream if nothing of the answer arrived by then |
| `complete_by(instant)` | Fails the stream if the answer is not complete by then |
| `idle_timeout(d)` | Fails the stream when the server is silent for `d` |

A layer may change the request before passing it on (a default system
prompt, a model alias), call `next.run` more than once after cloning `next`
(that is what `Retry` does), or not call it at all and return an error. It
cannot rewrite events.

## Compatibility handling

Some servers reject optional fields, `reasoning_effort` and `stream_options`,
with a 400 or 422. The client then sends the request once more without them,
and remembers: later requests of this client and its clones leave them out
from the start. Nothing was generated by the rejected request, so this is
not a retry in the sense above, and it needs no configuration.

The consequence to know: on such a server `Completion::usage` is `None`, and
the reasoning effort is not applied.

Only a rejection whose body does not explain it gets the second attempt. A
context overflow (`ContextOverflow`) or a prompt the content filter blocked
(`ContentFilter`) is reported as it is: sent again, a blocked prompt would be
billed again.

A tool choice and a response format are not optional fields: the answer has
to meet them. The second attempt keeps them, a request that carries them and
no optional field is not sent twice, and a server that does not take them
fails the request with `Unsupported`, the status, and its message. Do not
catch that and resend without them: the answer would not be what the code
acts on.

## TLS and the crypto provider

HTTPS is rustls with the webpki roots and HTTP/2 by ALPN. The crypto provider
is a feature:

| Feature | Provider |
|---|---|
| `tls` (default) | ring. Builds without a C toolchain everywhere |
| `tls-aws-lc` | aws-lc-rs. With both features on, this one is used |

svir passes its provider to rustls explicitly, so it works with either. The
choice matters to the rest of the build: rustls picks a process-wide default
provider only when exactly one is compiled in. If another dependency brings
aws-lc-rs (reqwest can), svir's `tls` adds ring, and code that relies
on the default (`ClientConfig::builder()`) panics. Take the provider the
build already has:

```toml
[dependencies]
svir = { version = "0.1.6", default-features = false, features = ["client", "tls-aws-lc"] }
```

The other fix is in the code that relies on the default: pass a provider
there too, or install one at startup with `CryptoProvider::install_default`.

## A custom HTTP backend

The built-in backend is hyper with rustls: no redirects, no proxies, the
default roots. For a proxy, client certificates, or other roots, implement
`svir::http::Backend` over an HTTP client that has them.

```rust
use svir::http::{Backend, BoxBody, HttpRequest, HttpResponse};
use svir::prelude::*;

struct Mine;

impl Backend for Mine {
    type Body = BoxBody;

    async fn send(&self, request: HttpRequest) -> Result<HttpResponse<BoxBody>, Error> {
        // Send `request.method`, `request.url`, `request.headers`, and
        // `request.body` with your HTTP client, and return the response
        // once its headers have arrived.
        let _ = request;

        Err(Error::new(ErrorKind::Transport).with_detail("not connected"))
    }
}

fn client() -> Result<Client<Mine>, Error> {
    // `http` comes before any layer.
    Client::openai("https://models.example.com").http(Mine).build()
}
```

The contract of `send`:

* Send the request as given and return the response as received. No
  redirects, no retries, no changes to the body.
* `request.headers` holds `authorization` when there is a key and the
  headers added with `.header(..)`. Every value but those of `content-type`
  and `accept` may be a credential: keep it out of logs, and send it marked
  sensitive where the HTTP client can. `HttpRequest`'s `Debug` withholds
  them.
* `request.body` is `Some(HttpBody { length, stream })`. Send it with
  `Content-Length: length`, not chunked: not every model server accepts a
  chunked request. `stream` yields `Result<Bytes, Error>`.
* The response body is any `Stream<Item = Result<Bytes, Error>>` that is
  `Send + Unpin + 'static`. `BoxBody` is the boxed form; a backend that can
  name its stream type avoids the box.
* Report a failure to connect or to read as `ErrorKind::Transport`, a
  timeout as `ErrorKind::Timeout`. Mark a request that never left with
  `.with_unsent()`, which is what `Retry::connect` looks for. Status codes
  are svir's to map: return a 4xx or 5xx as a response, not as an error.
* Dropping the response body must close the exchange: that is how a
  cancelled call stops the generation.

The client's type carries the backend: `Client<Mine>`. `Client` alone means
the built-in one. `.http(..)` after a layer is a `Config` error at `build()`,
since a layer is tied to the backend it was added for.

## Tests without a server

A scripted backend answers from memory. It is the way to test code that
calls a model without a model.

```rust
use bytes::Bytes;
use svir::http::{Backend, BoxBody, HttpRequest, HttpResponse};
use svir::prelude::*;

const ANSWER: &str = concat!(
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"pong\"},\"finish_reason\":null}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n",
);

/// Answers every request with the same stream.
struct Canned;

impl Backend for Canned {
    type Body = BoxBody;

    async fn send(&self, _request: HttpRequest) -> Result<HttpResponse<BoxBody>, Error> {
        let body = futures_util::stream::iter([Ok(Bytes::from_static(ANSWER.as_bytes()))]);
        let headers = vec![("content-type".to_owned(), "text/event-stream".to_owned())];

        Ok(HttpResponse::new(200, headers, Box::pin(body)))
    }
}

async fn pong() -> Result<(), Error> {
    let client = Client::openai("http://127.0.0.1:1").http(Canned).build()?;

    let answer = client.complete(Request::new("m").user("ping")).await?;
    assert_eq!(answer.text, "pong");

    Ok(())
}
```

* The response must say `content-type: text/event-stream`, or the client
  refuses it.
* A strict client needs a well-formed stream: a delta, a chunk with the
  finish reason, then `[DONE]`. A tool call is a delta with `tool_calls` and
  the finish reason `tool_calls`.
* To script several answers, keep a queue in the backend and pop one per
  request. To assert on what was sent, read `request.body` to its end and
  parse it as JSON.
* A failure is scripted by status: a `429` response tests rate-limit
  handling, a stream without `[DONE]` tests truncation.
* This needs `bytes` and `futures-util` as dev-dependencies.
