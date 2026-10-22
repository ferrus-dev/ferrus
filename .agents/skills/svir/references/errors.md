# Errors and troubleshooting

One error type, a kind to act on, and what each symptom usually means.

## Contents

- [The error](#the-error)
- [Kinds](#kinds)
- [Handling them](#handling-them)
- [The server's own message](#the-servers-own-message)
- [Symptom to cause: at runtime](#symptom-to-cause-at-runtime)
- [Symptom to cause: at compile time](#symptom-to-cause-at-compile-time)

## The error

Every fallible call returns `svir::Error`.

| Accessor | Gives |
|---|---|
| `kind()` | An `ErrorKind`: what went wrong, in terms code can act on |
| `is_retryable()` | Whether the same request can succeed later |
| `retry_after()` | How long the server asked to wait, if it said (capped at 30 s) |
| `is_unsent()` | The request never reached the server: the connection could not be made |
| `status()` | The HTTP status of a response that was not a success; `None` for any other failure |
| `detail()` | A short description written by svir |
| `server_message()` | The server's own words, if it sent any |
| `source()` | The underlying error, through `std::error::Error` |

`Display` is the kind's description and the detail. It never contains the
API key, the request URL, headers, or the server's message.

## Kinds

| Kind | Retryable | Raised for | What to do |
|---|---|---|---|
| `Transport` | yes | The server could not be reached; HTTP 500, 502, 503 | Check that the server runs; retry with backoff |
| `Timeout` | yes | A connect, idle, or layer deadline passed; HTTP 408, 504 | Retry, or raise the deadline for a slow local model |
| `RateLimited` | yes | HTTP 429 | Wait `retry_after()` if present, then retry |
| `TruncatedStream` | yes | The stream ended before the answer was complete | Send the request again if the partial answer can be discarded |
| `Authentication` | no | HTTP 401, 403 | Fix the key |
| `ContextOverflow` | no | The request does not fit in the model's context | Shorten the conversation, or drop attachments |
| `ContentFilter` | no | The server's content filter blocked the prompt | Change the prompt: the same one is blocked again, and billed again |
| `Server` | no | The server reported a failure inside the stream | Read `server_message()`; the server's logs say more |
| `Protocol` | no | A malformed or inconsistent response | Report it, with the raw stream |
| `Unsupported` | no | Something svir cannot represent; a tool call required that the request cannot make; an unexpected status; a response that is not an event stream | See the table below |
| `ResponseLimit` | no | The response passed a configured limit | Raise `Limits` on purpose |
| `Attachment` | no | A file could not be read, is not what it claims, or changed | Fix the file or its media type |
| `Config` | no | The URL, the key source, or a header is not acceptable | Fix the builder call |

`ErrorKind` is `#[non_exhaustive]`: a `match` needs a wildcard arm. A blocked
prompt is recognized by the error's code alone, `content_filter`, as Azure
OpenAI sends it with a 400. An answer the filter stops is not an error but
`FinishReason::ContentFilter`, and a refusal to answer is not an error either
but `FinishReason::Refusal`; see `streaming.md`.

## Handling them

```rust
use svir::prelude::*;

enum Next {
    Answer(Completion),
    RetryAfter(std::time::Duration),
    Shorten,
    GiveUp(String),
}

async fn ask(client: &Client, request: &Request) -> Next {
    let error = match client.complete(request).await {
        Ok(done) => return Next::Answer(done),
        Err(error) => error,
    };

    match error.kind() {
        ErrorKind::ContextOverflow => Next::Shorten,
        ErrorKind::RateLimited => {
            Next::RetryAfter(error.retry_after().unwrap_or(std::time::Duration::from_secs(5)))
        }
        _ if error.is_retryable() => Next::RetryAfter(std::time::Duration::from_secs(1)),
        // `Display` is safe to log and to show.
        _ => Next::GiveUp(error.to_string()),
    }
}
```

* Match on the kind, never on the text of `Display` or `detail()`: the text
  can change between releases, the kinds are part of the API.
* `complete` either returns the whole answer or an error; a retry there is
  clean. With `stream`, a failure after the first event means the caller has
  shown part of an answer: retrying is a product decision, not a default.
* For retries before the response starts, prefer the `Retry` layer to a
  hand-written loop; see `client.md`.
* A proxy that wants to answer with the upstream's status reads
  `error.status()`. It is `None` for a timeout or a failure inside a stream,
  where the proxy has to pick a status of its own (502, 504).

## The server's own message

`error.server_message()` is what the server said, cut to 4 KiB: `error.message`
from a JSON body (or `error` itself when it is a string); `detail` when a JSON
body has no `error`, as FastAPI servers such as mlx-vlm report errors (for a
list of validation errors, the first one's `msg`; svir 0.1.6); a plain-text
body; or a message inside the stream. A rejected request (400, 413, 422) or
an error inside the stream is `ContextOverflow` when its code names an
overflow, or its message speaks of the context's length, size, window, or
budget, or of compacted context.

It is kept out of `Display` and `Debug` on purpose. A server's message can
quote the prompt, a file name, or an internal address, and those do not
belong in logs. Show it to the person who made the request; log
`error.to_string()`.

```rust
use svir::prelude::*;

fn explain(error: &Error) -> String {
    match error.server_message() {
        Some(said) => format!("{error} (the server said: {said})"),
        None => error.to_string(),
    }
}
```

## Symptom to cause: at runtime

| Symptom | Usual cause |
|---|---|
| `Config` from `build()`: "plain HTTP to a host that is not loopback" | The URL is `http://` to another machine. Use `https://`, or `.allow_http()` for a trusted network |
| `Config`: "an https URL needs the `tls` or the `tls-aws-lc` feature" | `default-features = false` without either |
| A panic elsewhere: "no process-level CryptoProvider available" | Two rustls providers are compiled in: svir's `tls` brought ring next to another crate's aws-lc-rs. Take svir with `tls-aws-lc` instead of `tls`; see `client.md` |
| `Config`: "the API key variable ... is not set" | `api_key_env` names a variable the process does not have. svir reads no `.env` file |
| `Config`: "http() must be called before layers are added" | Move `.http(backend)` above `.layer(..)` and `.wrap(..)` |
| `Config`: "a header name is not valid" | A name given to `.header(..)` has a space or another character a header name cannot have |
| `Config`: "the header ... is set by svir, not by the caller" | `.header(..)` named `authorization`, `content-type`, `content-length`, `accept`, `host`, `transfer-encoding`, or `connection`. The Bearer key goes through `api_key` |
| `Config`: "the value of header ... is not valid" | The value has a line break, another control character, or text that is not ASCII. Often a value read from a file with its trailing newline: trim it |
| `Transport`, `is_unsent()` true | Nothing listens at the URL: the server is not running, or the port is wrong |
| `Unsupported`, detail "HTTP 404" | The base URL points somewhere that is not the API (a path too many, a web UI), or the server does not know the model. `server_message()` usually says which |
| `Unsupported`, detail "HTTP 404", on a message with an image | mlx-lm takes text only. Serve the model with mlx-vlm |
| A request to mlx-lm is slow to start, or fails to load a model | mlx-lm loads the model a request names when it is not the loaded one. Name the model as the server lists it: the path `--model` gave |
| `Unsupported`: "the response is not an event stream" | A success status with HTML or JSON: a gateway page, or an endpoint that ignores `stream` |
| `Unsupported`: "a delta field outside the protocol", or a similar "outside the protocol" | Strict decoding met a field svir does not know. Read what the server sends (`cargo run --example relay` in the svir repository); `.lenient()` skips such fields |
| `Unsupported`: "a finish reason outside the protocol" | The server stopped for a reason svir does not know (a filtered answer is `FinishReason::ContentFilter`, not this). The answer may not be what it looks like; lenient mode does not change this |
| `ContentFilter`, detail "HTTP 400" | Azure OpenAI's content filter blocked the prompt. Nothing was generated, but the evaluation was billed: change the prompt rather than send it again |
| `Unsupported`: "a tool call is required, and the request offers no tools" | `ToolChoice::Required` on a request with no tools. Add them with `.tool(..)` or `.tools(&toolbox)` |
| `Unsupported`: "a call is required of a tool the request does not offer" | `ToolChoice::tool(name)` names a tool that is not in the request: a typo, or a tool not added |
| `Unsupported`, detail "HTTP 400", on a request with a tool choice or a response format | The server does not take it, and svir never sends the request without it. LM Studio rejects a named tool (use `Required` with that one tool) and `ResponseFormat::Json` (use a `Schema`). OpenAI rejects `Json` unless the messages say "JSON"; OpenAI and Azure OpenAI reject a strict schema whose objects do not list every property as required with `additionalProperties: false` |
| `Protocol`: "an answer that is both content and a refusal", or "a refusal with tool calls" | The server contradicts itself about what the answer is. Report it, with the raw stream; lenient mode does not change this |
| `Server`, and `server_message()` says the output "does not match the expected" format | llama.cpp could not parse what the model wrote, often a tool call it did not expect, such as one under `ToolChoice::None` |
| `Unsupported`, detail "HTTP 400" or "HTTP 422" | The server rejected the request. Read `server_message()`. Common: a model without tool or image support, or a message the server's chat template cannot take |
| `Unsupported`, detail "HTTP 400", on a request with tools: "\"auto\" tool choice requires --enable-auto-tool-choice and --tool-call-parser to be set" | vLLM started without a tool-call parser. Start it with `--enable-auto-tool-choice --tool-call-parser` and the parser for the model |
| `Transport`, detail "HTTP 500", on a message with an image: "image input is not supported" | llama.cpp without the model's image projector: start it with `--mmproj`. The kind is retryable, but sending the request again fails the same way |
| `Timeout`: "the server sent no response" or "the response stalled" | Silence for longer than the idle timeout (5 min by default). A local model on a long prompt can take longer; raise `idle_timeout` |
| `Timeout` from a `Timeout::first_token` layer on a local model | The deadline is shorter than the model needs to read the prompt |
| `TruncatedStream` | The connection was cut: a gateway's own timeout, a server that crashed or was unloaded |
| `ContextOverflow` before anything was sent | `context_tokens` is set and the body's bytes plus `max_tokens` exceed it. Bytes overestimate images by far |
| `ContextOverflow` from the server | The conversation outgrew the context. Some servers say it inside a `200` stream; svir reports both the same way |
| `ContextOverflow` from mlx-vlm on every request | The server's `--max-tokens` counts toward its context limit (`MAX_KV_SIZE`) when a request sets no `max_tokens`. Set `max_tokens`, or start the server with a lower `--max-tokens` |
| `Attachment`: "an image has no media type" | An extension other than `png`, `jpg`, `jpeg`, `gif`, `webp`. Add `.media_type(..)` |
| `Attachment`: "a text file is not UTF-8" | `TextFile` is for text. Send an image as `Image`; convert anything else first |
| "stream did not contain valid UTF-8", an `std::io::Error` | Not svir: the program's own input. In a terminal without `iutf8` (`stty -a`), Backspace over a non-ASCII letter erases one byte and leaves half of it. Read bytes and check them, or `stty iutf8`. A server that sends bytes that are not text is `Protocol`: "an event is not valid UTF-8" |
| `Attachment`: "a text file is not the escaped length it declares" | `TextFile::escaped_len` declared a length the file cannot have, or one text in memory does not have. Measure the whole file with `svir::body::escaped_len` |
| `Attachment`: "an attachment changed after the body was built, or is not the length it declares" | The file was written to between measuring and sending, or its declared `escaped_len` is wrong |
| `ResponseLimit` | A response over 64 MiB, an event over 256 KiB, or more than 64 tool calls. Raise `Limits` if that is expected |
| `done.usage` is `None` | The server does not report usage, or it rejected the field and the client stopped asking |
| `Effort` stops changing anything, and `done.usage` is `None`, on vLLM | The chat template rejected the effort (Qwen3.8's has no `High`), and the client stopped sending the optional fields. Use an effort the template names; a new client sends them again |
| `done.text` starts with blank lines | The server left them after the reasoning. Trim for display |
| `done.text` holds the reasoning, then `</think>` and the answer | The server has no reasoning parser, and the model's chat template opens `<think>` in the prompt. Turn the parser on: `--reasoning-parser` for vLLM |
| `done.text` is empty and `finish` is `Length` | Reasoning used the whole output budget. Raise `max_tokens`, or lower the effort |
| `done.text` is empty and `finish` is `ToolCalls` | Not a failure: the model is waiting for tool results |
| `finish` is `ContentFilter`, though the whole answer streamed | Azure's asynchronous content filter vets the answer after streaming it, and blocked part of it. Withdraw the text the user was shown |
| `finish` is `Refusal` | The model would not answer, and `done.text` is its refusal: show it. Asked for a format, OpenAI's models refuse rather than give an answer they will not put in it |
| `parse` fails: "the answer is not whole: it finished with Length" | The output limit cut the answer off: raise `max_tokens`. With another finish named, the text is not the answer asked for either |
| `parse` fails, and `done.text` is empty | LM Studio, with reasoning on, sent the JSON as reasoning. Add `.reasoning(Effort::Off)` |
| `parse` fails on an answer that looks right | The type and the schema disagree, or no response format was set and the model wrapped the JSON in prose or a code fence. Derive the schema from the type with `Schema::of` |
| The model never calls a tool | The model has no tool support, or the description does not say when to use the tool |
| The model answers without the call `tool_choice` required | The server took the choice and did not keep it: LM Studio with `Required`, llama.cpp with a named tool, mlx-lm with any choice. svir does not check: test `done.calls` |
| A loop with a required tool call never ends with an answer | The choice stays on the request every turn. Set `.tool_choice(ToolChoice::Auto)` with the results |
| The next request after a tool call is rejected | The assistant turn or a result is missing: `request.assistant(done).tool_results(results)` |
| The same answer, whatever model is named | Some local servers answer with the loaded model when they do not know the ID. List the models |
| A request for one model is slow once, then fast | A local server loaded the model on the first request |

## Symptom to cause: at compile time

| The compiler says | Cause |
|---|---|
| No variant `System` on `Role` | The system prompt is `Request::system(..)` |
| Cannot create a non-exhaustive struct with a struct expression | Use the constructor: `Request::new`, `Tool::new`, `ToolResult::new`, `ToolResult::error`, `Schema::new`, `Usage::new` |
| Non-exhaustive patterns on `Event`, `ErrorKind`, `FinishReason`, `Part`, `ToolChoice`, `ResponseFormat` | Add a wildcard arm |
| Use of moved value: `request` | The builder takes `self`. Write `request = request.user(..)`, and pass `&request` to `complete` and `stream` |
| No method `add` on `Tools` | Enable svir's `schemars` feature, or use `add_tool` with a schema |
| No function `of` on `Schema` | Enable svir's `schemars` feature, or write the schema with `Schema::new` |
| `T: DeserializeOwned` is not satisfied, from `parse` | The type does not derive `Deserialize`, or borrows: `parse` returns an owned value, so a field is a `String`, not a `&str` |
| No method `tool_choice` or `response_format` on `Request`, no `Schema` or `ToolChoice` in the prelude | The lock file is on svir 0.1.4 or earlier: `cargo update -p svir` |
| The trait `JsonSchema` is not implemented | The caller's `schemars` is not version 1, so its derive is a different trait |
| Cannot find `Trace` in `svir::layer` | Enable the `tracing` feature |
| Cannot find `Client` in `svir` | `default-features = false` turned the `client` feature off |
| `Layer`, `Next`, `Retry`, or `Decoder` not found | They are not in the prelude: `svir::layer::..`, `svir::openai::chat::..` |
| Mismatched types: expected `Client<..>`, found `Client` | A client with a custom backend is `Client<Backend>`; name the type, or make the function generic over `B: svir::http::Backend` |
| A future is not `Send`, in a `Layer` or `Toolbox` impl | Something that is not `Send` (an `Rc`, a `std::sync::MutexGuard`) is held across an `.await` |
| Expected `ToolResult`, found `Result<..>` in `Toolbox::call` | `call` returns a result for the model, never an error: turn the failure into `ToolResult::error(&call.id, message)` |
