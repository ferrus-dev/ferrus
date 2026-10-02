//! Real process/HTTP/SQLite regression for the opt-in native Executor frontend.
#![cfg(feature = "nano-openai")]

// Reuse the native private-file implementation for host configuration fixtures.
#[path = "../src/nano/private.rs"]
#[allow(dead_code, unused_imports)]
mod private;

use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

fn ferrus(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ferrus"));
    command.current_dir(root);
    command
}
fn success(command: &mut Command) -> String {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "status={}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
fn git(root: &Path, args: &[&str]) -> String {
    success(Command::new("git").current_dir(root).args(args))
}
fn private_config(path: &Path, text: &str) {
    let mut file = private::file(path, true).unwrap();
    file.write_all(text.as_bytes()).unwrap();
    drop(file);
    // Fail at provisioning with the actual security error, before launching a child.
    let mut bytes = String::new();
    private::read_only_file(path)
        .unwrap()
        .read_to_string(&mut bytes)
        .unwrap();
    assert_eq!(bytes, text);
}
fn log_tail(path: &Path) -> std::io::Result<String> {
    let mut file = fs::File::open(path)?;
    let start = file.metadata()?.len().saturating_sub(8192);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(8192).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

struct Process(Child, tempfile::NamedTempFile);
impl Process {
    fn spawn(command: &mut Command) -> Self {
        // A pipe read only after exit can fill and block the child on Windows.
        let stderr = tempfile::NamedTempFile::new().unwrap();
        let child = command.stderr(stderr.reopen().unwrap()).spawn().unwrap();
        Self(child, stderr)
    }

    fn stderr(&self) -> String {
        log_tail(self.1.path()).unwrap()
    }

    fn failure_diagnostics(&mut self) -> String {
        let _ = self.0.kill();
        let status = self.0.wait();
        format!("status={status:?}\nstderr tail:\n{}", self.stderr())
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn process_diagnostics_do_not_block_verbose_children() {
    const CHILD: &str = "FERRUS_TEST_VERBOSE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        std::io::stderr()
            .write_all(&vec![b'x'; 256 * 1024])
            .unwrap();
        return;
    }

    let mut child = Process::spawn(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "process_diagnostics_do_not_block_verbose_children",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null()),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success(), "{}", child.stderr());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "verbose child did not exit: {}",
            child.failure_diagnostics()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(child.stderr(), "x".repeat(8192));
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    data: PathBuf,
    workspace: PathBuf,
    baseline: String,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let data = root.join(".ferrus/data");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(root.join(".ferrus/tasks")).unwrap();
        fs::write(
            root.join(".ferrus/project.toml"),
            toml::to_string(&json!({"project_id":"nano-e2e", "name":"test", "data_dir":data}))
                .unwrap(),
        )
        .unwrap();
        fs::write(data.join("project.toml"), toml::to_string(&json!({"id":"nano-e2e", "name":"test", "workspace_dir":root, "ferrus_dir":root.join(".ferrus"), "created_at":"2026-09-19T00:00:00Z", "last_opened_at":"2026-09-19T00:00:00Z", "version":1})).unwrap()).unwrap();
        fs::write(root.join("ferrus.toml"), "[checks]\ncommands = ['echo check-output-not-a-frame']\n[limits]\nmax_check_retries = 3\nmax_review_cycles = 3\nmax_feedback_lines = 30\nwait_timeout_secs = 1\n").unwrap();
        success(ferrus(&root).arg("recover"));
        fs::write(
            root.join(".ferrus/tasks/t-001.md"),
            "Create answer.txt, check and submit.",
        )
        .unwrap();
        fs::write(root.join(".gitignore"), ".ferrus/\n").unwrap();
        git(&root, &["init", "--quiet"]);
        git(&root, &["config", "core.autocrlf", "false"]);
        git(&root, &["add", "ferrus.toml", ".gitignore"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Nano Test",
                "-c",
                "user.email=nano@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        );
        let baseline = git(&root, &["rev-parse", "HEAD^{tree}"]);
        fs::create_dir_all(data.join("worktrees/.baseline-trees")).unwrap();
        fs::write(data.join("worktrees/.baseline-trees/t-001.txt"), &baseline).unwrap();
        let workspace = data.join("worktrees/t-001");
        git(
            &root,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                workspace.strip_prefix(&root).unwrap().to_str().unwrap(),
                "HEAD",
            ],
        );
        fs::create_dir_all(workspace.join(".ferrus")).unwrap();
        fs::copy(
            root.join(".ferrus/project.toml"),
            workspace.join(".ferrus/project.toml"),
        )
        .unwrap();
        let db = Connection::open(data.join("ferrus.db")).unwrap();
        db.execute(
            "INSERT INTO tasks(id,path,status) VALUES ('t-001','.ferrus/tasks/t-001.md','pending')",
            [],
        )
        .unwrap();
        db.execute("INSERT INTO runs(id,task_id,role,agent,status,started_at,updated_at,workspace_path) VALUES ('nano-e2e-run','t-001','executor','executor:nano:1','running','2026-09-19T00:00:00Z','2026-09-19T00:00:00Z',?1)", [workspace.to_str().unwrap()]).unwrap();
        Self {
            _dir: dir,
            root,
            data,
            workspace,
            baseline,
        }
    }
    fn command(&self, config: &Path) -> Command {
        let mut command = ferrus(&self.workspace);
        command
            .args(["--debug", "nano", "run", "--config"])
            .arg(config)
            .args(["--model", "override-model"])
            .env("FERRUS_PROJECT_ROOT", &self.root)
            .env("FERRUS_AGENT_ID", "executor:nano:1")
            .env("FERRUS_TASK_ID", "t-001")
            .env("FERRUS_RUN_ID", "nano-e2e-run")
            .env("FERRUS_BASELINE_TREE", &self.baseline)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
}

fn serve(
    listener: TcpListener,
    resumed: Option<(PathBuf, &'static str)>,
    external_mcp: bool,
    graph_peer: bool,
    native_context: bool,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        for turn in 0..3 {
            let deadline = Instant::now() + Duration::from_secs(60);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(err)
                        if err.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    Err(err) => panic!("mock API: {err}"),
                }
            };
            // Accepted sockets can inherit the listener's nonblocking mode.
            // Only accept polls; request reads use the bounded blocking timeout.
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(30)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse::<usize>().unwrap();
                }
            }
            assert!((1..=512 * 1024).contains(&length));
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let request: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(request["model"], "override-model");
            for tool in request["tools"].as_array().unwrap() {
                let schema = &tool["function"]["parameters"];
                assert_eq!(schema["type"], "object", "{tool}");
                assert!(schema["properties"].is_object(), "{tool}");
            }
            assert_eq!(
                request["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|tool| tool["function"]["name"] == "repository_search"),
                native_context
            );
            if turn == 0
                && let Some((database, state)) = &resumed
            {
                assert!(request["messages"].to_string().contains("Use forty-two."));
                let db = Connection::open(database).unwrap();
                let restored: String = db
                    .query_row("SELECT status FROM tasks WHERE id='t-001'", [], |row| {
                        row.get(0)
                    })
                    .unwrap();
                assert_eq!(&restored, state);
            }
            let calls = match turn {
                0 => {
                    let mut calls = vec![
                        (
                            "apply_patch",
                            json!({"edits":[{"operation":"create","path":"answer.txt","content":"42\n"}]}),
                        ),
                        (
                            "exec",
                            json!({"command":"echo not-a-json-event", "cwd":".", "timeout_ms":1000}),
                        ),
                    ];
                    if external_mcp {
                        assert!(
                            request["tools"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .any(|tool| tool["function"]["name"] == "mcp_local_echo")
                        );
                        calls.insert(0, ("mcp_local_echo", json!({"text":"from-mcp"})));
                    }
                    if graph_peer {
                        for name in [
                            "mcp_graph_repository_graph_status",
                            "mcp_graph_repository_search",
                            "mcp_graph_repository_context",
                        ] {
                            assert!(
                                request["tools"]
                                    .as_array()
                                    .unwrap()
                                    .iter()
                                    .any(|tool| tool["function"]["name"] == name)
                            );
                        }
                        calls.insert(
                            0,
                            (
                                "mcp_graph_repository_context",
                                json!({"seeds":[{"type":"path","value":"ferrus.toml"}]}),
                            ),
                        );
                        calls.insert(
                            0,
                            ("mcp_graph_repository_search", json!({"query":"ferrus"})),
                        );
                        calls.insert(0, ("mcp_graph_repository_graph_status", json!({})));
                    }
                    calls
                }
                1 => vec![("check", json!({}))],
                _ => vec![(
                    "submit",
                    json!({"content":"Created answer.txt; checks pass."}),
                )],
            };
            let calls: Vec<Value> = calls.into_iter().enumerate().map(|(i,(name,args))| json!({"index":i,"id":format!("t{turn}-{i}"),"type":"function","function":{"name":name,"arguments":args.to_string()}})).collect();
            let data = json!({"choices":[{"index":0,"delta":{"tool_calls":calls},"finish_reason":"tool_calls"}]});
            let sse = format!("data: {data}\n\ndata: [DONE]\n\n");
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}", sse.len()).unwrap();
        }
    })
}

#[test]
fn headless_process_edits_checks_and_submits_an_isolated_task() {
    run_headless_task(None, false, false, true, true);
}

#[test]
fn headless_http_rejection_fails_the_task_and_preserves_safe_diagnostics() {
    let fixture = Fixture::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "No provider request");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("mock API: {error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut length = 0;
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse::<usize>().unwrap();
            }
        }
        assert!((1..=512 * 1024).contains(&length));
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        let body = r#"{"error":"secret-server-detail: invalid tool schema"}"#;
        write!(stream, "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    });
    let config = fixture.root.join("nano.toml");
    private_config(
        &config,
        &format!("base_url = {url:?}\nmodel = 'fixture-model'\n"),
    );
    let mut child = Process::spawn(&mut fixture.command(&config));
    let mut stdin = child.0.stdin.take().unwrap();
    let reader = BufReader::new(child.0.stdout.take().unwrap());
    let (tx, rx) = mpsc::channel();
    let output = std::thread::spawn(move || {
        for line in reader.lines() {
            tx.send(serde_json::from_str::<Value>(&line.unwrap()).unwrap())
                .unwrap();
        }
    });
    let ready = rx
        .recv_timeout(Duration::from_secs(30))
        .unwrap_or_else(|error| panic!("No ready event: {error}; {}", child.failure_diagnostics()));
    assert_eq!(ready["event"]["type"], "ready");
    writeln!(stdin, "{{\"version\":1,\"command\":\"start\"}}").unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "{}", child.failure_diagnostics());
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(!status.success());
    output.join().unwrap();
    server.join().unwrap();
    let events: Vec<_> = rx.try_iter().collect();
    let ended = events
        .iter()
        .find(|event| event["event"]["type"] == "ended")
        .unwrap();
    assert_eq!(ended["event"]["reason"]["reason"], "provider_protocol");
    assert_eq!(ended["event"]["durable"], true);
    assert!(!events.iter().any(|event| event["event"]["type"] == "error"));
    let stderr = child.stderr();
    assert!(stderr.contains("Unsupported"), "{stderr}");
    assert!(stderr.contains("400"), "{stderr}");
    assert!(!stderr.contains("secret-server-detail"));
    let journal =
        fs::read_to_string(fixture.data.join("nano/sessions/nano-e2e-run/events.jsonl")).unwrap();
    let records: Vec<Value> = journal
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let failed = records
        .iter()
        .find(|record| record["event"]["event"] == "model_failed")
        .unwrap();
    assert_eq!(
        failed["event"]["diagnostic"],
        json!({"source":"http","status":400})
    );
    assert_eq!(failed["event"]["retryable"], false);
    assert_eq!(
        records
            .iter()
            .filter(|record| record["event"]["event"] == "model_started")
            .count(),
        1
    );
    assert!(!journal.contains("secret-server-detail"));
    let db = Connection::open(fixture.data.join("ferrus.db")).unwrap();
    let state: (String, String, Option<String>, i64) = db
        .query_row(
            "SELECT status,failure_reason,claimed_by,check_retries FROM tasks WHERE id='t-001'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        state,
        ("failed".into(), "nano_provider_protocol".into(), None, 0)
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join(".ferrus/tasks/t-001.md")).unwrap(),
        "Create answer.txt, check and submit."
    );
}

#[test]
fn headless_native_context_and_working_set_can_be_disabled_separately() {
    run_headless_task(None, false, false, false, true);
    run_headless_task(None, false, false, true, false);
}

#[cfg(feature = "nano-mcp")]
#[test]
fn headless_process_calls_isolated_external_stdio_tool() {
    run_headless_task(None, true, false, true, true);
}

#[cfg(feature = "nano-mcp")]
#[test]
fn headless_process_calls_bound_ferrus_graph_over_mcp() {
    run_headless_task(None, false, true, false, false);
}

#[cfg(feature = "nano-mcp")]
#[test]
fn mcp_peer_rejects_oversized_stdout_before_transport() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let config = root.join("mcp.toml");
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/nano_mcp_peer.py");
    private_config(
        &config,
        &format!(
            "[[servers]]\nid = 'local'\ncommand = {}\nargs = {}\nallow = ['echo']\n",
            toml::Value::String(find_python().display().to_string()),
            serde_json::to_string(&[script.display().to_string(), "oversize-startup".into()])
                .unwrap(),
        ),
    );

    let output = ferrus(&root)
        .args(["nano", "mcp-peer", "--config"])
        .arg(&config)
        .args(["--server", "local"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("MCP stdio frame exceeds limit"));

    let call_config = root.join("call-mcp.toml");
    private_config(
        &call_config,
        &format!(
            "[[servers]]\nid = 'local'\ncommand = {}\nargs = {}\nallow = ['echo']\n",
            toml::Value::String(find_python().display().to_string()),
            serde_json::to_string(&[script.display().to_string(), "oversize".into()]).unwrap(),
        ),
    );
    let mut child = ferrus(&root)
        .args(["nano", "mcp-peer", "--config"])
        .arg(&call_config)
        .args(["--server", "local"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"echo\",\"arguments\":{\"text\":\"hello\"}}}\n",
        )
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("MCP stdio frame exceeds limit"));
}

#[cfg(feature = "nano-mcp")]
#[test]
fn mcp_discovery_keeps_the_claim_alive_and_observes_cancel() {
    let fixture = Fixture::new();
    let mut config = fs::OpenOptions::new()
        .append(true)
        .open(fixture.root.join("ferrus.toml"))
        .unwrap();
    write!(
        config,
        "\n[lease]\nttl_secs = 6\nheartbeat_interval_secs = 1\n"
    )
    .unwrap();
    drop(config);

    let marker = fixture.data.join("listing-started");
    let peer = fixture.data.join("mcp.toml");
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/nano_mcp_peer.py");
    private_config(
        &peer,
        &format!(
            "[[servers]]\nid = 'local'\ncommand = {}\nargs = {}\nallow = ['echo']\ntimeout_ms = 120000\n",
            toml::Value::String(find_python().display().to_string()),
            serde_json::to_string(&[
                script.display().to_string(),
                "stall-list".into(),
                marker.display().to_string(),
            ])
            .unwrap(),
        ),
    );
    let settings = fixture.data.join("provider.toml");
    private_config(
        &settings,
        &format!(
            "base_url = 'http://127.0.0.1:1234/v1'\nmodel = 'configured-model'\nmcp_config_file = {}\n",
            toml::Value::String(peer.display().to_string()),
        ),
    );

    let mut child = Process::spawn(&mut fixture.command(&settings));
    let mut stdin = child.0.stdin.take().unwrap();
    let stdout = child.0.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            tx.send(serde_json::from_str::<Value>(&line.unwrap()).unwrap())
                .unwrap();
        }
    });
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(30)).unwrap(),
        json!({"version":1,"event":{"type":"ready"}})
    );
    writeln!(stdin, "{{\"version\":1,\"command\":\"start\"}}").unwrap();

    let database = fixture.data.join("ferrus.db");
    let deadline = Instant::now() + Duration::from_secs(10);
    let initial_heartbeat = loop {
        let db = Connection::open(&database).unwrap();
        let (claimed_by, heartbeat): (Option<String>, Option<String>) = db
            .query_row(
                "SELECT claimed_by, last_heartbeat FROM tasks WHERE id='t-001'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        if marker.exists() {
            assert_eq!(claimed_by.as_deref(), Some("executor:nano:1"));
            break heartbeat.unwrap();
        }
        assert!(Instant::now() < deadline, "{}", child.failure_diagnostics());
        std::thread::sleep(Duration::from_millis(20));
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let db = Connection::open(&database).unwrap();
        let heartbeat: String = db
            .query_row(
                "SELECT last_heartbeat FROM tasks WHERE id='t-001'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        if heartbeat != initial_heartbeat {
            break;
        }
        assert!(Instant::now() < deadline, "{}", child.failure_diagnostics());
        std::thread::sleep(Duration::from_millis(20));
    }

    let cancelled_at = Instant::now();
    writeln!(stdin, "{{\"version\":1,\"command\":\"cancel\"}}").unwrap();
    let mut ended = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(event) = rx.recv_timeout(Duration::from_millis(20))
            && event["event"]["type"] == "ended"
        {
            ended = Some(event);
        }
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success(), "{}", child.stderr());
            break;
        }
        assert!(Instant::now() < deadline, "{}", child.failure_diagnostics());
    }
    assert!(cancelled_at.elapsed() < Duration::from_secs(10));
    reader.join().unwrap();
    for event in rx.try_iter() {
        if event["event"]["type"] == "ended" {
            ended = Some(event);
        }
    }
    let ended = ended.expect("cancelled discovery must record a terminal event");
    assert_eq!(ended["event"]["reason"]["reason"], "cancelled");
    assert_eq!(ended["event"]["durable"], true);
}

#[test]
fn relaunched_human_waiter_delivers_the_answer_before_inference_and_submits() {
    for state in ["executing", "addressing"] {
        run_headless_task(Some((state, true)), false, false, true, true);
    }
}

#[test]
fn relaunched_consultation_delivers_the_response_before_inference_and_submits() {
    for state in ["executing", "addressing"] {
        run_headless_task(Some((state, false)), false, false, true, true);
    }
}

fn run_headless_task(
    resume: Option<(&'static str, bool)>,
    external_mcp: bool,
    graph_peer: bool,
    native_context: bool,
    working_set: bool,
) {
    let fixture = Fixture::new();
    let (request_file, response_file, restored_event) = match resume {
        Some((_, false)) => (
            "CONSULT_REQUEST.md",
            "CONSULT_RESPONSE.md",
            "task_consultation_resolved",
        ),
        _ => ("QUESTION.md", "ANSWER.md", "task_human_answered"),
    };
    if let Some((state, human)) = resume {
        let directory = fixture.root.join(".ferrus/runs/t-001");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join(request_file), "Which answer?").unwrap();
        fs::write(directory.join(response_file), "Use forty-two.").unwrap();
        if state == "addressing" {
            fs::write(directory.join("REVIEW.md"), "Address the stored guidance.").unwrap();
        }
        let db = Connection::open(fixture.data.join("ferrus.db")).unwrap();
        db.execute(
            "INSERT INTO runs(id,task_id,role,agent,status,started_at,updated_at,workspace_path)
             SELECT 'previous-nano-run',task_id,role,agent,'exited',started_at,updated_at,workspace_path
             FROM runs WHERE id='nano-e2e-run'",
            [],
        ).unwrap();
        db.execute(
            "UPDATE tasks SET status='awaiting_human', paused_status=?1,
            awaiting_human_status=?1, awaiting_human_by='executor:nano:1',
            claimed_by='executor:nano:1', lease_until='2000-01-01T00:00:00Z',
            human_answer_recorded=1 WHERE id='t-001'",
            [state],
        )
        .unwrap();
        if !human {
            db.execute(
                "UPDATE tasks SET status='consultation', awaiting_human_status=NULL,
                awaiting_human_by=NULL, human_answer_recorded=0 WHERE id='t-001'",
                [],
            )
            .unwrap();
        }
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let settings = fixture.data.join("provider.toml");
    #[cfg(feature = "nano-mcp")]
    let mcp_setting = if external_mcp {
        let peer = fixture.data.join("mcp.toml");
        let python = find_python();
        let script =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/nano_mcp_peer.py");
        private_config(
            &peer,
            &format!(
                "[[servers]]\nid = 'local'\ncommand = {}\nargs = {}\nallow = ['echo']\n",
                toml::Value::String(python.display().to_string()),
                serde_json::to_string(&[script.display().to_string()]).unwrap(),
            ),
        );
        format!(
            "mcp_config_file = {}\n",
            toml::Value::String(peer.display().to_string())
        )
    } else if graph_peer {
        let peer = fixture.data.join("graph-mcp.toml");
        private_config(
            &peer,
            &format!(
                "[[servers]]\nid = 'graph'\ncommand = {}\nargs = ['serve', '--role', 'executor']\nallow = ['repository_graph_status', 'repository_search', 'repository_context']\ninherit_managed_binding = true\ntimeout_ms = 12000\n",
                toml::Value::String(env!("CARGO_BIN_EXE_ferrus").to_string()),
            ),
        );
        format!(
            "mcp_config_file = {}\n",
            toml::Value::String(peer.display().to_string())
        )
    } else {
        String::new()
    };
    #[cfg(not(feature = "nano-mcp"))]
    let mcp_setting = String::new();
    private_config(
        &settings,
        &format!(
            "base_url = 'http://{}/v1'\nmodel = 'configured-model'\ncontext_tokens = 65536\nnative_context_enabled = {native_context}\nworking_set_enabled = {working_set}\n{mcp_setting}",
            listener.local_addr().unwrap(),
        ),
    );
    let server = serve(
        listener,
        resume.map(|(state, _)| (fixture.data.join("ferrus.db"), state)),
        external_mcp,
        graph_peer,
        native_context,
    );
    let mut launch = fixture.command(&settings);
    if external_mcp {
        launch.env("FERRUS_NANO_SECRET", "must-not-reach-peer");
    }
    let mut child = Process::spawn(&mut launch);
    let mut stdin = child.0.stdin.take().unwrap();
    let stdout = child.0.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            tx.send(serde_json::from_str::<Value>(&line.unwrap()).unwrap())
                .unwrap();
        }
    });
    let ready = rx
        .recv_timeout(Duration::from_secs(30))
        .unwrap_or_else(|error| panic!("No ready event: {error}; {}", child.failure_diagnostics()));
    assert_eq!(ready, json!({"version":1,"event":{"type":"ready"}}));
    writeln!(stdin, "{{\"version\":1,\"command\":\"start\"}}").unwrap();
    let mut ended = None;
    let mut last_event = ready;
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if let Ok(event) = rx.recv_timeout(Duration::from_millis(100)) {
            assert_eq!(event["version"], 1);
            last_event = event.clone();
            if event["event"]["type"] == "ended" {
                ended = Some(event);
            }
        }
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success(), "{}", child.stderr());
            break;
        }
    }
    if child.0.try_wait().unwrap().is_none() {
        let diagnostics = child.failure_diagnostics();
        let journal = log_tail(&fixture.data.join("nano/sessions/nano-e2e-run/events.jsonl"));
        panic!(
            "nano did not exit; last event={last_event}\n{diagnostics}\njournal tail={journal:?}"
        );
    }
    reader.join().unwrap();
    for event in rx.try_iter() {
        if event["event"]["type"] == "ended" {
            ended = Some(event);
        }
    }
    let ended = ended.unwrap_or_else(|| {
        panic!(
            "No terminal protocol event; last event={last_event}\n{}",
            child.stderr()
        )
    });
    assert_eq!(
        ended["event"]["reason"]["reason"],
        "submitted",
        "{ended}; journal tail={:?}",
        log_tail(&fixture.data.join("nano/sessions/nano-e2e-run/events.jsonl"))
    );
    assert_eq!(ended["event"]["durable"], true);
    server.join().unwrap();
    let db = Connection::open(fixture.data.join("ferrus.db")).unwrap();
    let status: String = db
        .query_row("SELECT status FROM tasks WHERE id='t-001'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(status, "reviewing");
    let submits: i64 = db
        .query_row(
            "SELECT count(*) FROM events WHERE type='submitted'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(submits, 1);
    assert_eq!(
        fs::read_to_string(fixture.workspace.join("answer.txt")).unwrap(),
        "42\n"
    );
    assert!(!fixture.root.join("answer.txt").exists());
    assert!(
        fs::read_to_string(fixture.root.join(".ferrus/runs/t-001/PATCH.diff"))
            .unwrap()
            .contains("+42")
    );
    let journal =
        fs::read_to_string(fixture.data.join("nano/sessions/nano-e2e-run/events.jsonl")).unwrap();
    let started: Value = serde_json::from_str(journal.lines().next().unwrap()).unwrap();
    assert_eq!(
        started["event"]["launch_evidence"]["baseline_tree"],
        fixture.baseline
    );
    assert_eq!(
        started["event"]["launch_evidence"]["native_context_enabled"],
        native_context
    );
    assert_eq!(
        started["event"]["launch_evidence"]["working_set_enabled"],
        working_set
    );
    assert_eq!(
        started["event"]["launch_evidence"]["graph_peer_mode"],
        if graph_peer { "complete" } else { "absent" }
    );
    let (peer_count, tool_count) = if graph_peer {
        (1, 3)
    } else if external_mcp {
        (1, 1)
    } else {
        (0, 0)
    };
    assert_eq!(
        started["event"]["launch_evidence"]["mcp_peer_count"],
        peer_count
    );
    assert_eq!(
        started["event"]["launch_evidence"]["mcp_tool_count"],
        tool_count
    );
    assert_eq!(
        started["event"]["launch_evidence"]["graph_peer_id"],
        if graph_peer {
            Value::String("graph".into())
        } else {
            Value::Null
        }
    );
    assert_eq!(
        started["event"]["launch_evidence"]["graph_peer_timeout_ms"],
        if graph_peer {
            Value::from(12_000)
        } else {
            Value::Null
        }
    );
    assert_eq!(started["event"]["provider"]["model"], "override-model");
    assert_eq!(journal.contains("context_prepared"), working_set);
    assert!(journal.contains("not-a-json-event"));
    let results: Vec<Value> = journal
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|record| record["event"]["event"] == "tool_result")
        .collect();
    assert_eq!(
        results.len(),
        if graph_peer {
            7
        } else if external_mcp {
            5
        } else {
            4
        }
    );
    if graph_peer {
        assert!(
            results
                .iter()
                .any(|result| result["event"]["outcome"]["content"]["kind"] == "repository_status")
        );
        assert!(
            results
                .iter()
                .any(|result| result["event"]["outcome"]["content"]["kind"] == "repository_search")
        );
        assert!(results.iter().any(|result|
            result["event"]["outcome"]["content"]["kind"] == "repository_context"));
    }
    for result in results {
        assert_eq!(result["event"]["outcome"]["status"], "success", "{result}");
    }
    if external_mcp {
        assert!(journal.contains("from-mcp"));
        assert!(journal.contains("inherited_provider_secret"));
        assert!(journal.contains("null"));
        assert!(!journal.contains("must-not-reach-peer"));
    }
    if resume.is_some() {
        assert!(journal.contains("Use forty-two."));
        assert_eq!(
            db.query_row::<i64, _, _>(
                "SELECT count(*) FROM events WHERE type=?1",
                [restored_event],
                |row| row.get(0),
            )
            .unwrap(),
            1
        );
        let run_dir = fixture.root.join(".ferrus/runs/t-001");
        assert!(
            fs::read_to_string(run_dir.join(request_file))
                .unwrap_or_default()
                .is_empty()
        );
        assert_eq!(
            fs::read_to_string(run_dir.join(response_file)).unwrap(),
            "Use forty-two."
        );
    }
    drop(stdin);
}

#[cfg(feature = "nano-mcp")]
fn find_python() -> PathBuf {
    for directory in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        for executable in ["python3", "python", "python.exe"] {
            let path = directory.join(executable);
            if path.is_file() {
                return path.canonicalize().unwrap();
            }
        }
    }
    panic!("Python 3 is required for the Nano MCP peer integration fixture");
}

#[test]
fn native_registration_validates_before_writes_and_creates_no_loopback_mcp() {
    let f = Fixture::new();
    let before = fs::read(f.root.join("ferrus.toml")).unwrap();
    let settings = f.data.join("provider.toml");
    private_config(
        &settings,
        "base_url = 'http://127.0.0.1:1234/v1'\nmodel = 'local-model'\n",
    );
    let unsupported = ferrus(&f.root)
        .args(["register", "--supervisor", "nano"])
        .env("FERRUS_NANO_CONFIG", &settings)
        .output()
        .unwrap();
    assert!(!unsupported.status.success());
    assert_eq!(fs::read(f.root.join("ferrus.toml")).unwrap(), before);
    let invalid = ferrus(&f.root)
        .args(["register", "--executor", "nano"])
        .env("FERRUS_NANO_CONFIG", f.data.join("missing.toml"))
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    assert_eq!(fs::read(f.root.join("ferrus.toml")).unwrap(), before);
    success(
        ferrus(&f.root)
            .args([
                "register",
                "--executor",
                "nano",
                "--executor-model",
                " override ",
            ])
            .env("FERRUS_NANO_CONFIG", &settings),
    );
    let config: toml::Value =
        toml::from_str(&fs::read_to_string(f.root.join("ferrus.toml")).unwrap()).unwrap();
    assert_eq!(config["hq"]["executor"]["agent"].as_str(), Some("nano"));
    assert_eq!(config["hq"]["executor"]["model"].as_str(), Some("override"));
    success(
        ferrus(&f.root)
            .args(["register", "--executor", "nano"])
            .env("FERRUS_NANO_CONFIG", &settings),
    );
    let config: toml::Value =
        toml::from_str(&fs::read_to_string(f.root.join("ferrus.toml")).unwrap()).unwrap();
    assert_eq!(config["hq"]["executor"]["model"].as_str(), Some(""));
    for path in [".codex", ".claude", ".qwen", ".goose", "opencode.json"] {
        assert!(!f.root.join(path).exists());
    }
    assert!(
        success(ferrus(&f.root).args(["nano", "--version"])).contains(env!("CARGO_PKG_VERSION"))
    );
    assert!(
        !ferrus(&f.root)
            .args(["nano", "run", "--interactive"])
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[cfg(unix)]
#[test]
fn native_registration_provisions_default_local_provider_once() {
    use std::os::unix::fs::PermissionsExt;

    let f = Fixture::new();
    let home = f.data.join("home");
    fs::create_dir(&home).unwrap();
    let settings = home.join(".ferrus/nano.toml");
    let before = fs::read(f.root.join("ferrus.toml")).unwrap();

    let missing_model = ferrus(&f.root)
        .args(["register", "--executor", "nano"])
        .env_remove("FERRUS_NANO_CONFIG")
        .env("HOME", &home)
        .output()
        .unwrap();
    assert!(!missing_model.status.success());
    assert!(String::from_utf8_lossy(&missing_model.stderr).contains("--executor-model"));
    assert_eq!(fs::read(f.root.join("ferrus.toml")).unwrap(), before);
    assert!(!settings.exists());

    let registered = ferrus(&f.root)
        .args([
            "register",
            "--executor",
            "nano",
            "--executor-model",
            "local/model",
        ])
        .env_remove("FERRUS_NANO_CONFIG")
        .env("HOME", &home)
        .output()
        .unwrap();
    assert!(
        registered.status.success(),
        "{}",
        String::from_utf8_lossy(&registered.stderr)
    );
    let original = fs::read(&settings).unwrap();
    assert!(String::from_utf8_lossy(&original).contains("local/model"));
    private::check(&settings, false).unwrap();
    private::check(settings.parent().unwrap(), true).unwrap();
    let config: toml::Value =
        toml::from_str(&fs::read_to_string(f.root.join("ferrus.toml")).unwrap()).unwrap();
    assert_eq!(config["hq"]["executor"]["model"].as_str(), Some(""));

    let changed_settings = "base_url = 'http://127.0.0.1:1234/v1'\nmodel = 'updated/model'\n";
    fs::write(&settings, changed_settings).unwrap();
    success(
        ferrus(&f.root)
            .args(["register", "--executor", "nano"])
            .env_remove("FERRUS_NANO_CONFIG")
            .env("HOME", &home),
    );
    let config: toml::Value =
        toml::from_str(&fs::read_to_string(f.root.join("ferrus.toml")).unwrap()).unwrap();
    assert_eq!(config["hq"]["executor"]["model"].as_str(), Some(""));

    // Existing Ferrus homes may predate Nano and need not have private directory mode.
    fs::set_permissions(
        settings.parent().unwrap(),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    success(
        ferrus(&f.root)
            .args([
                "register",
                "--executor",
                "nano",
                "--executor-model",
                "another-model",
            ])
            .env_remove("FERRUS_NANO_CONFIG")
            .env("HOME", &home),
    );
    let config: toml::Value =
        toml::from_str(&fs::read_to_string(f.root.join("ferrus.toml")).unwrap()).unwrap();
    assert_eq!(
        config["hq"]["executor"]["model"].as_str(),
        Some("another-model")
    );
    success(
        ferrus(&f.root)
            .args(["register", "--executor", "nano"])
            .env_remove("FERRUS_NANO_CONFIG")
            .env("HOME", &home),
    );
    let config: toml::Value =
        toml::from_str(&fs::read_to_string(f.root.join("ferrus.toml")).unwrap()).unwrap();
    assert_eq!(config["hq"]["executor"]["model"].as_str(), Some(""));
    assert_eq!(fs::read_to_string(&settings).unwrap(), changed_settings);
}

#[test]
fn invalid_start_frames_fail_without_claiming_or_creating_a_journal() {
    let f = Fixture::new();
    let settings = f.data.join("provider.toml");
    private_config(
        &settings,
        "base_url = 'http://127.0.0.1:1234/v1'\nmodel = 'local-model'\n",
    );
    for input in [
        b"{\"version\":99,\"command\":\"start\"}\n".to_vec(),
        b"garbage\n".to_vec(),
        vec![b' '; 4097],
    ] {
        let mut child = Process::spawn(&mut f.command(&settings));
        let mut reader = BufReader::new(child.0.stdout.take().unwrap());
        let mut ready = String::new();
        assert!(
            reader.read_line(&mut ready).unwrap() > 0,
            "No ready event: {}",
            child.failure_diagnostics()
        );
        assert_eq!(
            serde_json::from_str::<Value>(&ready).unwrap()["event"]["type"],
            "ready"
        );
        child.0.stdin.as_mut().unwrap().write_all(&input).unwrap();
        child.0.stdin.take();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                assert!(!status.success());
                break;
            }
            assert!(
                Instant::now() < deadline,
                "invalid input did not stop the process: {}",
                child.failure_diagnostics()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut events = String::new();
        reader.read_to_string(&mut events).unwrap();
        assert!(
            events
                .lines()
                .all(|line| serde_json::from_str::<Value>(line).is_ok())
        );
        assert!(!f.data.join("nano/sessions").exists());
        let db = Connection::open(f.data.join("ferrus.db")).unwrap();
        let state: String = db
            .query_row("SELECT status FROM tasks WHERE id='t-001'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(state, "pending");
    }
}
