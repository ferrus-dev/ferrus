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
fn complete_graph_peer_retains_its_configured_id() {
    let schema = json!({"type":"object"});
    let entries = [
        "repository_graph_status",
        "repository_search",
        "repository_context",
    ]
    .into_iter()
    .map(|remote_name| {
        let name = provider_name("repo", remote_name);
        (
            name.clone(),
            Entry {
                server: 0,
                remote_name: remote_name.into(),
                descriptor: ToolDescriptor {
                    name,
                    description: String::new(),
                    input_schema: schema.clone(),
                },
                input: jsonschema::validator_for(&schema).unwrap(),
                output: None,
                schemas: (schema.clone(), None),
                graph_equivalence: true,
            },
        )
    })
    .collect();
    let mut tools = McpTools {
        servers: vec![Server {
            id: "repo".into(),
            client: None,
            closing: None,
            timeout: Duration::from_secs(1),
        }],
        entries,
        active: None,
    };
    assert_eq!(tools.graph_peer_mode(), GraphPeerMode::Complete);
    assert_eq!(tools.graph_peer_id(), Some("repo"));
    assert_eq!(tools.graph_peer_timeout_ms(), Some(1_000));
    assert_eq!(
        tools.graph_remote_name("mcp_repo_repository_search"),
        Some("repository_search")
    );
    assert_eq!(tools.catalog_counts(), (1, 3));
    tools.servers.push(Server {
        id: "extra".into(),
        client: None,
        closing: None,
        timeout: Duration::from_secs(1),
    });
    tools.entries.insert(
        "mcp_extra_echo".into(),
        Entry {
            server: 1,
            remote_name: "echo".into(),
            descriptor: ToolDescriptor {
                name: "mcp_extra_echo".into(),
                description: String::new(),
                input_schema: schema.clone(),
            },
            input: jsonschema::validator_for(&schema).unwrap(),
            output: None,
            schemas: (schema, None),
            graph_equivalence: false,
        },
    );
    assert_eq!(tools.graph_peer_mode(), GraphPeerMode::Complete);
    assert_eq!(tools.catalog_counts(), (2, 4));
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
fn managed_graph_binding_rejects_other_commands_and_tools() {
    let temporary = tempfile::tempdir().unwrap();
    let command = toml::Value::String(std::env::current_exe().unwrap().display().to_string());
    let directory = temporary.path().join("private");
    private::directory(&directory, true).unwrap();
    let cases = [
        (
            "['serve', '--role', 'executor']",
            "['repository_search']",
            true,
        ),
        ("['serve']", "['repository_search']", false),
        ("['serve', '--role', 'executor']", "['exec']", false),
    ];
    for (index, (args, allow, expected)) in cases.into_iter().enumerate() {
        let path = directory.join(format!("peer-{index}.toml"));
        let mut file = private::file(&path, true).unwrap();
        write!(file, "[[servers]]\nid = 'graph'\ncommand = {command}\nargs = {args}\nallow = {allow}\ninherit_managed_binding = true\n").unwrap();
        drop(file);
        assert_eq!(Config::load(&path).is_ok(), expected);
    }
}

#[tokio::test]
async fn taskless_discovery_omits_peers_requiring_a_managed_binding() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("private");
    private::directory(&directory, true).unwrap();
    let path = directory.join("peers.toml");
    let command = toml::Value::String(std::env::current_exe().unwrap().display().to_string());
    let mut file = private::file(&path, true).unwrap();
    write!(file, "[[servers]]\nid = 'graph'\ncommand = {command}\nargs = ['serve', '--role', 'executor']\nallow = ['repository_search']\ninherit_managed_binding = true\n").unwrap();
    drop(file);
    let mut tools = McpTools::connect_taskless(&path, &[], &Cancellation::default())
        .await
        .unwrap();
    assert_eq!(tools.catalog_counts(), (0, 0));
    assert_eq!(tools.graph_peer_mode(), GraphPeerMode::Absent);
    assert!(tools.shutdown().await);
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
        graph_equivalence: false,
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
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/nano_mcp_peer.py");
    let script: &'static str = Box::leak(script.to_string_lossy().into_owned().into_boxed_str());
    let config = Config {
        servers: vec![ServerConfig {
            id: "local".into(),
            command,
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            inherit_managed_binding: false,
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

#[test]
fn stdio_frames_are_bounded_before_the_client_receives_them() {
    let limit = Arc::new(AtomicUsize::new(MAX_CATALOG_FRAME_BYTES));
    let mut request = Vec::new();
    forward_peer_requests(
        io::Cursor::new(
            b"{\"jsonrpc\":\"2.0\",\"method\":\"tools/call\"}\n{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n",
        ),
        &mut request,
        Arc::clone(&limit),
    )
    .unwrap();
    assert_eq!(limit.load(Ordering::Acquire), MAX_CALL_FRAME_BYTES);

    let mut forwarded = Vec::new();
    let mut oversized = vec![b'x'; MAX_CALL_FRAME_BYTES];
    oversized.push(b'\n');
    let error =
        forward_peer_responses(io::Cursor::new(oversized), &mut forwarded, &limit).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(forwarded.is_empty());

    let exact = vec![b'x'; MAX_CALL_FRAME_BYTES - 1];
    let mut exact = [exact, vec![b'\n']].concat();
    forward_peer_responses(io::Cursor::new(&exact), &mut forwarded, &limit).unwrap();
    assert_eq!(forwarded, exact);
    exact.pop();
    assert_eq!(
        read_frame(&mut io::Cursor::new(exact), || MAX_CALL_FRAME_BYTES)
            .unwrap_err()
            .kind(),
        io::ErrorKind::UnexpectedEof
    );
}

#[tokio::test]
async fn peer_results_fail_closed_and_keep_native_names_reserved() {
    // Python startup competes with Git/SQLite tests on loaded Windows CI
    // runners. Keep the connection budget separate from the short call
    // timeout exercised below.
    let startup_timeout_ms = 15_000;
    let mut normal = direct_peer("normal", startup_timeout_ms, &[])
        .await
        .unwrap();
    let result = normal.execute(&call(), &Cancellation::default()).await;
    assert!(
        matches!(result, ToolOutcome::Success(value) if value["structuredContent"]["echo"] == "hello")
    );
    assert!(normal.shutdown().await);

    let mut error = direct_peer("error", startup_timeout_ms, &[]).await.unwrap();
    assert!(
        matches!(error.execute(&call(), &Cancellation::default()).await,
        ToolOutcome::Failed(ToolError::Mcp(value)) if value["code"] == "remote_error")
    );
    assert!(error.shutdown().await);

    let mut changed = direct_peer("schema-change", startup_timeout_ms, &[])
        .await
        .unwrap();
    assert!(
        matches!(changed.execute(&call(), &Cancellation::default()).await,
        ToolOutcome::Failed(ToolError::Mcp(value)) if value["code"] == "schema_changed")
    );
    assert!(changed.shutdown().await);

    let mut oversized = direct_peer("oversize", startup_timeout_ms, &[])
        .await
        .unwrap();
    assert!(matches!(
        oversized.execute(&call(), &Cancellation::default()).await,
        ToolOutcome::Unknown(ToolError::OutputLimit)
    ));
    assert!(oversized.servers[0].client.is_none());
    assert!(oversized.shutdown().await);

    let mut invalid_output = direct_peer("invalid-output", startup_timeout_ms, &[])
        .await
        .unwrap();
    assert!(matches!(
        invalid_output.execute(&call(), &Cancellation::default()).await,
        ToolOutcome::Unknown(ToolError::Mcp(value)) if value["code"] == "invalid_output"
    ));
    assert!(invalid_output.servers[0].client.is_none());
    assert!(invalid_output.shutdown().await);

    let mut timeout = direct_peer("timeout", startup_timeout_ms, &[])
        .await
        .unwrap();
    // Startup under a loaded Windows runner can exceed one second. Only
    // the tool call itself needs the short timeout under test.
    timeout.servers[0].timeout = Duration::from_millis(1000);
    assert!(matches!(
        timeout.execute(&call(), &Cancellation::default()).await,
        ToolOutcome::Unknown(_)
    ));
    assert!(timeout.shutdown().await);

    let mut cancelled = direct_peer("timeout", startup_timeout_ms, &[])
        .await
        .unwrap();
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
    assert!(
        direct_peer("normal", startup_timeout_ms, &native)
            .await
            .is_err()
    );
}
