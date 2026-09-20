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
    io::{BufRead, BufReader, Read, Write},
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
struct Process(Child);
impl Process {
    fn failure_diagnostics(&mut self) -> String {
        let _ = self.0.kill();
        let status = self.0.wait();
        let mut stderr = String::new();
        if let Some(mut pipe) = self.0.stderr.take() {
            let _ = pipe.read_to_string(&mut stderr);
        }
        format!("status={status:?}\n{stderr}")
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
                0 => vec![
                    (
                        "apply_patch",
                        json!({"edits":[{"operation":"create","path":"answer.txt","content":"42\n"}]}),
                    ),
                    (
                        "exec",
                        json!({"command":"echo not-a-json-event", "timeout_ms":1000}),
                    ),
                ],
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
    run_headless_task(None);
}

#[test]
fn relaunched_human_waiter_delivers_the_answer_before_inference_and_submits() {
    for state in ["executing", "addressing"] {
        run_headless_task(Some((state, true)));
    }
}

#[test]
fn relaunched_consultation_delivers_the_response_before_inference_and_submits() {
    for state in ["executing", "addressing"] {
        run_headless_task(Some((state, false)));
    }
}

fn run_headless_task(resume: Option<(&'static str, bool)>) {
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
    private_config(
        &settings,
        &format!(
            "base_url = 'http://{}/v1'\nmodel = 'configured-model'\ncontext_tokens = 65536\n",
            listener.local_addr().unwrap()
        ),
    );
    let server = serve(
        listener,
        resume.map(|(state, _)| (fixture.data.join("ferrus.db"), state)),
    );
    let mut child = Process(fixture.command(&settings).spawn().unwrap());
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
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if let Ok(event) = rx.recv_timeout(Duration::from_millis(100)) {
            assert_eq!(event["version"], 1);
            if event["event"]["type"] == "ended" {
                ended = Some(event);
            }
        }
        if let Some(status) = child.0.try_wait().unwrap() {
            let mut stderr = String::new();
            child
                .0
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut stderr)
                .unwrap();
            assert!(status.success(), "{stderr}");
            break;
        }
    }
    assert!(child.0.try_wait().unwrap().is_some(), "nano did not exit");
    reader.join().unwrap();
    for event in rx.try_iter() {
        if event["event"]["type"] == "ended" {
            ended = Some(event);
        }
    }
    let ended = ended.expect("terminal protocol event");
    assert_eq!(ended["event"]["reason"]["reason"], "submitted");
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
    assert!(journal.contains("not-a-json-event"));
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
        for name in [request_file, response_file] {
            assert!(
                fs::read_to_string(fixture.root.join(".ferrus/runs/t-001").join(name))
                    .unwrap_or_default()
                    .is_empty()
            );
        }
    }
    drop(stdin);
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
        let mut child = Process(f.command(&settings).spawn().unwrap());
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
                "invalid input did not stop the process"
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
