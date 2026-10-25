# The codec, and proxies

`Encoder` and `Decoder` are what the client is built from, and they are
public. Use them to relay a model's stream through a proxy, or to run svir
under an HTTP stack of your own.

## Contents

- [Which piece for which job](#which-piece-for-which-job)
- [A proxy that relays the stream](#a-proxy-that-relays-the-stream)
- [The decoder](#the-decoder)
- [The encoder](#the-encoder)
- [A transport of one's own](#a-transport-of-ones-own)

## Which piece for which job

| The job | Use |
|---|---|
| Call a model and read the answer | `Client::stream` or `complete`. Not this file |
| Relay the server's bytes to someone else, and also know what was said | `Client::send` and a `Decoder` |
| Another HTTP client, but svir's timeouts, status mapping, and layers | A custom `http::Backend`; see `client.md` |
| No svir transport at all: only the body to send and the events to read | `Encoder` and `Decoder`, with `default-features = false` |

Most code that reaches for the codec wants the second row.

## A proxy that relays the stream

A chat backend sits between a browser and a model server. The browser should
get the server's event stream unchanged, and the backend wants the finished
answer to store. `client.send` gives the bytes; a `Decoder` reads them on the
way past.

```rust
use bytes::Bytes;
use svir::openai::chat::Decoder;
use svir::prelude::*;

/// Relays the answer through `forward`, and returns it once it is complete.
async fn relay(
    client: &Client,
    request: &Request,
    mut forward: impl FnMut(Bytes),
) -> Result<Completion, Error> {
    // `send` has done the authentication, the status mapping, and the
    // compatibility handling. What is left is bytes.
    let mut raw = client.send(request).await?;
    let mut decoder = Decoder::lenient();
    let mut answer = None;

    while let Some(bytes) = raw.next().await {
        let bytes = bytes?;
        forward(bytes.clone());

        for event in decoder.push(&bytes) {
            if let Event::Completed(done) = event? {
                answer = Some(done);
            }
        }
    }

    // A stream that ended without its end marker is an error, not a short
    // answer.
    decoder.finish()?;

    answer.ok_or_else(|| Error::new(ErrorKind::TruncatedStream))
}
```

* `send` returns only for a success status with an event stream. A `401` or
  a `429` from the model server is already an `Err` with its kind, and
  `error.status()` has the status itself; do not relay it as a stream.
* Decide what the proxy does when decoding fails after bytes were forwarded.
  The downstream has them already; the proxy can only stop and mark the
  answer as failed.
* A lenient decoder suits a proxy: the downstream client, not the proxy, is
  the judge of fields svir does not know.
* `RawStream` fails with `ErrorKind::Timeout` when the server goes silent,
  and dropping it cancels the request, exactly as `EventStream` does.
* `send` does not pass through layers: they work on events.
* An upstream `401` means the model server refused the proxy's key. It is
  not the proxy's own client being unauthenticated; keep the two apart in
  what the proxy answers.

## The decoder

`Decoder` is push-based and does no I/O: bytes in, events out.

```rust
use svir::openai::chat::Decoder;
use svir::prelude::*;

const RESPONSE: &str = concat!(
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n",
);

fn decode() -> Result<Option<Completion>, Error> {
    let mut decoder = Decoder::strict();
    let mut answer = None;

    // The pieces can be cut anywhere: inside a line, a string, a character.
    for piece in RESPONSE.as_bytes().chunks(7) {
        for event in decoder.push(piece) {
            if let Event::Completed(done) = event? {
                answer = Some(done);
            }
        }
    }
    decoder.finish()?;

    Ok(answer)
}
```

| Call | Does |
|---|---|
| `Decoder::strict()` / `Decoder::lenient()` / `Decoder::new(mode)` | A decoder for one response |
| `.limits(limits)` / `.think(think)` | Set before the first `push` |
| `push(&bytes)` | Returns what these bytes complete, as `Vec<Result<Event, Error>>` in wire order |
| `is_done()` | Whether the stream completed or failed |
| `finish()` | Ends the input; a stream that never completed is `TruncatedStream` |

Rules the decoder keeps, and code around it can rely on:

* At most one item of a `push` is an error, and it is the last. After
  `Event::Completed` or an error, `push` returns nothing.
* The outcome does not depend on how the bytes were cut.
* One decoder reads one response. Make a new one for the next.
* `finish` reports truncation once; after an error already returned by
  `push` it is `Ok`.
* `Completion::timing` counts from the moment the decoder was created, so
  create it when the response starts.

## The encoder

`Encoder` turns a `Request` into a `Body` whose exact length is known before
its first byte.

```rust
use svir::openai::chat::Encoder;
use svir::prelude::*;

fn encode(request: &Request) -> Result<(u64, bytes::Bytes), Error> {
    let body = Encoder::new().include_usage(true).encode(request)?;
    let length = body.len();

    // `into_bytes` works when no attachment is a file path.
    Ok((length, body.into_bytes()?))
}
```

| Call | Does |
|---|---|
| `Encoder::new()` | Sends what the request sets and nothing else |
| `.include_usage(bool)` | Asks for token counts when the request does not say. Off here; the client turns it on |
| `.lean(bool)` | Leaves out `reasoning_effort` and `stream_options`, for a server known to reject them. A tool choice and a response format stay |
| `.context_tokens(n)` | Fails with `ContextOverflow` when the body length plus `max_tokens` does not fit |
| `encode(&request)` | No I/O. An attachment held as a file path is `ErrorKind::Attachment`; a tool call required that the request cannot make is `Unsupported` |
| `encode_files(&request).await` | Feature `client`. Measures file attachments first, so that the length is exact; a text file with a declared `escaped_len` is not read |
| `body.len()` | The `Content-Length` to send |
| `body.into_bytes()` | The whole body, when every attachment is in memory |
| `body.into_stream()` | Feature `client`. The body in blocks; files are read as it is polled |

The body always says `"stream": true`. svir has no decoder for a response
that is not a stream.

## A transport of one's own

With `default-features = false` svir is the types and the codec. The
transport then has to do what the client does:

1. `POST {base}/v1/chat/completions` with `content-type: application/json`,
   `accept: text/event-stream`, the `Authorization` header if there is a key,
   and `Content-Length` from `body.len()`.
2. Treat any status other than success as an error before decoding. svir's
   mapping of statuses to `ErrorKind` lives in the client and is not part of
   the codec; with a transport of your own, the mapping is yours.
3. Require `content-type: text/event-stream` on a success. Anything else is
   not an answer stream.
4. Feed the body to a `Decoder` as it arrives, and call `finish` at the end.
5. Stop reading once `Event::Completed` arrives.

If the only reason is a different HTTP client, a custom `http::Backend` is
less work and keeps the status mapping, the timeouts, and the compatibility
handling; see `client.md`.
