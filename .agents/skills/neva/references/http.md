# Transports, security and deployment

## Choosing a transport

| Transport | When | Server | Client |
|---|---|---|---|
| `stdio` | The client spawns the server as a child process — desktop assistants, CLI tools | `opt.with_stdio()` | `opt.with_stdio("cmd", ["args"])` |
| Streamable HTTP | A network service, several clients, containers | `opt.with_http(...)` / `opt.with_default_http()` | `opt.with_http(...)` / `opt.with_default_http()` |

Nothing else in your code changes between them.

## The HTTP transport is stateless

Under MCP 2026-07-28 it is request/response only:

* no `Mcp-Session-Id`, no session `DELETE`;
* no standalone SSE `GET` stream — server pushes ride a client-opened
  `subscriptions/listen` request;
* every request carries `MCP-Protocol-Version` plus mandatory `_meta` keys
  for the protocol version and the client's capabilities;
* routing headers (`Mcp-Method`, `Mcp-Name`, `Mcp-Param-{name}`) must agree
  with the body or the request is rejected with `HeaderMismatch`
  (`-32020`) and HTTP `400`.

A `POST` gets a `text/event-stream` reply in exactly three cases: its
`_meta` carries `io.modelcontextprotocol/logLevel`, its `_meta` carries a
`progressToken`, or it is a `subscriptions/listen` request. Everything else
gets a single JSON object.

## Server setup

```rust
use neva::prelude::*;

#[tokio::main]
async fn main() {
    App::new()
        .with_options(|opt| opt
            .with_http(|http| http
                .bind("127.0.0.1:3000")
                .with_endpoint("/mcp")))
        .run()
        .await;
}
```

`with_default_http()` is `127.0.0.1:3000` + `/mcp`.

### TLS

```rust
use neva::prelude::*;

#[tokio::main]
async fn main() {
    let http = HttpServer::new("localhost:7878")
        .with_tls(|tls| tls.with_dev_cert(DevCertMode::Auto));

    App::new()
        .with_options(|opt| opt.set_http(http))
        .run()
        .await;
}
```

`DevCertMode::Auto` generates a self-signed certificate for local
development. In production supply your own certificate and key.

### JWT authentication

```rust
use neva::prelude::*;

#[tokio::main]
async fn main() {
    let secret = std::env::var("JWT_SECRET").expect("JWT_SECRET must be set");

    App::new()
        .with_options(|opt| opt
            .with_http(|http| http
                .with_auth(|auth| auth
                    .with_aud(["my-service"])
                    .with_iss(["my-issuer"])
                    .set_decoding_key(secret.as_bytes()))))
        .run()
        .await;
}
```

| Method | Purpose |
|---|---|
| `set_decoding_key()` | Secret or public key verifying signatures |
| `with_aud()` | Accepted audiences |
| `with_iss()` | Accepted issuers |
| `validate_exp()` | Validate expiry (default `true`) |

Roles and permissions then gate individual primitives, answering `403` when
a token does not satisfy them:

```rust
use neva::prelude::*;

#[tool(roles = ["admin"])]
async fn admin_tool(name: String) {
    tracing::info!("admin tool for {name}");
}

#[resource(uri = "res://restricted/{name}", permissions = ["read"])]
async fn restricted_resource(uri: Uri, name: String) -> (String, String) {
    (uri.to_string(), name)
}
```

OAuth 2.1 is available on both sides behind `server-oauth` (protected
resource metadata and token validation) and `client-oauth` (discovery,
dynamic registration, authorization code + PKCE).

### DNS-rebinding protection

A server on loopback is reachable by any page the browser loads: point
`evil.example.com` at `127.0.0.1` and the browser connects. The request is
genuinely local; the name it was addressed by is what gives the attack
away. neva validates `Origin` and `Host` and answers `403` before reading
the body.

**The default needs no call.** Bound to loopback, only loopback names are
accepted — `localhost`, `127.0.0.0/8`, `[::1]` — on any port. Bound to
anything else, everything is accepted, because the legitimate names are not
knowable from there.

```rust
use neva::prelude::*;

#[tokio::main]
async fn main() {
    let http = HttpServer::new("0.0.0.0:3000")
        .with_allowed_origins(["https://mcp.example.com", "https://app.example.com"]);

    App::new()
        .with_options(|opt| opt.set_http(http))
        .run()
        .await;
}
```

| Entry | Matches an `Origin` of |
|---|---|
| `https://app.example.com` | that scheme, host **and** port (missing port = the scheme's default) |
| `app.example.com` | that host, any scheme, any port |
| `app.example.com:8443` | that host, any scheme, that port |

Prefer the full origin — a bare host trusts everything served under that
name, including other ports. `Host` is matched by hostname either way.
Matching is case-insensitive, loopback is always accepted, and a request
with neither header is left alone.

`HttpServer::new("127.0.0.1:3000").allow_any_origin()` turns the gate off.
Only meaningful on a loopback bind, and only when something in front
already validates the name — not to silence a `403` whose cause has not
been read.

## Client setup

```rust
use neva::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let mut client = Client::new()
        .with_options(|opt| opt
            .with_http(|http| http
                .bind("localhost:7878")
                .with_tls(|tls| tls.with_certs_verification(false))   // dev only
                .with_auth("eyJhbGci...")));

    client.connect().await?;
    client.disconnect().await
}
```

`with_auth(token)` sends `Authorization: Bearer <token>` on every request.
Never disable certificate verification outside local development.

## A custom HTTP stack

`http-server` ships the engine-agnostic abstractions with **no** framework;
`http-server-volga` is the bundled default. To host the MCP endpoint on
axum, hyper, actix-web or your own adapter, implement `HttpEngine` and wire
it in:

```toml
neva = { version = "0.5", features = ["server-macros", "http-server", "tracing", "di", "tasks"] }
axum = "0.8"
```

```rust
// HttpServer::from_engine(my_engine) — then the usual
// opt.set_http(server). Auth, TLS and role gates are configured the
// same way; the DNS-rebinding gate lives in the transport core, so a
// custom engine gets it too and it survives `with_engine(...)`.
```

Working adapters live in the neva repository under `examples/axum`,
`examples/hyper` and `examples/actix`.

## Running more than one instance

Mandatory once you scale past one process, because a multi-round request
can land anywhere:

```rust
use neva::prelude::*;

#[tokio::main]
async fn main() {
    App::new()
        .with_request_state_secret(std::env::var("MCP_STATE_SECRET").unwrap().as_bytes())
        .with_options(|opt| opt.with_default_http())
        .run()
        .await;
}
```

Plus `App::with_request_state_store(<shared store>)` — see `mrtr.md` for
what each protects and why the state is sealed rather than signed.

## Feature flags

| Preset | Contains |
|---|---|
| `server-full` | `server-macros`, `tracing`, `http-server-volga`, `server-tls`, `server-oauth`, `di`, `tasks` |
| `client-full` | `client-macros`, `tracing`, `http-client`, `client-tls`, `client-oauth`, `tasks` |
| `full` | both |

Individually: `server`, `server-macros`, `http-server`,
`http-server-volga`, `server-tls`, `server-oauth`; `client`,
`client-macros`, `http-client`, `client-tls`, `client-oauth`; shared
`macros`, `di`, `tasks`, `tracing`.

`legacy-spec` is not a capability — it selects the protocol generation and
compiles the other one out. `--all-features` therefore builds the *legacy*
profile; use `--features "server-full client-full"` for the default one.

Minimal builds worth knowing:

```toml
# stdio-only server, macros, no HTTP
neva = { version = "0.5", features = ["server-macros", "tracing"] }

# lightweight HTTP client
neva = { version = "0.5", features = ["http-client"] }

# a server that is also a client (agent pattern)
neva = { version = "0.5", features = ["server-full", "http-client"] }
```

## Testing a server

```bash
npx @modelcontextprotocol/inspector cargo run     # stdio
```

For HTTP, run the server and connect the Inspector to
`http://127.0.0.1:3000/mcp`.
