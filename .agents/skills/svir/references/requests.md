# Requests

How to say what the model should answer: the system prompt, messages and
their parts, attachments, reasoning effort, an answer as JSON, and the
conversation so far.

## Contents

- [The request](#the-request)
- [Messages and parts](#messages-and-parts)
- [Attachments](#attachments)
- [Reasoning effort](#reasoning-effort)
- [Structured output](#structured-output)
- [A conversation](#a-conversation)
- [Storing and restoring](#storing-and-restoring)
- [What a request cannot say](#what-a-request-cannot-say)

## The request

`Request::new(model)` and a chain of methods. Every method takes `self` and
returns it.

| Method | Effect |
|---|---|
| `system(text)` | The system prompt. One per request; a second call replaces it |
| `user(text)` | Adds a user message with this text |
| `message(message)` | Adds a message built with `Message` |
| `assistant(completion)` | Adds the model's answer, with its text, tool calls, and reasoning |
| `tool_result(call_id, content)` | Adds the result of one tool call |
| `tool_results(results)` | Adds results, one message each, from `Vec<ToolResult>` |
| `tool(tool)` / `tools(&toolbox)` | Describes tools the model may call; see `tools.md` |
| `tool_choice(choice)` | Whether the model may or must call a tool; see `tools.md` |
| `response_format(format)` | The answer as JSON, or as JSON that matches a schema; see [Structured output](#structured-output) |
| `reasoning(effort)` | How much the model should reason |
| `max_tokens(n)` | The most tokens to generate. On most servers reasoning counts toward it |
| `temperature(t)` | Sampling temperature |
| `include_usage(bool)` | Overrides the client's default (on) of asking for token counts |
| `send_reasoning(bool)` | Sends reasoning from earlier answers back (off by default) |

Nothing is sent unless it is set: a request without `max_tokens` or
`temperature` leaves them to the server, and a tool choice or a response
format left at its default is not sent either.

```rust
use svir::prelude::*;

fn request(question: &str) -> Request {
    Request::new("qwen3-27b")
        .system("Answer in one paragraph.")
        .reasoning(Effort::Low)
        .max_tokens(2048)
        .temperature(0.2)
        .user(question)
}
```

The fields are public for reading (`request.model`, `request.messages`), which
is what a layer uses. `Request` is `Clone`.

## Messages and parts

A `Message` is a role and its parts in order.

```rust
use svir::prelude::*;

fn messages() -> Request {
    let question = Message::user("What changed between these two?")
        .with(Image::path("before.png"))
        .with(Image::path("after.png"))
        .with(TextFile::path("diff.patch"));

    Request::new("qwen3-27b").message(question)
}
```

| Constructor | Use |
|---|---|
| `Message::user(text)` | A user turn. Empty text adds no part, so `Message::user("")` followed by images carries only the images |
| `Message::assistant(text)` | A model turn rebuilt from stored text; for a live answer use `request.assistant(done)` |
| `Message::tool_result(result)` | One tool result |
| `.with(part)` | Adds a part: a `&str`, a `String`, an `Image`, a `TextFile`, a `ToolCall`, a `ToolResult`, a `Reasoning` |

Which parts a role may carry is checked when the request is sent, and a
violation is `ErrorKind::Unsupported`. Nothing is dropped silently.

| Role | Parts |
|---|---|
| `User` | Text, images, text files |
| `Assistant` | Text, reasoning, tool calls |
| `Tool` | Tool results only |

## Attachments

```rust
use svir::prelude::*;

fn attachments(photo: Vec<u8>) -> Message {
    Message::user("Describe what you see.")
        // A file: nothing is read until the request is sent.
        .with(Image::path("chart.png"))
        // An extension svir does not know needs its media type.
        .with(Image::path("scan.tif").media_type("image/tiff"))
        // Bytes already in memory always name theirs.
        .with(Image::bytes(photo, "image/jpeg"))
        // A text file, by path or from memory.
        .with(TextFile::path("notes.md"))
        .with(TextFile::text("query.sql", "select 1;"))
}
```

* **Images** travel as base64 data URLs. The media type comes from the
  extension for `png`, `jpg`, `jpeg`, `gif`, and `webp`; anything else needs
  `.media_type(..)`, or sending fails with `ErrorKind::Attachment`.
* **Text files** travel as text inside the message, wrapped with their name.
  They must be UTF-8. `.name(..)` changes the name the model is told.
* **Files are read while the request is sent**, a block at a time, so a large
  image is never in memory whole. A file that is missing, unreadable, or
  changed since the body was measured fails the call with
  `ErrorKind::Attachment`, not the construction of the message.
* Paths are relative to the process's working directory.
* The model has to see images. A text-only model rejects the request or
  ignores the image; svir cannot tell which model can.

A text file is read twice by default: once to measure its length escaped
into JSON, which depends on its contents, and once to send it. An
application that stores files measures once and declares it (svir 0.1.4):

```rust
use svir::prelude::*;

/// When the file arrives: its length once escaped into a JSON string.
fn measure(text: &[u8]) -> u64 {
    svir::body::escaped_len(text)
}

/// When it is sent: the stored length, so the file is read only once.
fn attach(path: &str, escaped: u64) -> TextFile {
    TextFile::path(path).escaped_len(escaped)
}
```

* The lengths of a file's pieces add up to the file's, even a piece that
  ends inside a character, so an upload can be measured chunk by chunk.
* `escaped_len` does not check UTF-8; the body stream does. A wrong length
  or bytes that are not UTF-8 fail with `ErrorKind::Attachment`, when the
  body is built or while it streams, never as a body that disagrees with its
  `Content-Length`.
* `svir::body::escaped_len` needs no feature. Images need no declaration:
  their base64 length follows from their size.

## Reasoning effort

`Effort` is `Off`, `Low`, `Medium`, `High`, or `XHigh`. Unset sends nothing
and leaves the choice to the server. `Off` asks for no reasoning; whether
the model obeys is the model's business.

A server that rejects the field is handled by the client: the request is sent
again without it, once, and the server is remembered. No code is needed for
that. vLLM hands the effort to the model's chat template, which may name only
some values (Qwen3.8's rejects `High` with a 400); the client then stops
sending the optional fields to that server, usage included. Use an effort the
template names.

Reasoning comes back as `Event::Reasoning` and in `Completion::reasoning`; see
`streaming.md`.

## Structured output

An answer as JSON, or as JSON that matches a schema, read back into a type
(svir 0.1.5). The answer arrives as text like any other.

<!-- snippet: features="schemars" -->
```rust
use schemars::JsonSchema;
use serde::Deserialize;
use svir::prelude::*;

/// A river, as an atlas lists it.
#[derive(Deserialize, JsonSchema)]
struct River {
    /// The river's name in English.
    name: String,
    /// Its length in kilometres.
    length_km: u32,
    /// The lake or sea it flows into.
    mouth: String,
}

async fn river(client: &Client) -> Result<River, Box<dyn std::error::Error>> {
    let request = Request::new("qwen3-27b")
        .response_format(Schema::of::<River>())
        .user("Describe the river that joins Lake Onega to Lake Ladoga.");

    let answer = client.complete(&request).await?;

    Ok(answer.parse()?)
}
```

| `response_format(..)` | Sent as | The answer |
|---|---|---|
| `ResponseFormat::Text` | Nothing | Text of any shape. The default |
| `ResponseFormat::Json` | `{"type": "json_object"}` | A JSON object of any shape |
| `Schema::new(name, schema)`, `Schema::of::<T>()` | `{"type": "json_schema", ...}` | JSON that matches the schema |

A schema written by hand:

```rust
use serde_json::json;
use svir::prelude::*;

fn weather(city: &str) -> Request {
    let schema = json!({
        "type": "object",
        "properties": {"city": {"type": "string"}, "celsius": {"type": "number"}},
        "required": ["city", "celsius"],
        "additionalProperties": false
    });

    Request::new("qwen3-27b")
        .response_format(Schema::new("weather", schema).strict(true))
        .user(format!("The weather in {city}, please."))
}
```

* **`Schema::of::<T>()`** needs svir's `schemars` feature and `schemars = "1"`
  in the caller's crate. It names the schema after the type and takes the doc
  comments as descriptions. A schema's name is ASCII letters, digits, `_`,
  and `-`, at most 64 characters.
* **`.strict(true)`** is off by default and sent only when on. OpenAI and
  Azure OpenAI guarantee a matching answer only with it, and then accept a
  schema only when every object lists all of its properties under `required`
  and sets `additionalProperties: false`. A derived schema fits when every
  struct has `#[serde(deny_unknown_fields)]` and no `Option` fields (schemars
  leaves an `Option` out of `required`). Local servers constrain sampling to
  the schema, strict or not, except mlx-lm, which reads no response format.
  svir never rewrites a schema to fit.
* **`ResponseFormat::Json`**: OpenAI rejects it unless the messages contain
  the word "JSON"; LM Studio rejects it with a 400 and wants a schema.
* **`done.parse::<T>()`** reads the text with serde. Only an answer that
  finished with `Stop` parses; any other finish is an error before the text
  is read ("the answer is not whole: it finished with Length"), because an
  answer cut off by the output limit can still be valid JSON. The error is a
  `serde_json::Error`. Blank lines before the JSON parse fine.
* **Nothing validates the answer against the schema.** The type is the
  check; a rule the type does not express is checked after parsing. To read
  text that did not finish with `Stop` anyway, use `serde_json::from_str`.
* **A model may refuse**, OpenAI's above all when asked for a format: the
  finish is `FinishReason::Refusal` and the text is the refusal. `parse`
  fails on it. See `streaming.md`.
* **`parse` reads the text, never the reasoning.** LM Studio with reasoning
  on (effort unset, `Low`, `Medium`) holds the reasoning to the schema and
  sends the whole JSON as reasoning, with no text. Ask it for
  `.reasoning(Effort::Off)` with the format.
* **A response format is never dropped.** The compatibility retry keeps it,
  and a server that does not take it fails the request with `Unsupported`,
  the status, and its message. Ask for a format the server takes; do not
  fall back to text silently.
* Streamed, the JSON arrives as `Event::Text` pieces and is not valid until
  whole. Parse the completion, not the deltas.

## A conversation

The client keeps nothing. The request is the conversation, and the caller
owns it.

```rust
use svir::prelude::*;

async fn converse(client: &Client, lines: Vec<String>) -> Result<(), Error> {
    let mut request = Request::new("qwen3-27b").system("You are a concise assistant.");

    for line in lines {
        request = request.user(line);

        let done = client.complete(&request).await?;
        println!("{}", done.text.trim());

        // The answer goes back as it is: text, tool calls, reasoning.
        request = request.assistant(done);
    }

    Ok(())
}
```

* `request.assistant(done)` takes the `Completion` by value. Clone it first
  if it is also stored elsewhere.
* Reasoning in earlier answers is kept in the history but not sent back
  unless `.send_reasoning(true)`. When sent, it goes under the field it
  arrived in (the same text in both fields goes as `reasoning_content`);
  reasoning split out of inline `<think>` tags is never sent.
* When a call fails, the user message already added is still in the request.
  Decide whether to keep it for the retry or rebuild the request.
* The history grows with every turn. Trimming or summarizing it is the
  caller's policy; a request that no longer fits fails with
  `ErrorKind::ContextOverflow`.

## Storing and restoring

`Request`, `Message`, `Part`, `Completion`, and the rest of the public data
types implement `Serialize` and `Deserialize`, and their serde form is a
stable part of the API.

```rust
use svir::prelude::*;

fn roundtrip(request: &Request) -> Result<Request, serde_json::Error> {
    let stored = serde_json::to_string(request)?;

    serde_json::from_str(&stored)
}
```

An attachment given as a path is stored as the path, and one given as bytes
as base64. A stored path has to exist again when the request is sent.

## What a request cannot say

svir 0.1 has no stop sequences, and asks for one choice. The response is
always a stream; `complete` collects it. Do not reach for a field that is not
in the table above: say that svir 0.1 does not carry it. `tool_choice` and
`response_format` need svir 0.1.5; on an earlier lock file, say so rather
than writing the JSON by hand.
