//! Explicit external stdio tools. Ferrus tools stay native and retain their names.

use super::{private, tools::*};
use anyhow::{Result, ensure};
use neva::client::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

const MAX_SERVERS: usize = 4;
const MAX_TOOLS: usize = 32;
const MAX_SCHEMA_BYTES: usize = 4 * 1024;
// Leave room for ToolOutcome framing in the engine's 32 KiB journal cap.
const MAX_RESULT_BYTES: usize = 24 * 1024;
const MAX_ARGUMENT_BYTES: usize = 16 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    servers: Vec<ServerConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerConfig {
    id: String,
    command: PathBuf,
    #[serde(default)]
    args: Vec<String>,
    cwd: Option<PathBuf>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// Names explicitly granted by the host. MCP annotations grant nothing.
    allow: Vec<String>,
    #[serde(default = "default_timeout")]
    timeout_ms: u64,
}

fn default_timeout() -> u64 {
    10_000
}

impl Config {
    fn load(path: &Path) -> Result<Self> {
        ensure!(path.is_absolute(), "MCP config path must be absolute");
        let mut bytes = Vec::new();
        private::read_only_file(path)
            .map_err(|_| anyhow::anyhow!("MCP config must be an owner-only regular file"))?
            .take(32 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| anyhow::anyhow!("Cannot read MCP config"))?;
        ensure!(bytes.len() <= 32 * 1024, "MCP config exceeds size limit");
        let text =
            std::str::from_utf8(&bytes).map_err(|_| anyhow::anyhow!("Invalid MCP config"))?;
        let config: Self =
            toml::from_str(text).map_err(|_| anyhow::anyhow!("Invalid MCP config"))?;
        ensure!(config.servers.len() <= MAX_SERVERS, "Too many MCP servers");
        let mut ids = BTreeSet::new();
        for server in &config.servers {
            ensure!(
                !server.id.is_empty()
                    && server.id.len() <= 24
                    && server
                        .id
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "Invalid MCP server id"
            );
            ensure!(ids.insert(&server.id), "Duplicate MCP server id");
            ensure!(
                server.command.is_absolute() && server.command.is_file(),
                "MCP command must be an absolute file"
            );
            ensure!(
                server
                    .cwd
                    .as_ref()
                    .is_none_or(|cwd| cwd.is_absolute() && cwd.is_dir()),
                "MCP cwd must be an absolute directory"
            );
            ensure!(
                (100..=120_000).contains(&server.timeout_ms),
                "Invalid MCP timeout"
            );
            ensure!(
                server.args.len() <= 32
                    && server
                        .args
                        .iter()
                        .all(|arg| arg.len() <= 2048 && !arg.contains('\0')),
                "Invalid MCP arguments"
            );
            ensure!(
                server.env.len() <= 32
                    && server.env.iter().all(|(key, value)| {
                        !key.is_empty()
                            && key.len() <= 128
                            && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                            && value.len() <= 4096
                            && !value.contains('\0')
                    }),
                "Invalid MCP environment"
            );
            ensure!(
                !server.allow.is_empty() && server.allow.len() <= MAX_TOOLS,
                "MCP tools must be explicitly allowed"
            );
            let mut allowed = BTreeSet::new();
            ensure!(
                server
                    .allow
                    .iter()
                    .all(|name| !name.is_empty() && name.len() <= 256 && allowed.insert(name)),
                "Invalid MCP tool allowlist"
            );
        }
        Ok(config)
    }
}

pub(crate) fn validate_config(path: &Path) -> Result<()> {
    Config::load(path).map(|_| ())
}

/// neva 0.6.1 owns process creation and inherits the caller environment. This
/// private peer replaces that environment before running the configured server.
/// On Unix, exec also gives neva direct ownership of the real server process.
pub(crate) fn run_peer(config_path: &Path, server_id: &str) -> Result<()> {
    let config = Config::load(config_path)?;
    let server = config
        .servers
        .iter()
        .find(|s| s.id == server_id)
        .ok_or_else(|| anyhow::anyhow!("Unknown MCP server"))?;
    let mut command = Command::new(&server.command);
    command
        .args(&server.args)
        .env_clear()
        .envs(&server.env)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::null());
    if let Some(cwd) = &server.cwd {
        command.current_dir(cwd);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec().into())
    }
    #[cfg(windows)]
    {
        let status = command.status()?;
        ensure!(status.success(), "MCP peer exited unsuccessfully");
        Ok(())
    }
}

struct Entry {
    server: usize,
    remote_name: String,
    descriptor: ToolDescriptor,
    input: jsonschema::Validator,
    output: Option<jsonschema::Validator>,
    schemas: (Value, Option<Value>),
}

struct Server {
    client: Option<Client>,
    closing: Option<tokio::task::JoinHandle<Result<(), neva::error::Error>>>,
    timeout: Duration,
}

pub(crate) struct McpTools {
    servers: Vec<Server>,
    entries: BTreeMap<String, Entry>,
    active: Option<usize>,
}

impl Drop for McpTools {
    fn drop(&mut self) {
        // Managed startup can fail after discovery but before Engine owns the
        // tools (for example, while restoring an answer). Still close peers
        // before the Nano process and its Tokio runtime exit.
        let clients: Vec<_> = self
            .servers
            .iter_mut()
            .filter_map(|server| server.client.take())
            .collect();
        if clients.is_empty() {
            return;
        }
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                let _ =
                    runtime.block_on(tokio::time::timeout(Duration::from_secs(2), async move {
                        for client in clients {
                            let _ = client.disconnect().await;
                        }
                    }));
            }
            let _ = done.send(());
        });
        // A stalled transport or runtime teardown must not block managed exit.
        let _ = finished.recv_timeout(Duration::from_secs(2));
    }
}

enum Launch {
    Peer,
    #[cfg(test)]
    Direct {
        command: &'static str,
        args: Vec<&'static str>,
    },
}

impl McpTools {
    pub(crate) async fn connect(
        config_path: &Path,
        native: &[ToolDescriptor],
        cancellation: &Cancellation,
    ) -> Result<Self> {
        let config = Config::load(config_path)?;
        let mut this = Self {
            servers: Vec::new(),
            entries: BTreeMap::new(),
            active: None,
        };
        let result = this
            .discover(config_path, config, native, Launch::Peer, cancellation)
            .await;
        if result.is_err() {
            let _ = this.shutdown().await;
        }
        result.map(|()| this)
    }

    async fn discover(
        &mut self,
        path: &Path,
        config: Config,
        native: &[ToolDescriptor],
        launch: Launch,
        cancellation: &Cancellation,
    ) -> Result<()> {
        let mut reserved: BTreeSet<_> = native.iter().map(|tool| tool.name.as_str()).collect();
        reserved.extend(super::lifecycle::NAMES.iter().copied());
        for peer in config.servers {
            ensure!(!cancellation.is_cancelled(), "MCP discovery interrupted");
            // neva's stdio API takes static strings. These bounded, non-secret
            // launcher arguments live until this Nano process exits.
            let (command, args): (&'static str, Vec<&'static str>) = match &launch {
                Launch::Peer => {
                    let command = std::env::current_exe()?;
                    let command =
                        Box::leak(command.to_string_lossy().into_owned().into_boxed_str());
                    let config_arg =
                        Box::leak(path.to_string_lossy().into_owned().into_boxed_str());
                    let id_arg = Box::leak(peer.id.clone().into_boxed_str());
                    (
                        command,
                        vec![
                            "nano", "mcp-peer", "--config", config_arg, "--server", id_arg,
                        ],
                    )
                }
                #[cfg(test)]
                Launch::Direct { command, args } => (*command, args.clone()),
            };
            let timeout = Duration::from_millis(peer.timeout_ms);
            let mut client = Client::new()
                .with_options(|options| options.with_stdio(command, args).with_timeout(timeout));
            let connected = tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    let _ = tokio::time::timeout(Duration::from_secs(1), client.disconnect()).await;
                    anyhow::bail!("MCP discovery interrupted");
                }
                result = tokio::time::timeout(timeout, client.connect()) => result,
            };
            if !matches!(connected, Ok(Ok(()))) {
                let _ = tokio::time::timeout(Duration::from_secs(1), client.disconnect()).await;
                anyhow::bail!("Cannot connect MCP server {}", peer.id);
            }
            let index = self.servers.len();
            self.servers.push(Server {
                client: Some(client),
                closing: None,
                timeout,
            });
            let allowed: BTreeSet<_> = peer.allow.iter().cloned().collect();
            let mut found = BTreeSet::new();
            let mut cursor = None;
            let mut cursors = BTreeSet::new();
            for _ in 0..8 {
                let page = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => anyhow::bail!("MCP discovery interrupted"),
                    result = tokio::time::timeout(
                        timeout,
                        self.servers[index].client.as_mut().unwrap().list_tools(cursor),
                    ) => result??,
                };
                ensure!(page.tools.len() <= MAX_TOOLS, "MCP catalog exceeds limit");
                for tool in page.tools {
                    if !allowed.contains(&tool.name) {
                        continue;
                    }
                    ensure!(
                        !reserved.contains(tool.name.as_str()),
                        "Native Ferrus tool cannot be configured through MCP"
                    );
                    ensure!(found.insert(tool.name.clone()), "Duplicate MCP tool name");
                    let name = provider_name(&peer.id, &tool.name);
                    ensure!(
                        !reserved.contains(name.as_str()) && !self.entries.contains_key(&name),
                        "MCP name collision"
                    );
                    let schema = serde_json::to_value(&tool.input_schema)?;
                    ensure!(
                        schema.is_object()
                            && serde_json::to_vec(&schema)?.len() <= MAX_SCHEMA_BYTES
                            && safe_schema(&schema, 0),
                        "Invalid MCP input schema"
                    );
                    let input = jsonschema::validator_for(&schema)
                        .map_err(|_| anyhow::anyhow!("Invalid MCP input schema"))?;
                    let output_schema = tool
                        .output_schema
                        .as_ref()
                        .map(serde_json::to_value)
                        .transpose()?;
                    let output = match &output_schema {
                        Some(schema) => {
                            ensure!(
                                serde_json::to_vec(schema)?.len() <= MAX_SCHEMA_BYTES
                                    && safe_schema(schema, 0),
                                "Invalid MCP output schema"
                            );
                            Some(
                                jsonschema::validator_for(schema)
                                    .map_err(|_| anyhow::anyhow!("Invalid MCP output schema"))?,
                            )
                        }
                        None => None,
                    };
                    let description = tool.descr.unwrap_or_default();
                    ensure!(
                        description.len() <= 1024 && self.entries.len() < MAX_TOOLS,
                        "MCP catalog exceeds limit"
                    );
                    self.entries.insert(
                        name.clone(),
                        Entry {
                            server: index,
                            remote_name: tool.name,
                            descriptor: ToolDescriptor {
                                name,
                                description,
                                input_schema: schema.clone(),
                            },
                            input,
                            output,
                            schemas: (schema, output_schema),
                        },
                    );
                }
                cursor = page.next_cursor;
                if let Some(next) = cursor {
                    ensure!(cursors.insert(next), "MCP catalog cursor loop");
                } else {
                    break;
                }
            }
            ensure!(cursor.is_none(), "MCP catalog pagination exceeds limit");
            ensure!(found == allowed, "Configured MCP tool is unavailable");
        }
        Ok(())
    }

    pub(crate) fn descriptors(&self) -> Vec<ToolDescriptor> {
        self.entries
            .values()
            .map(|entry| ToolDescriptor {
                name: entry.descriptor.name.clone(),
                description: entry.descriptor.description.clone(),
                input_schema: entry.descriptor.input_schema.clone(),
            })
            .collect()
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    pub(crate) fn validate(&self, name: &str, arguments: &Value) -> Result<(), ToolError> {
        let entry = self.entries.get(name).ok_or(ToolError::UnknownTool)?;
        if !arguments.is_object()
            || serde_json::to_vec(arguments).map_or(true, |v| v.len() > MAX_ARGUMENT_BYTES)
            || !entry.input.is_valid(arguments)
        {
            return Err(ToolError::InvalidArguments);
        }
        Ok(())
    }

    pub(crate) async fn execute(
        &mut self,
        call: &ValidatedCall,
        cancellation: &Cancellation,
    ) -> ToolOutcome {
        let Some(entry) = self.entries.get(&call.name) else {
            return ToolOutcome::Failed(ToolError::UnknownTool);
        };
        if self.validate(&call.name, &call.arguments).is_err() {
            return ToolOutcome::Failed(ToolError::InvalidArguments);
        }
        let server_index = entry.server;
        let remote_name = entry.remote_name.clone();
        let expected_schemas = entry.schemas.clone();
        let timeout = self.servers[server_index].timeout;
        self.active = Some(server_index);
        // Fail closed on a changed declaration. The provider's schema stays
        // pinned to the one that was advertised for this session.
        let result = async {
            let client = self.servers[server_index]
                .client
                .as_mut()
                .ok_or(ToolOutcome::Failed(ToolError::Failed))?;
            let current = current_schema(client, &remote_name)
                .await
                .map_err(ToolOutcome::Failed)?;
            if current.as_ref() != Some(&expected_schemas) {
                return Err(ToolOutcome::Failed(ToolError::Mcp(
                    json!({"code":"schema_changed"}),
                )));
            }
            let args: HashMap<String, Value> = call
                .arguments
                .as_object()
                .unwrap()
                .clone()
                .into_iter()
                .collect();
            client.call_tool(remote_name, args).await.map_err(|_| {
                ToolOutcome::Unknown(ToolError::Mcp(json!({"code":"call_unconfirmed"})))
            })
        };
        let outcome = tokio::select! {
            biased;
            _ = cancellation.cancelled() => ToolOutcome::Unknown(ToolError::Interrupted),
            result = tokio::time::timeout(timeout, result) => match result {
                Ok(Ok(response)) => {
                    let output = serde_json::to_value(&response);
                    match output {
                        Ok(value) if serde_json::to_vec(&value).is_ok_and(|bytes| bytes.len() <= MAX_RESULT_BYTES) => {
                            if response.is_error { ToolOutcome::Failed(ToolError::Mcp(json!({"code":"remote_error", "result":value}))) }
                            else if self.entries[&call.name].output.as_ref().is_some_and(|schema| response.struct_content.as_ref().is_none_or(|v| !schema.is_valid(v))) {
                                ToolOutcome::Failed(ToolError::Mcp(json!({"code":"invalid_output"})))
                            } else { ToolOutcome::Success(value) }
                        },
                        _ => ToolOutcome::Failed(ToolError::OutputLimit),
                    }
                },
                Ok(Err(outcome)) => outcome,
                Err(_) => ToolOutcome::Unknown(ToolError::Mcp(json!({"code":"timeout"}))),
            }
        };
        if matches!(outcome, ToolOutcome::Unknown(_)) {
            let _ = self.disconnect(server_index).await;
        }
        self.active = None;
        outcome
    }

    async fn disconnect(&mut self, index: usize) -> bool {
        let server = &mut self.servers[index];
        if let Some(client) = server.client.take() {
            // The task owns teardown even if the engine drops this future at
            // its own deadline. Dropping a JoinHandle does not cancel it.
            server.closing = Some(tokio::spawn(async move { client.disconnect().await }));
        }
        let Some(closing) = &mut server.closing else {
            return true;
        };
        match tokio::time::timeout(Duration::from_secs(2), closing).await {
            Ok(result) => {
                server.closing = None;
                matches!(result, Ok(Ok(())))
            }
            Err(_) => false,
        }
    }

    pub(crate) async fn interrupted(&mut self) -> Option<ToolOutcome> {
        let index = self.active.take()?;
        let _ = self.disconnect(index).await;
        Some(ToolOutcome::Unknown(ToolError::Interrupted))
    }

    pub(crate) async fn shutdown(&mut self) -> bool {
        self.active = None;
        let mut clean = true;
        for index in 0..self.servers.len() {
            clean &= self.disconnect(index).await;
        }
        clean
    }
}

async fn current_schema(
    client: &mut Client,
    name: &str,
) -> Result<Option<(Value, Option<Value>)>, ToolError> {
    let mut cursor = None;
    let mut cursors = BTreeSet::new();
    let mut result = None;
    for _ in 0..8 {
        let page = client
            .list_tools(cursor)
            .await
            .map_err(|_| ToolError::Failed)?;
        if page.tools.len() > MAX_TOOLS {
            return Err(ToolError::Failed);
        }
        for tool in page.tools {
            if tool.name == name {
                if result.is_some() {
                    return Err(ToolError::Failed);
                }
                result = Some((
                    serde_json::to_value(tool.input_schema).map_err(|_| ToolError::Failed)?,
                    tool.output_schema
                        .map(|schema| serde_json::to_value(schema).map_err(|_| ToolError::Failed))
                        .transpose()?,
                ));
            }
        }
        cursor = page.next_cursor;
        if let Some(next) = cursor {
            if !cursors.insert(next) {
                return Err(ToolError::Failed);
            }
        } else {
            return Ok(result);
        }
    }
    Err(ToolError::Failed)
}

fn safe_schema(value: &Value, depth: usize) -> bool {
    if depth > 32 {
        return false;
    }
    match value {
        Value::Object(fields) => fields.iter().all(|(key, value)| {
            if matches!(key.as_str(), "$ref" | "$dynamicRef" | "$recursiveRef") {
                value
                    .as_str()
                    .is_some_and(|reference| reference.starts_with('#'))
            } else {
                safe_schema(value, depth + 1)
            }
        }),
        Value::Array(values) => values.iter().all(|value| safe_schema(value, depth + 1)),
        _ => true,
    }
}

fn provider_name(server: &str, tool: &str) -> String {
    let simple = format!("mcp_{server}_{tool}");
    if simple.len() <= 64
        && simple
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return simple;
    }
    let digest = Sha256::digest([server.as_bytes(), b"\0", tool.as_bytes()].concat());
    let mut suffix = String::with_capacity(32);
    for byte in &digest[..16] {
        use std::fmt::Write;
        let _ = write!(suffix, "{byte:02x}");
    }
    format!("mcp_{server}_{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn names_are_stable_bounded_and_isolated() {
        assert_eq!(provider_name("local", "echo"), "mcp_local_echo");
        let first = provider_name("local", "a/b");
        assert_eq!(first, provider_name("local", "a/b"));
        assert_ne!(first, provider_name("local", "a_b"));
        assert_ne!(first, provider_name("other", "a/b"));
        assert!(first.len() <= 64);
        assert!(
            first
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        );
    }

    #[test]
    fn config_requires_private_explicit_allowlist() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("private");
        private::directory(&directory, true).unwrap();
        let command = std::env::current_exe().unwrap();
        let valid = directory.join("valid.toml");
        let mut file = private::file(&valid, true).unwrap();
        write!(
            file,
            "[[servers]]\nid = 'test'\ncommand = '{}'\nallow = ['echo']\n",
            command.display()
        )
        .unwrap();
        drop(file);
        assert!(Config::load(&valid).is_ok());
        let duplicate = directory.join("duplicate.toml");
        let mut file = private::file(&duplicate, true).unwrap();
        write!(
            file,
            "[[servers]]\nid = 'test'\ncommand = '{}'\nallow = ['echo', 'echo']\n",
            command.display()
        )
        .unwrap();
        drop(file);
        assert!(Config::load(&duplicate).is_err());
        let implicit = directory.join("implicit.toml");
        let mut file = private::file(&implicit, true).unwrap();
        write!(
            file,
            "[[servers]]\nid = 'test'\ncommand = '{}'\nallow = []\n",
            command.display()
        )
        .unwrap();
        drop(file);
        assert!(Config::load(&implicit).is_err());
    }

    #[test]
    fn arguments_follow_pinned_schema() {
        let schema = json!({"type":"object", "properties":{"text":{"type":"string"}}, "required":["text"], "additionalProperties":false});
        let descriptor = ToolDescriptor {
            name: "mcp_test_echo".into(),
            description: "echo".into(),
            input_schema: schema.clone(),
        };
        let entry = Entry {
            server: 0,
            remote_name: "echo".into(),
            descriptor,
            input: jsonschema::validator_for(&schema).unwrap(),
            output: None,
            schemas: (schema, None),
        };
        let tools = McpTools {
            servers: Vec::new(),
            entries: BTreeMap::from([("mcp_test_echo".into(), entry)]),
            active: None,
        };
        assert!(
            tools
                .validate("mcp_test_echo", &json!({"text":"hello"}))
                .is_ok()
        );
        assert!(tools.validate("mcp_test_echo", &json!({"text":4})).is_err());
        assert!(
            tools
                .validate("mcp_test_echo", &json!({"text":"hello", "extra":true}))
                .is_err()
        );
        assert!(tools.validate("mcp_test_echo", &Value::Null).is_err());
        assert!(tools.validate("echo", &json!({})).is_err());
        assert!(safe_schema(&json!({"$ref":"#/$defs/args"}), 0));
        assert!(!safe_schema(
            &json!({"$ref":"https://example.invalid/schema"}),
            0
        ));
    }

    fn python() -> &'static str {
        for directory in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
            for executable in ["python3", "python", "python.exe"] {
                let path = directory.join(executable);
                if path.is_file() {
                    return Box::leak(
                        path.canonicalize()
                            .unwrap()
                            .to_string_lossy()
                            .into_owned()
                            .into_boxed_str(),
                    );
                }
            }
        }
        panic!("Python 3 is required for the Nano MCP peer fixture");
    }

    async fn direct_peer(
        mode: &'static str,
        timeout_ms: u64,
        native: &[ToolDescriptor],
    ) -> Result<McpTools> {
        let command = PathBuf::from(python());
        let script =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/nano_mcp_peer.py");
        let script: &'static str =
            Box::leak(script.to_string_lossy().into_owned().into_boxed_str());
        let config = Config {
            servers: vec![ServerConfig {
                id: "local".into(),
                command,
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
                allow: vec!["echo".into()],
                timeout_ms,
            }],
        };
        let mut tools = McpTools {
            servers: Vec::new(),
            entries: BTreeMap::new(),
            active: None,
        };
        let result = tools
            .discover(
                Path::new("/unused"),
                config,
                native,
                Launch::Direct {
                    command: python(),
                    args: vec!["-u", script, mode],
                },
                &Cancellation::default(),
            )
            .await;
        if result.is_err() {
            let _ = tools.shutdown().await;
        }
        result.map(|()| tools)
    }

    fn call() -> ValidatedCall {
        ValidatedCall {
            call_id: "one".into(),
            provider_call_id: "peer-one".into(),
            name: "mcp_local_echo".into(),
            arguments: json!({"text":"hello"}),
        }
    }

    #[tokio::test]
    async fn peer_results_fail_closed_and_keep_native_names_reserved() {
        let mut normal = direct_peer("normal", 5000, &[]).await.unwrap();
        let result = normal.execute(&call(), &Cancellation::default()).await;
        assert!(
            matches!(result, ToolOutcome::Success(value) if value["structuredContent"]["echo"] == "hello")
        );
        assert!(normal.shutdown().await);

        let mut error = direct_peer("error", 5000, &[]).await.unwrap();
        assert!(
            matches!(error.execute(&call(), &Cancellation::default()).await,
            ToolOutcome::Failed(ToolError::Mcp(value)) if value["code"] == "remote_error")
        );
        assert!(error.shutdown().await);

        let mut changed = direct_peer("schema-change", 5000, &[]).await.unwrap();
        assert!(
            matches!(changed.execute(&call(), &Cancellation::default()).await,
            ToolOutcome::Failed(ToolError::Mcp(value)) if value["code"] == "schema_changed")
        );
        assert!(changed.shutdown().await);

        let mut oversized = direct_peer("oversize", 5000, &[]).await.unwrap();
        assert!(matches!(
            oversized.execute(&call(), &Cancellation::default()).await,
            ToolOutcome::Failed(ToolError::OutputLimit)
        ));
        assert!(oversized.shutdown().await);

        let mut timeout = direct_peer("timeout", 5000, &[]).await.unwrap();
        // Startup under a loaded Windows runner can exceed one second. Only
        // the tool call itself needs the short timeout under test.
        timeout.servers[0].timeout = Duration::from_millis(1000);
        assert!(matches!(
            timeout.execute(&call(), &Cancellation::default()).await,
            ToolOutcome::Unknown(_)
        ));
        assert!(timeout.shutdown().await);

        let mut cancelled = direct_peer("timeout", 5000, &[]).await.unwrap();
        let cancellation = Cancellation::default();
        let trigger = cancellation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            trigger.cancel();
        });
        assert!(matches!(
            cancelled.execute(&call(), &cancellation).await,
            ToolOutcome::Unknown(ToolError::Interrupted)
        ));
        assert!(cancelled.shutdown().await);

        let native = [ToolDescriptor {
            name: "echo".into(),
            description: String::new(),
            input_schema: json!({"type":"object"}),
        }];
        assert!(direct_peer("normal", 5000, &native).await.is_err());
    }
}
