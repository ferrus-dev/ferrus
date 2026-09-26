# Nano external stdio tools

Build Ferrus with `--features nano-openai,nano-mcp`. External MCP tools are
opt-in through an absolute, owner-only `mcp_config_file` path in Nano's provider
settings file:

```toml
base_url = "http://127.0.0.1:1234/v1"
model = "local-model"
mcp_config_file = "/home/user/.config/ferrus/nano-mcp.toml"
```

The MCP file lists explicit stdio peers and the exact tools Nano may call:

```toml
[[servers]]
id = "local"
command = "/usr/local/bin/my-mcp-server"
args = ["--stdio"]
cwd = "/home/user/project"
allow = ["lookup"]
timeout_ms = 10000

[servers.env]
PATH = "/usr/local/bin:/usr/bin"
```

The command must be absolute. If `cwd` is set, it must be absolute; otherwise
the peer inherits Nano's working directory. The child receives only
the configured environment, so credentials from the Nano provider or HQ are
not inherited. Put any credential the peer needs in this owner-only file;
Nano does not add its contents to model requests. Peer stderr is discarded,
and its stdout is used only for the MCP protocol. Nano stdout remains the
versioned JSONL stream.

Nano connects to each peer and discovers its allowed tools before inference.
The provider sees names prefixed with `mcp_` and the configured server ID;
unusual or long tool names receive a stable hash suffix. Native Ferrus tools
retain their names and cannot be replaced by an MCP peer. Tool annotations do
not grant permission. Arguments are checked against the pinned input schema;
a changed schema fails the call before execution. Outputs are capped at 24 KiB.
An unconfirmed call is recorded as an unknown effect and the connection is
closed, preventing automatic replay.

This first release supports stdio tools only, using Ferrus's existing neva
legacy protocol profile. Sampling, elicitation, HTTP, OAuth, MCP resources,
and prompts are unsupported. Peers must handle unsupported client requests
without expecting Nano to answer them. Each configured server is a trusted
local executable with the operating-system permissions of its user; the
allowlist controls which of its tools the model can invoke.
