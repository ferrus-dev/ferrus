//! Offline process regressions for unregistered standalone workspaces.
#![cfg(feature = "nano-openai")]

#[path = "../src/nano/private.rs"]
#[allow(dead_code, unused_imports)]
mod private;

use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

struct Provider {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Provider {
    fn new(responses: Vec<Value>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let worker = thread::spawn(move || {
            for response in responses {
                let deadline = Instant::now() + Duration::from_secs(40);
                let stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            thread::sleep(Duration::from_millis(10))
                        }
                        error => panic!("Provider accept: {error:?}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut reader = BufReader::new(stream);
                let mut size = 0;
                loop {
                    let mut line = String::new();
                    assert!(reader.read_line(&mut line).unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        size = value.trim().parse::<usize>().unwrap();
                    }
                }
                assert!(size <= 2 * 1024 * 1024);
                let mut bytes = vec![0; size];
                reader.read_exact(&mut bytes).unwrap();
                captured
                    .lock()
                    .unwrap()
                    .push(serde_json::from_slice(&bytes).unwrap());
                let body = format!("data: {response}\n\ndata: [DONE]\n\n");
                write!(reader.get_mut(), "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
                reader.get_mut().flush().unwrap();
            }
        });
        Self {
            url,
            requests,
            worker: Some(worker),
        }
    }
    fn finish(mut self) -> Vec<Value> {
        self.worker.take().unwrap().join().unwrap();
        self.requests.lock().unwrap().clone()
    }
}

fn answer(text: &str) -> Value {
    json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":3}})
}
fn calls(calls: &[(&str, Value)]) -> Value {
    json!({"choices":[{"index":0,"delta":{"tool_calls":calls.iter().enumerate().map(|(index,(name,args))| json!({"index":index,"id":format!("call-{index}"),"type":"function","function":{"name":name,"arguments":args.to_string()}})).collect::<Vec<_>>()},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":3}})
}

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    store: PathBuf,
    config: PathBuf,
    home: PathBuf,
}
impl Fixture {
    fn standalone_home(&self) -> PathBuf {
        // Windows Known Folders ignores HOME/USERPROFILE overrides. Use the
        // same resolver as production, without changing the production policy.
        #[cfg(windows)]
        let home = dirs::home_dir().unwrap();
        #[cfg(not(windows))]
        let home = self.home.clone();
        home.join(".ferrus/standalone")
    }
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let root = base.join("workspace");
        fs::create_dir(&root).unwrap();
        let config = base.join("nano.toml");
        let home = base.join("home");
        fs::create_dir(&home).unwrap();
        Self {
            _temp: temp,
            root,
            store: base.join("storage"),
            config,
            home,
        }
    }
    fn configure(&self, url: &str, extra: &str) {
        let mut file = private::file(&self.config, true).unwrap();
        writeln!(
            file,
            "base_url = {url:?}\nmodel = 'offline-model'\nreasoning_effort = 'none'\n{extra}"
        )
        .unwrap();
    }
    fn command(&self, id: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ferrus-nano"));
        command
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home);
        command
            .args([
                "--workspace",
                self.root.to_str().unwrap(),
                "--storage",
                self.store.to_str().unwrap(),
                "--config",
                self.config.to_str().unwrap(),
                "--session-id",
                id,
            ])
            .stdin(Stdio::null());
        // Managed launch metadata must not grant or alter standalone authority.
        command
            .env("FERRUS_TASK_ID", "foreign-task")
            .env("FERRUS_RUN_ID", "foreign-run");
        command
    }
    fn records(&self, id: &str) -> Vec<Value> {
        fs::read_to_string(self.store.join(format!("nano/sessions/{id}/events.jsonl")))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    fn assert_unregistered(&self) {
        assert!(!self.home.join(".ferrus/projects").exists());
        assert!(!find_database(&self.home));
        assert!(!self.root.join(".ferrus").exists());
        assert!(!self.root.join("ferrus.toml").exists());
        assert!(!self.store.join("ferrus.db").exists());
        for directory in [&self.root, &self.store] {
            if directory.exists() {
                assert!(!find_database(directory));
            }
        }
    }
}
fn find_database(root: &Path) -> bool {
    fs::read_dir(root).unwrap().any(|entry| {
        let entry = entry.unwrap();
        entry.file_name() == "ferrus.db"
            || (entry.file_type().unwrap().is_dir() && find_database(&entry.path()))
    })
}
fn success(command: &mut Command) -> String {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn standalone_non_git_uses_native_tools_without_orchestration_state() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join("AGENTS.md"),
        "Keep the requested answer file small.\n",
    )
    .unwrap();
    let provider = Provider::new(vec![
        calls(&[
            (
                "apply_patch",
                json!({"edits":[{"operation":"create","path":"answer.txt","content":"standalone\n"}]}),
            ),
            ("read_file", json!({"path":"answer.txt"})),
            (
                "repository_fallback",
                json!({"operation":"search","reason":"disabled","input":{"query":"standalone","paths":["answer.txt"]}}),
            ),
            (
                "exec",
                json!({"command":"echo standalone","cwd":".","timeout_ms":5000}),
            ),
        ]),
        answer("Completed the standalone request."),
    ]);
    fixture.configure(&provider.url, "");
    assert!(
        success(
            fixture
                .command("native-tools")
                .args(["--prompt", "Create answer.txt and inspect it."])
        )
        .contains("Completed the standalone request.")
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("answer.txt")).unwrap(),
        "standalone\n"
    );
    let requests = provider.finish();
    let names: Vec<_> = requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"repository_fallback"));
    for name in [
        "submit",
        "check",
        "consult",
        "ask_human",
        "wait_for_task",
        "repository_search",
    ] {
        assert!(!names.contains(&name));
    }
    assert!(
        requests[0]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("standalone coding assistant")
    );
    assert!(
        requests[0]["messages"][1]["content"]
            .as_str()
            .unwrap()
            .contains("Keep the requested answer file small")
    );
    let records = fixture.records("native-tools");
    assert!(records[0]["event"]["identity"]["task_id"].is_null());
    assert!(records[0]["event"]["identity"]["run_id"].is_null());
    assert_eq!(
        records.last().unwrap()["event"]["reason"]["reason"],
        "model_finished"
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| record["event"]["event"] == "tool_result"
                && record["event"]["outcome"]["status"] == "success")
            .count(),
        4
    );
    fixture.assert_unregistered();
}

#[test]
fn standalone_git_graph_is_explicit_local_and_snapshot_bound_after_workspace_move() {
    let mut fixture = Fixture::new();
    success(
        Command::new("git")
            .arg("init")
            .arg("--object-format=sha1")
            .arg(&fixture.root),
    );
    fs::write(
        fixture.root.join("Cargo.toml"),
        "[package]\nname = 'standalone-fixture'\nversion = '0.1.0'\nedition = '2024'\n[lib]\npath = 'lib.rs'\n",
    ).unwrap();
    fs::write(
        fixture.root.join("lib.rs"),
        "pub fn standalone_symbol() -> bool { true }\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ferrus-nano"))
        .env("HOME", &fixture.home)
        .env("USERPROFILE", &fixture.home)
        .args([
            "--workspace",
            fixture.root.to_str().unwrap(),
            "--storage",
            fixture.store.to_str().unwrap(),
            "--index-graph",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let index: Value = serde_json::from_slice(&output.stdout).unwrap();
    let moved = fixture.root.with_file_name("moved-workspace");
    fs::rename(&fixture.root, &moved).unwrap();
    // The historical pathname may now refer to a different directory.
    fs::create_dir(&fixture.root).unwrap();
    fixture.root = moved.canonicalize().unwrap();
    let provider = Provider::new(vec![
        calls(&[
            ("repository_search", json!({"query":"standalone_symbol"})),
            (
                "repository_context",
                json!({"seeds":[{"type":"path","value":"lib.rs"}],"include_snippets":true}),
            ),
            (
                "repository_context",
                json!({"seeds":[{"type":"path","value":"lib.rs"}],"include_snippets":true,"max_snippet_bytes":1}),
            ),
            (
                "repository_search",
                json!({"query":"standalone-fixture","kinds":["cargo_package"]}),
            ),
            (
                "repository_context",
                json!({"seeds":[{"type":"path","value":"lib.rs"}],"cursor":"obsolete-cursor","include_snippets":true}),
            ),
        ]),
        calls(&[(
            "apply_patch",
            json!({"edits":[{"operation":"create","path":"new.rs","content":"pub fn new_symbol() {}\n"}]}),
        )]),
        answer("Read the explicit standalone index and updated the workspace."),
    ]);
    fixture.configure(&provider.url, "");
    success(fixture.command("graph-query").args([
        "--graph",
        "--prompt",
        "Inspect standalone_symbol.",
    ]));
    let requests = provider.finish();
    let binding: Value =
        serde_json::from_slice(&fs::read(fixture.store.join("workspace.json")).unwrap()).unwrap();
    assert_eq!(
        PathBuf::from(binding["workspace"].as_str().unwrap()),
        fixture.root
    );
    let results: Vec<_> = fixture
        .records("graph-query")
        .into_iter()
        .filter(|record| record["event"]["event"] == "tool_result")
        .collect();
    assert_eq!(results.len(), 6);
    for result in &results {
        assert_eq!(result["event"]["outcome"]["status"], "success");
    }
    let context = &results[1]["event"]["outcome"]["content"]["result"]["Ok"];
    assert_eq!(context["snapshot_id"], index["snapshot"]["id"]);
    assert!(context["task_view"].is_null());
    assert!(!context["data"]["snippets"].as_array().unwrap().is_empty());
    let bounded = &results[2]["event"]["outcome"]["content"]["result"]["Ok"];
    assert!(
        bounded["diagnostics"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "content.snippets_truncated")
    );
    let snippet_bytes: usize = bounded["data"]["snippets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|snippet| snippet["text"].as_str().unwrap().len())
        .sum();
    assert!(snippet_bytes <= 1);
    let package = &results[3]["event"]["outcome"]["content"]["result"]["Ok"];
    assert!(
        package["data"]["hits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["kind"] == "cargo_package")
    );
    let error = &results[4]["event"]["outcome"]["content"]["result"]["Err"];
    assert_eq!(error["code"], "stale_cursor");
    assert_eq!(error["retryable"], false);
    assert!(error["message"].as_str().unwrap().contains("snapshot"));
    let tool_contents = |request: &Value| {
        request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "tool")
            .map(|message| {
                serde_json::from_str::<Value>(message["content"].as_str().unwrap()).unwrap()
            })
            .collect::<Vec<_>>()
    };
    let selected = tool_contents(&requests[1]);
    assert_eq!(selected.len(), 5);
    for (selected, recorded) in selected.iter().zip(&results) {
        assert_eq!(selected, &recorded["event"]["outcome"]);
    }
    let invalidated = tool_contents(&requests[2]);
    for result in &invalidated[..4] {
        assert_eq!(result["content"]["kind"], "evidence_unavailable");
        assert_eq!(result["content"]["reason"], "publication_changed");
    }
    fixture.assert_unregistered();
}

#[test]
fn resume_preserves_history_and_budget_after_workspace_move_without_replaying_effects() {
    let mut fixture = Fixture::new();
    let provider = Provider::new(vec![
        answer("The first request is complete."),
        answer("The second request is complete."),
    ]);
    fixture.configure(&provider.url, "");
    success(
        fixture
            .command("first")
            .args(["--prompt", "Remember the completed first request."]),
    );
    let previous_path = fixture.store.join("nano/sessions/first/events.jsonl");
    let previous = fs::read(&previous_path).unwrap();
    let previous_binding: Value =
        serde_json::from_slice(&fs::read(fixture.store.join("workspace.json")).unwrap()).unwrap();
    let moved = fixture.root.with_file_name("moved-workspace");
    fs::rename(&fixture.root, &moved).unwrap();
    assert!(!fixture.root.exists());
    fixture.root = moved.canonicalize().unwrap();
    success(fixture.command("second").args([
        "--resume",
        "first",
        "--prompt",
        "Continue with the second request.",
    ]));
    assert_eq!(fs::read(&previous_path).unwrap(), previous);
    let binding: Value =
        serde_json::from_slice(&fs::read(fixture.store.join("workspace.json")).unwrap()).unwrap();
    assert_eq!(binding["workspace_id"], previous_binding["workspace_id"]);
    assert_eq!(
        PathBuf::from(binding["workspace"].as_str().unwrap()),
        fixture.root
    );
    let requests = provider.finish();
    assert!(
        requests[1]["messages"][1]["content"]
            .as_str()
            .unwrap()
            .contains("The first request is complete.")
    );
    let records = fixture.records("second");
    assert_eq!(records[0]["event"]["inherited_budget"]["model_turns"], 1);
    assert_eq!(records.last().unwrap()["budget"]["model_turns"], 2);
    assert_eq!(
        records.last().unwrap()["budget"]["reported_input_tokens"],
        20
    );
    fixture.assert_unregistered();
}

#[test]
fn resume_refuses_an_interrupted_effect_before_contacting_provider() {
    let fixture = Fixture::new();
    let provider = Provider::new(vec![
        calls(&[(
            "apply_patch",
            json!({"edits":[{"operation":"create","path":"answer.txt","content":"once\n"}]}),
        )]),
        answer("Done."),
    ]);
    fixture.configure(&provider.url, "");
    success(
        fixture
            .command("interrupted")
            .args(["--prompt", "Create answer.txt once."]),
    );
    provider.finish();
    let records = fixture.records("interrupted");
    let prefix: Vec<_> = records
        .iter()
        .take_while(|record| record["event"]["event"] != "tool_result")
        .map(|record| record.to_string())
        .collect();
    fs::write(
        fixture.store.join("nano/sessions/interrupted/events.jsonl"),
        prefix.join("\n") + "\n",
    )
    .unwrap();
    let output = fixture
        .command("refused")
        .args(["--resume", "interrupted", "--prompt", "Continue safely."])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("manual reconciliation"));
    assert!(!fixture.store.join("nano/sessions/refused").exists());
    assert_eq!(
        fs::read_to_string(fixture.root.join("answer.txt")).unwrap(),
        "once\n"
    );
    fixture.assert_unregistered();
}

#[test]
fn storage_binding_rejects_another_workspace_and_paths_inside_workspace() {
    let fixture = Fixture::new();
    let provider = Provider::new(vec![answer("Done.")]);
    fixture.configure(&provider.url, "");
    success(fixture.command("owner").args(["--prompt", "Finish."]));
    provider.finish();
    let other = fixture.root.parent().unwrap().join("another");
    fs::create_dir(&other).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ferrus-nano"))
        .env("HOME", &fixture.home)
        .env("USERPROFILE", &fixture.home)
        .args([
            "--workspace",
            other.to_str().unwrap(),
            "--storage",
            fixture.store.to_str().unwrap(),
            "--prompt",
            "Must not run.",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("another workspace"));
    let output = Command::new(env!("CARGO_BIN_EXE_ferrus-nano"))
        .env("HOME", &fixture.home)
        .env("USERPROFILE", &fixture.home)
        .args([
            "--workspace",
            fixture.root.to_str().unwrap(),
            "--storage",
            fixture.root.join("storage").to_str().unwrap(),
            "--prompt",
            "Must not run.",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!fixture.root.join("storage").exists());
}

#[cfg(feature = "nano-mcp")]
#[test]
fn standalone_uses_external_mcp_but_skips_managed_bindings() {
    let fixture = Fixture::new();
    let peers = fixture.root.parent().unwrap().join("mcp.toml");
    let python = ["python3", "python"]
        .into_iter()
        .find(|python| {
            Command::new(python)
                .arg("--version")
                .output()
                .is_ok_and(|output| output.status.success())
        })
        .expect("Python 3 is required for the MCP fixture");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/nano_mcp_peer.py");
    let executable = if cfg!(windows) {
        success(Command::new(python).args(["-c", "import sys; print(sys.executable)"]))
    } else {
        success(Command::new("which").arg(python))
    };
    let mut file = private::file(&peers, true).unwrap();
    writeln!(file, "[[servers]]\nid = 'external'\ncommand = {}\nargs = [{}]\nallow = ['echo']\n\n[[servers]]\nid = 'graph'\ncommand = {}\nargs = []\nallow = ['repository_graph_status']\ninherit_managed_binding = true", json!(executable.trim()), json!(script.to_str().unwrap()), json!(env!("CARGO_BIN_EXE_ferrus"))).unwrap();
    let provider = Provider::new(vec![
        calls(&[("mcp_external_echo", json!({"text":"standalone peer"}))]),
        answer("External peer completed."),
    ]);
    fixture.configure(
        &provider.url,
        &format!("mcp_config_file = {}", json!(peers)),
    );
    success(
        fixture
            .command("mcp-peer")
            .args(["--prompt", "Use the external echo peer."]),
    );
    let requests = provider.finish();
    assert!(!requests[0]["tools"].to_string().contains("mcp_graph_"));
    let records = fixture.records("mcp-peer");
    assert!(
        records
            .iter()
            .any(|record| record["event"]["event"] == "tool_result"
                && record["event"]["outcome"]["status"] == "success")
    );
    fixture.assert_unregistered();
}

#[test]
fn memory_context_is_explicit_read_only_and_independent_of_registration() {
    use ferrus::project_memory::{
        domain::*, index::*, policy::MemoryPolicy, source::LocalMemorySource, sqlite::MemorySidecar,
    };
    use ferrus::repository_graph::domain::RepoPath;
    let fixture = Fixture::new();
    success(
        Command::new("git")
            .arg("init")
            .arg("--object-format=sha1")
            .arg(&fixture.root),
    );
    fs::create_dir_all(fixture.root.join("docs/specs")).unwrap();
    fs::write(fixture.root.join("docs/specs/standalone.md"), "# Standalone memory\n\n- [ ] #1.0 Standalone memory milestone\n\nID: standalone-memory\nDepends on: none\n").unwrap();
    success(Command::new("git").current_dir(&fixture.root).args([
        "add",
        "--",
        "docs/specs/standalone.md",
    ]));
    let memory_data = fixture.root.parent().unwrap().join("memory");
    fs::create_dir(&memory_data).unwrap();
    let project = ProjectRef {
        namespace: ProjectNamespace::new("local:standalone-test").unwrap(),
        project_id: ProjectId::new("memory-fixture").unwrap(),
    };
    let source = LocalMemorySource::discover_at(
        fixture.root.clone(),
        memory_data.clone(),
        project,
        RepoPath::new("docs/specs").unwrap(),
        MemoryPolicy::default(),
    )
    .unwrap();
    let mut sidecar = MemorySidecar::open_at(&memory_data).unwrap();
    let outcome = MemoryIndexer::new(&source, &mut sidecar)
        .unwrap()
        .index(MemoryIndexOptions::default())
        .unwrap();
    drop(sidecar);
    let path = memory_data.join("project-memory.db");
    let before = fs::read(&path).unwrap();
    success(
        Command::new(env!("CARGO_BIN_EXE_ferrus-nano"))
            .env("HOME", &fixture.home)
            .env("USERPROFILE", &fixture.home)
            .args([
                "--workspace",
                fixture.root.to_str().unwrap(),
                "--storage",
                fixture.store.to_str().unwrap(),
                "--index-graph",
            ]),
    );
    let provider = Provider::new(vec![
        calls(&[
            ("project_memory_status", json!({})),
            (
                "project_context_search",
                json!({"domain":"memory","query":"Standalone memory"}),
            ),
            (
                "project_context_search",
                json!({"domain":"memory","query":"Standalone memory","kinds":["milestone"]}),
            ),
            (
                "project_context_search",
                json!({"domain":"all","query":"Standalone memory","kinds":["milestone"]}),
            ),
        ]),
        answer("Read structural memory."),
    ]);
    fixture.configure(&provider.url, "");
    success(fixture.command("memory-query").args([
        "--graph",
        "--memory-sidecar",
        path.to_str().unwrap(),
        "--memory-namespace",
        "local:standalone-test",
        "--memory-project",
        "memory-fixture",
        "--prompt",
        "Inspect explicit project memory.",
    ]));
    let requests = provider.finish();
    assert_eq!(before, fs::read(&path).unwrap());
    let results: Vec<_> = fixture
        .records("memory-query")
        .into_iter()
        .filter(|record| record["event"]["event"] == "tool_result")
        .collect();
    assert_eq!(
        results[0]["event"]["outcome"]["content"]["result"]["revision_id"],
        json!(outcome.revision.id)
    );
    assert_eq!(results[1]["event"]["outcome"]["status"], "success");
    assert!(results[1]["event"]["outcome"]["content"]["result"]["Ok"].is_object());
    let unfiltered = results[1]["event"]["outcome"]["content"]["result"]["Ok"]["results"]
        .as_array()
        .unwrap();
    assert!(
        unfiltered
            .iter()
            .any(|hit| hit["result"]["entity"]["data"]["type"] == "specification")
    );
    assert!(
        unfiltered
            .iter()
            .any(|hit| hit["result"]["entity"]["data"]["type"] == "milestone")
    );
    for filtered in &results[2..] {
        let hits = filtered["event"]["outcome"]["content"]["result"]["Ok"]["results"]
            .as_array()
            .unwrap();
        let memory: Vec<_> = hits
            .iter()
            .filter(|hit| hit["domain"] == "memory")
            .collect();
        assert!(!memory.is_empty());
        assert!(
            memory
                .iter()
                .all(|hit| hit["result"]["entity"]["data"]["type"] == "milestone")
        );
    }
    let selected: Vec<_> = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .map(|message| serde_json::from_str::<Value>(message["content"].as_str().unwrap()).unwrap())
        .collect();
    assert_eq!(selected.len(), results.len());
    for (selected, recorded) in selected.iter().zip(&results) {
        assert_eq!(selected, &recorded["event"]["outcome"]);
    }
    assert!(!find_database(&memory_data));
    fixture.assert_unregistered();
}

#[test]
fn repository_domain_context_does_not_require_a_memory_publication() {
    use ferrus::project_memory::sqlite::MemorySidecar;
    let fixture = Fixture::new();
    fs::write(fixture.root.join("lib.rs"), "pub fn current_symbol() {}\n").unwrap();
    let memory_data = fixture.root.parent().unwrap().join("memory");
    fs::create_dir(&memory_data).unwrap();
    drop(MemorySidecar::open_at(&memory_data).unwrap());
    let memory_path = memory_data.join("project-memory.db");
    let before = fs::read(&memory_path).unwrap();
    success(
        Command::new(env!("CARGO_BIN_EXE_ferrus-nano"))
            .env("HOME", &fixture.home)
            .env("USERPROFILE", &fixture.home)
            .args([
                "--workspace",
                fixture.root.to_str().unwrap(),
                "--storage",
                fixture.store.to_str().unwrap(),
                "--index-graph",
            ]),
    );
    let graph_before = fs::read(fixture.store.join("repo-graph.db")).unwrap();
    let provider = Provider::new(vec![
        calls(&[
            ("project_memory_status", json!({})),
            (
                "project_context_search",
                json!({"domain":"repository","query":"lib.rs"}),
            ),
            (
                "project_context",
                json!({"domain":"repository","seeds":[{"type":"path","value":"lib.rs"}]}),
            ),
            (
                "project_context_search",
                json!({"domain":"memory","query":"lib.rs"}),
            ),
            (
                "project_context",
                json!({"domain":"all","seeds":[{"type":"path","value":"lib.rs"}]}),
            ),
        ]),
        answer("Read repository context without a memory publication."),
    ]);
    fixture.configure(&provider.url, "");
    success(fixture.command("repository-only").args([
        "--graph",
        "--memory-sidecar",
        memory_path.to_str().unwrap(),
        "--memory-namespace",
        "local:standalone-test",
        "--memory-project",
        "empty-memory",
        "--prompt",
        "Inspect repository context.",
    ]));
    provider.finish();
    let results: Vec<_> = fixture
        .records("repository-only")
        .into_iter()
        .filter(|record| record["event"]["event"] == "tool_result")
        .collect();
    assert_eq!(results.len(), 5);
    let status = &results[0]["event"]["outcome"]["content"]["result"];
    assert!(status["revision_id"].is_null());
    assert_eq!(status["data"]["availability"], "not_built");
    for (result, field) in results[1..3].iter().zip(["results", "items"]) {
        assert_eq!(result["event"]["outcome"]["status"], "success");
        let response = &result["event"]["outcome"]["content"]["result"]["Ok"];
        assert!(!response[field].as_array().unwrap().is_empty());
        assert!(response["repository"]["snapshot_id"].is_string());
        assert!(response["memory"].is_null());
    }
    for result in &results[3..] {
        assert_eq!(result["event"]["outcome"]["status"], "failed");
    }
    assert_eq!(before, fs::read(&memory_path).unwrap());
    assert_eq!(
        graph_before,
        fs::read(fixture.store.join("repo-graph.db")).unwrap()
    );
    fixture.assert_unregistered();
}

#[test]
fn incompatible_graph_status_is_structured_and_does_not_migrate_the_store() {
    use ferrus::repository_graph::sqlite::SIDECAR_SCHEMA_VERSION;
    for (version, action) in [(3, "index"), (SIDECAR_SCHEMA_VERSION + 1, "rebuild")] {
        let fixture = Fixture::new();
        private::directory(&fixture.store, true).unwrap();
        let path = fixture.store.join("repo-graph.db");
        let database = rusqlite::Connection::open(&path).unwrap();
        database
            .pragma_update(None, "application_id", 0x4652_4731u32)
            .unwrap();
        database
            .pragma_update(None, "user_version", version)
            .unwrap();
        database
            .execute_batch("CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY)")
            .unwrap();
        database
            .execute("INSERT INTO schema_migrations VALUES (?1)", [version])
            .unwrap();
        drop(database);
        let before = fs::read(&path).unwrap();
        let provider = Provider::new(vec![
            calls(&[
                ("repository_graph_status", json!({})),
                ("repository_search", json!({"query":"lib.rs"})),
            ]),
            answer("The graph requires maintenance."),
        ]);
        fixture.configure(&provider.url, "");
        success(fixture.command("incompatible").args([
            "--graph",
            "--prompt",
            "Inspect graph availability.",
        ]));
        let requests = provider.finish();
        let results: Vec<_> = fixture
            .records("incompatible")
            .into_iter()
            .filter(|record| record["event"]["event"] == "tool_result")
            .collect();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["event"]["outcome"]["status"], "success");
        let status = &results[0]["event"]["outcome"]["content"]["result"];
        assert_eq!(status["data"]["availability"], "incompatible");
        assert_eq!(status["data"]["recommended_action"], action);
        assert!(status["snapshot_id"].is_null());
        assert!(status["source_revision"].is_null());
        assert_eq!(status["data"]["fallback"], "direct_source_inspection");
        let projected: Value = serde_json::from_str(
            requests[1]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|message| message["role"] == "tool")
                .unwrap()["content"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(projected, results[0]["event"]["outcome"]);
        assert_eq!(results[1]["event"]["outcome"]["status"], "failed");
        assert_eq!(before, fs::read(&path).unwrap());
        fixture.assert_unregistered();
    }
}

#[test]
fn graph_status_without_an_index_reports_availability_and_preserves_fallback() {
    let fixture = Fixture::new();
    fs::write(fixture.root.join("lib.rs"), "pub fn current_symbol() {}\n").unwrap();
    let provider = Provider::new(vec![
        calls(&[
            ("repository_graph_status", json!({})),
            (
                "repository_fallback",
                json!({"operation":"search","reason":"missing","input":{"query":"current_symbol"}}),
            ),
        ]),
        answer("Used current workspace evidence."),
    ]);
    fixture.configure(&provider.url, "");
    success(fixture.command("missing-graph").args([
        "--graph",
        "--prompt",
        "Check graph availability and inspect current_symbol.",
    ]));
    provider.finish();
    let results: Vec<_> = fixture
        .records("missing-graph")
        .into_iter()
        .filter(|r| r["event"]["event"] == "tool_result")
        .collect();
    assert!(
        results
            .iter()
            .all(|r| r["event"]["outcome"]["status"] == "success")
    );
    let status = &results[0]["event"]["outcome"]["content"]["result"];
    assert_eq!(status["data"]["availability"], "not_built");
    assert_eq!(status["data"]["recommended_action"], "index");
    assert!(status["snapshot_id"].is_null());
    assert!(!fixture.store.join("repo-graph.db").exists());
    fixture.assert_unregistered();
}

#[test]
fn a_mutation_keeps_the_standalone_graph_stale_until_explicit_reindex() {
    let fixture = Fixture::new();
    fs::write(fixture.root.join("lib.rs"), "pub fn original_symbol() {}\n").unwrap();
    let index = || {
        success(
            Command::new(env!("CARGO_BIN_EXE_ferrus-nano"))
                .env("HOME", &fixture.home)
                .env("USERPROFILE", &fixture.home)
                .args([
                    "--workspace",
                    fixture.root.to_str().unwrap(),
                    "--storage",
                    fixture.store.to_str().unwrap(),
                    "--index-graph",
                ]),
        );
    };
    index();
    let provider = Provider::new(vec![
        calls(&[(
            "apply_patch",
            json!({"edits":[{"operation":"create","path":"created.rs","content":"pub fn newly_created() {}\n"}]}),
        )]),
        calls(&[
            ("repository_graph_status", json!({})),
            ("repository_search", json!({"query":"newly_created"})),
            (
                "repository_fallback",
                json!({"operation":"search","reason":"stale","input":{"query":"newly_created"}}),
            ),
        ]),
        answer("Used current workspace evidence."),
    ]);
    fixture.configure(&provider.url, "");
    success(fixture.command("graph-stale").args([
        "--graph",
        "--prompt",
        "Create a symbol and inspect it.",
    ]));
    provider.finish();
    let results: Vec<_> = fixture
        .records("graph-stale")
        .into_iter()
        .filter(|record| record["event"]["event"] == "tool_result")
        .collect();
    assert_eq!(
        results[1]["event"]["outcome"]["content"]["result"]["freshness"]["freshness"],
        "stale"
    );
    assert_eq!(results[2]["event"]["outcome"]["status"], "failed");
    assert_eq!(results[3]["event"]["outcome"]["status"], "success");
    assert!(fixture.store.join("graph-invalidated").exists());
    index();
    assert!(!fixture.store.join("graph-invalidated").exists());
    fixture.assert_unregistered();
}

#[test]
fn stdin_requests_use_default_private_storage_without_creating_project_state() {
    let fixture = Fixture::new();
    // The existing shared Ferrus home may be a normal, non-private directory.
    fs::create_dir(fixture.home.join(".ferrus")).unwrap();
    let provider = Provider::new(vec![answer("Read the stdin request.")]);
    fixture.configure(&provider.url, "");
    let mut child = Command::new(env!("CARGO_BIN_EXE_ferrus-nano"))
        .env("HOME", &fixture.home)
        .env("USERPROFILE", &fixture.home)
        .args([
            "--workspace",
            fixture.root.to_str().unwrap(),
            "--config",
            fixture.config.to_str().unwrap(),
            "--session-id",
            "stdin-request",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"Read a bounded request on stdin.\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Read the stdin request."));
    let requests = provider.finish();
    assert!(
        requests[0]["messages"][1]["content"]
            .as_str()
            .unwrap()
            .contains("bounded request on stdin")
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    let journal = PathBuf::from(
        stderr
            .lines()
            .find_map(|line| line.strip_prefix("Nano journal: "))
            .unwrap(),
    );
    assert!(journal.starts_with(fixture.standalone_home().canonicalize().unwrap()));
    let default_store = journal.ancestors().nth(3).unwrap();
    let binding: Value =
        serde_json::from_slice(&fs::read(default_store.join("workspace.json")).unwrap()).unwrap();
    assert_eq!(
        PathBuf::from(binding["workspace"].as_str().unwrap()),
        fixture.root
    );
    assert!(!find_database(default_store));
    fixture.assert_unregistered();
    // Only remove this test's workspace-bound storage, never the shared home.
    fs::remove_dir_all(default_store).unwrap();
}

#[test]
fn workspace_lock_applies_across_storage_paths_and_releases_on_setup_failure() {
    use fs2::FileExt;
    let fixture = Fixture::new();
    // Invalid provider setup still establishes the workspace binding, then
    // releases both locks without creating a session or contacting a model.
    let failed = fixture
        .command("setup-failure")
        .args(["--prompt", "Inspect"])
        .output()
        .unwrap();
    assert!(!failed.status.success());
    let binding: Value =
        serde_json::from_slice(&fs::read(fixture.store.join("workspace.json")).unwrap()).unwrap();
    let lock_path = fixture.standalone_home().join("locks").join(format!(
        "{}.lock",
        binding["workspace_id"].as_str().unwrap()
    ));
    let lock = private::file(&lock_path, false).unwrap();
    lock.try_lock_exclusive().unwrap();
    let other = fixture._temp.path().join("other-storage");
    let blocked = Command::new(env!("CARGO_BIN_EXE_ferrus-nano"))
        .env("HOME", &fixture.home)
        .env("USERPROFILE", &fixture.home)
        .args([
            "--workspace",
            fixture.root.to_str().unwrap(),
            "--storage",
            other.to_str().unwrap(),
            "--config",
            fixture.config.to_str().unwrap(),
            "--prompt",
            "Inspect",
        ])
        .output()
        .unwrap();
    assert!(!blocked.status.success());
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("already has a writer"));
    assert!(!other.exists());
    FileExt::unlock(&lock).unwrap();
    let provider = Provider::new(vec![answer("The released workspace is available.")]);
    fixture.configure(&provider.url, "");
    success(
        fixture
            .command("after-failure")
            .args(["--prompt", "Inspect"]),
    );
    provider.finish();
    fixture.assert_unregistered();
}
