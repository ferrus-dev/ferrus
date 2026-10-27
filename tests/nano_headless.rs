//! Real process/HTTP/SQLite regression for the opt-in native Executor frontend.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
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
        let before_stop = self.0.try_wait();
        let _ = self.0.kill();
        let status = self.0.wait();
        format!(
            "status_before_test_cleanup={before_stop:?}; status_after_test_cleanup={status:?}\nstderr tail:\n{}",
            self.stderr()
        )
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
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

fn run_interactive_task(interactive: bool) {
    let fixture = Fixture::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        for turn in 0..2 {
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "Missing interactive request");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("{error}"),
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
            let request: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(request["model"], "override-model");
            let names: Vec<_> = request["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["function"]["name"].as_str().unwrap())
                .collect();
            for forbidden in ["approve", "reject", "archive", "create_task"] {
                assert!(!names.contains(&forbidden));
            }

            let data = if turn == 0 {
                json!({"choices":[{"index":0,"delta":{"content":"Need direction."},"finish_reason":"stop"}]})
            } else {
                assert_eq!(
                    request["messages"].as_array().unwrap().last().unwrap(),
                    &json!({"role":"user", "content":"Create answer.txt with 42, check and submit."})
                );
                let calls: Vec<_> = [
                    ("apply_patch", json!({"edits":[{"operation":"create","path":"answer.txt","content":"42\n"}]})),
                    ("check", json!({})),
                    ("submit", json!({"content":"Created answer.txt and checked it."})),
                ].into_iter().enumerate().map(|(index,(name,args))| json!({"index":index,"id":format!("call-{index}"),"type":"function","function":{"name":name,"arguments":args.to_string()}})).collect();
                json!({"choices":[{"index":0,"delta":{"tool_calls":calls},"finish_reason":"tool_calls"}]})
            };
            let sse = format!("data: {data}\n\ndata: [DONE]\n\n");
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}", sse.len()).unwrap();
        }
    });
    let config = fixture.root.join("nano.toml");
    private_config(
        &config,
        &format!("base_url = {url:?}\nmodel = 'fixture-model'\n"),
    );
    let mut command = fixture.command(&config);
    if interactive {
        command.arg("--interactive");
    }

    let mut child = Process::spawn(&mut command);
    let mut stdin = child.0.stdin.take().unwrap();
    let stdout = child.0.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    let output = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            sender
                .send(serde_json::from_str::<Value>(&line.unwrap()).unwrap())
                .unwrap();
        }
    });
    assert_eq!(
        receiver.recv_timeout(Duration::from_secs(30)).unwrap()["event"]["type"],
        "ready"
    );
    writeln!(stdin, "{}", json!({"version":1,"command":"start"})).unwrap();
    if !interactive {
        writeln!(stdin, "{}", json!({"version":1,"command":"interact"})).unwrap();
    }

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let event = receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|error| panic!("{error}; {}", child.failure_diagnostics()));
        assert_ne!(
            event["event"]["type"], "ended",
            "Ended before input: {event}"
        );
        if event["event"]["phase"] == "input_requested" {
            break;
        }

        assert!(Instant::now() < deadline, "{}", child.failure_diagnostics());
    }

    assert!(child.0.try_wait().unwrap().is_none());
    writeln!(stdin, "{}", json!({"version":1,"command":{"steer":{"text":"Create answer.txt with 42, check and submit."}}})).unwrap();
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success(), "{}", child.stderr());
            break;
        }

        assert!(Instant::now() < deadline, "{}", child.failure_diagnostics());
        std::thread::sleep(Duration::from_millis(10));
    }

    output.join().unwrap();
    server.join().unwrap();
    assert!(
        receiver
            .try_iter()
            .any(|event| event["event"]["type"] == "ended"
                && event["event"]["reason"]["reason"] == "submitted")
    );
    let journal =
        fs::read_to_string(fixture.data.join("nano/sessions/nano-e2e-run/events.jsonl")).unwrap();
    let records: Vec<Value> = journal
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        records
            .iter()
            .any(|record| record["event"]["event"] == "interaction_opened")
    );
    assert!(
        records
            .iter()
            .any(|record| record["event"]["event"] == "user_input")
    );
    assert_eq!(
        fs::read_to_string(fixture.workspace.join("answer.txt")).unwrap(),
        "42\n"
    );
    let status: String = Connection::open(fixture.data.join("ferrus.db"))
        .unwrap()
        .query_row("SELECT status FROM tasks WHERE id='t-001'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(status, "reviewing");
}

#[path = "nano_headless/cancellation.rs"]
mod cancellation_tests;
#[path = "nano_headless/checks.rs"]
mod checks_tests;
#[path = "nano_headless/provider.rs"]
mod provider_tests;
#[path = "nano_headless/recovery.rs"]
mod recovery_tests;
#[path = "nano_headless/review.rs"]
mod review_tests;
#[path = "nano_headless/tasks.rs"]
mod tasks;
use tasks::*;

#[test]
fn headless_retry_exhaustion_fails_the_task_instead_of_allowing_hq_redispatch() {
    run_rejected_provider(503, 4, "limit", "nano_limit_retries");
}

#[test]
fn headless_native_context_and_working_set_can_be_disabled_separately() {
    run_headless_task(None, false, false, false, true);
    run_headless_task(None, false, false, true, false);
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

#[test]
fn headless_process_edits_checks_and_submits_an_isolated_task() {
    run_headless_task(None, false, false, true, true);
}

#[test]
fn interactive_process_attaches_to_headless_turn_then_steers_and_submits() {
    run_interactive_task(false);
}

#[test]
fn interactive_process_launch_waits_for_input_then_steers_and_submits() {
    run_interactive_task(true);
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

#[test]
fn native_registration_validates_before_writes_and_creates_no_loopback_mcp() {
    let f = Fixture::new();
    let before = fs::read(f.root.join("ferrus.toml")).unwrap();
    let settings = f.data.join("provider.toml");
    private_config(
        &settings,
        "base_url = 'http://127.0.0.1:1234/v1'\nmodel = 'local-model'\n",
    );
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
    success(
        ferrus(&f.root)
            .args([
                "register",
                "--supervisor",
                "nano",
                "--supervisor-model",
                "supervisor-model",
            ])
            .env("FERRUS_NANO_CONFIG", &settings),
    );
    let config: toml::Value =
        toml::from_str(&fs::read_to_string(f.root.join("ferrus.toml")).unwrap()).unwrap();
    assert_eq!(config["hq"]["supervisor"]["agent"].as_str(), Some("nano"));
    assert_eq!(
        config["hq"]["supervisor"]["model"].as_str(),
        Some("supervisor-model")
    );
    success(
        ferrus(&f.root)
            .args(["register", "--supervisor", "nano"])
            .env("FERRUS_NANO_CONFIG", &settings),
    );
    let updated: toml::Value =
        toml::from_str(&fs::read_to_string(f.root.join("ferrus.toml")).unwrap()).unwrap();
    assert_eq!(updated["hq"]["supervisor"]["model"].as_str(), Some(""));
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
