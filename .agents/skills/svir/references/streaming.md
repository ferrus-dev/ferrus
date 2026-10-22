# Reading an answer

What comes back from a call: events while the answer streams, and the
completion at the end.

## Contents

- [Whole or streamed](#whole-or-streamed)
- [Events](#events)
- [The completion](#the-completion)
- [Streaming and keeping the answer](#streaming-and-keeping-the-answer)
- [Reasoning](#reasoning)
- [Usage and speed](#usage-and-speed)
- [Cancelling and deadlines](#cancelling-and-deadlines)
- [Strict and lenient](#strict-and-lenient)
- [Limits](#limits)
- [When the stream fails](#when-the-stream-fails)

## Whole or streamed

| Call | Returns | Use when |
|---|---|---|
| `client.complete(&request).await?` | `Completion` | Only the finished answer matters |
| `client.stream(&request).await?` | `EventStream` | Text is shown as it arrives |
| `stream.completion().await?` | `Completion` | A stream was opened, and the rest of it is not shown |

The request is streamed on the wire either way; `complete` is `stream`
followed by `completion`. `stream` resolves when the response starts, which
for a local model can be long after the call: the model reads the whole
prompt first.

## Events

`EventStream::next()` yields `Option<Result<Event, Error>>`.

| Event | Carries | Meaning |
|---|---|---|
| `Event::Text(String)` | A piece of the answer | Show it |
| `Event::Reasoning(Reasoning)` | `source`, `text` | A piece of the model's reasoning; show it apart from the answer |
| `Event::ToolCallDelta(ToolCallDelta)` | `index`, `id`, `name`, `arguments` | A piece of a tool call, for display only |
| `Event::Completed(Completion)` | The whole answer | Always the last item |

After `Completed`, or after an `Err`, `next()` returns `None`. `Event` is
`#[non_exhaustive]`: end every `match` with `_ => {}`.

## The completion

| Field | Type | Holds |
|---|---|---|
| `finish` | `FinishReason` | `Stop`, `ToolCalls`, `Length`, `ContentFilter`, or `Refusal` |
| `text` | `String` | The answer, exactly as sent |
| `reasoning` | `Vec<Reasoning>` | Reasoning, one entry per source |
| `calls` | `Vec<ToolCall>` | Complete tool calls, in order |
| `usage` | `Option<Usage>` | Token counts, when the server reported them |
| `timing` | `Option<Timing>` | When the first and last visible tokens arrived |

Act on `finish`:

```rust
use svir::prelude::*;

fn describe(done: &Completion) -> &'static str {
    match done.finish {
        FinishReason::Stop => "the answer is complete",
        // Run the tools in `done.calls` and ask again; see tools.md.
        FinishReason::ToolCalls => "the model is waiting for tool results",
        // The output limit cut it off. With a reasoning model the text can be
        // empty: the reasoning used the budget. Raise `max_tokens`.
        FinishReason::Length => "the answer was cut off",
        // The server's content filter stopped it, or flagged it after it was
        // streamed. `done.text` holds what was sent, which may be what was
        // flagged: withdraw what the user was shown.
        FinishReason::ContentFilter => "the answer was filtered",
        // The model would not answer, and `done.text` says why: show it as
        // the answer. It does not have a format the request asked for.
        FinishReason::Refusal => "the model refused",
        _ => "a finish reason this code does not know yet",
    }
}
```

A `ContentFilter` finish can come after the whole answer: Azure OpenAI's
asynchronous content filter streams the answer before vetting it, and reports
a block afterwards, even after the model's own `stop`. Text before a block may
hold what was blocked in Azure's default mode too. Code that shows the deltas
as they arrive takes the text down on this finish; leaving it up with a note
keeps showing what the filter blocked.

A `Refusal` finish (svir 0.1.5) means the model would not answer, and the
text is its refusal. Chat Completions sends it in `refusal` in place of
`content`, with a `stop` finish on the wire; svir streams it as `Event::Text`
and makes the finish `Refusal`, so a chat shows it with no code of its own
and code that asked for JSON learns the text is not JSON. A refusal the
content filter stopped is `ContentFilter`. Do not look for a `refusal` field
on the completion: there is none.

`done.parse::<T>()` reads an answer asked for as JSON into a type, and only
when `finish` is `Stop`; see `requests.md`.

## Streaming and keeping the answer

Show the deltas, keep the completion. Do not rebuild the answer from the
deltas: the completion already has it, with the tool calls and the usage.

```rust
use std::io::Write;

use svir::prelude::*;

async fn show(client: &Client, request: &Request) -> Result<Completion, Error> {
    let mut stream = client.stream(request).await?;

    while let Some(event) = stream.next().await {
        match event? {
            Event::Text(piece) => {
                print!("{piece}");
                let _ = std::io::stdout().flush();
            }
            Event::Reasoning(piece) => eprint!("{}", piece.text),
            Event::Completed(done) => return Ok(done),
            _ => {}
        }
    }

    // Unreachable in practice: a stream ends with `Completed` or with an error.
    Err(Error::new(ErrorKind::TruncatedStream))
}
```

`EventStream` is `Send + Unpin + 'static` and owns everything it needs, so it
can be moved into a task or stored in a struct. It also implements
`futures_core::Stream`, for code that wants combinators.

## Reasoning

Servers carry reasoning in three ways, and svir reads all of them into
`Reasoning { source, text }`:

| `ReasoningSource` | Where it was |
|---|---|
| `ReasoningContent` | The `reasoning_content` field, or the same text in both `reasoning_content` and `reasoning` (mlx-vlm sends both; svir 0.1.6 reads it once) |
| `Reasoning` | The `reasoning` field |
| `Think` | `<think>...</think>` inside the answer text |

Inline `<think>` tags are split out of the text by default, so reasoning is
not shown as the answer even when a server has no reasoning parser.
`.think(Think::Keep)` on the client builder leaves the tags in the text.

A `</think>` with no `<think>` before it stays in the text. A chat template
can open the tag in the prompt; a server without a reasoning parser then
sends the reasoning as the answer, ending in a lone `</think>`, and svir has
streamed it as text before the marker arrives. The fix is on the server:
vLLM's `--reasoning-parser`, llama.cpp's default `--reasoning-format`.

`Completion::reasoning` has one entry per source, in order of first
appearance, with the pieces joined.

## Usage and speed

```rust
use svir::prelude::*;

fn report(done: &Completion) {
    if let Some(usage) = done.usage {
        println!("{} tokens in, {} out", usage.input, usage.output);

        if let Some(reasoning) = usage.reasoning {
            println!("{reasoning} of them reasoning");
        }
    }
    if let Some(rate) = done.tokens_per_second() {
        println!("{rate:.1} tokens per second");
    }
}
```

* Usage is asked for by default. A server that does not report it leaves
  `usage` as `None`; the answer is still complete. Never `unwrap` it.
* `usage.total` and `usage.reasoning` are present only when the server sent
  them.
* `tokens_per_second()` runs from the first visible token to the last, so the
  wait before the first token does not drag it down. It is `None` for a
  single token or a window under 50 ms: there is no honest rate then.
* `timing.first_token` is measured from the start of the response, not from
  the call. Time the call itself for time-to-first-token as a user feels it.

## Cancelling and deadlines

Dropping the stream cancels the request and closes the connection. A
cancellation token, a disconnect of the caller's own client, a `select!`
that moves on: all of them cancel by dropping.

```rust
use std::time::Duration;

use svir::prelude::*;

async fn bounded(client: &Client, request: &Request) -> Result<Completion, Error> {
    let answer = tokio::time::timeout(Duration::from_secs(60), client.complete(request));

    match answer.await {
        Ok(result) => result,
        // The future was dropped, and the request with it.
        Err(_) => Err(Error::new(ErrorKind::Timeout).with_detail("no answer in 60 seconds")),
    }
}
```

For deadlines on every call of a client (first token, silence, the whole
answer), use the `Timeout` layer instead; see `client.md`. The client has
one built in: a server that sends nothing for 5 minutes fails the call.

## Strict and lenient

Decoding is strict by default: anything svir does not know, or that does not
add up, is an error. `.lenient()` on the client builder relaxes what cannot
make the answer wrong.

| The server sends | Strict | Lenient |
|---|---|---|
| A field svir does not know in a delta or a tool call | `Unsupported` | Skipped |
| An event type or SSE field outside the protocol | `Unsupported` | Skipped |
| Data that is not JSON, or invalid UTF-8 | `Protocol` | Skipped, or replaced lossily |
| A changing `id` or `model`, usage reported twice or incomplete | `Protocol` | Tolerated |
| More than one choice | `Unsupported` | The first is read |
| A finish reason svir does not know (`content_filter` is known), or content after the finish reason | `Unsupported` | `Unsupported` |

Both modes enforce the limits, fail a stream that ends early, and refuse
tool calls that are inconsistent (missing or duplicate IDs, a gap in the
indices, a `tool_calls` finish with no calls, calls with a `content_filter`
finish). Calls with a `stop` finish, as vLLM sends a named tool's call, are
an answer of calls: the finish is `ToolCalls` (svir 0.1.6). A refusal is read in
both modes; an answer that is both content and a refusal, or a refusal with
tool calls, is `Protocol` in both.

Azure OpenAI's prompt report (no choices, `prompt_filter_results`) and the
annotations of its asynchronous content filter (`content_filter_offsets`, no
delta) carry nothing of the answer and are skipped in both modes. An
annotation that blocks, by a `content_filter` finish or a verdict marked
`filtered: true`, makes the finish `ContentFilter`, even after the model's
`stop`.

Choose strict for a server under your control, and to find out what a new
server actually sends. Choose lenient for a server with extensions you do
not need. Lenient is not a fix for an error you have not read.

## Limits

Every response is bounded, in both modes. Reaching a bound is
`ErrorKind::ResponseLimit`.

| Limit | Default |
|---|---|
| Bytes of the whole response, as they arrive | 64 MiB |
| Bytes of one server-sent event | 256 KiB |
| Tool calls in one answer | 64 |

```rust
use svir::Limits;
use svir::prelude::*;

fn tuned() -> Result<Client, Error> {
    let limits = Limits::default()
        .event_bytes(1024 * 1024)
        .tool_calls(16);

    Client::openai("http://127.0.0.1:1234").limits(limits).build()
}
```

The wire limit counts the event stream, which spends a few hundred bytes on
every token: 64 MiB is about a quarter of a million tokens. The decoder keeps
the answer, not the wire bytes, so the limit guards against a server that
never ends rather than against memory use.

## When the stream fails

An error is the last item of the stream. What arrived before it is a partial
answer: fine to have shown, wrong to treat as complete, and its tool calls
must not be run.

| Kind | What happened |
|---|---|
| `TruncatedStream` | The connection closed before the answer finished |
| `Timeout` | The server went silent, or a deadline passed |
| `Server` | The server reported a failure inside the stream |
| `ContextOverflow` | The server said, inside the stream, that the request does not fit |
| `Protocol`, `Unsupported` | The stream is malformed, or uses something svir does not read |
| `ResponseLimit` | A limit above was reached |

Nothing of this is retried by svir, and the `Retry` layer leaves a started
answer alone: the user has already seen part of it. Sending the request again
is the application's call. See `errors.md`.
