//! Tests for checks.

use super::*;

#[test]
fn taskless_interactive_process_waits_edits_checks_and_preserves_pending_tasks() {
    let fixture = Fixture::new();
    let mut config_text = fs::read_to_string(fixture.root.join("ferrus.toml")).unwrap();
    config_text.push_str("\n[repository_graph]\nenabled = true\n");
    fs::write(fixture.root.join("ferrus.toml"), config_text).unwrap();
    success(ferrus(&fixture.root).args(["graph", "index"]));
    let db = Connection::open(fixture.data.join("ferrus.db")).unwrap();
    db.execute("INSERT OR IGNORE INTO tasks(id,path,status) VALUES ('current','.ferrus/TASK.md','unknown')", []).unwrap();
    db.execute("INSERT INTO runs(id,task_id,role,agent,status,started_at,updated_at,workspace_path) VALUES ('nano-direct-run','current','executor','executor:nano:1','running','2026-10-05T00:00:00Z','2026-10-05T00:00:00Z',?1)", [fixture.root.to_str().unwrap()]).unwrap();
    drop(db);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let (requests_tx, requests_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        for turn in 0..2 {
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "Missing taskless request");
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
            let names: Vec<_> = request["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["function"]["name"].as_str().unwrap())
                .collect();
            for forbidden in [
                "submit",
                "consult",
                "ask_human",
                "wait_for_task",
                "approve",
                "reject",
                "enqueue_task",
            ] {
                assert!(!names.contains(&forbidden));
            }

            assert!(
                request["messages"][0]["content"]
                    .as_str()
                    .unwrap()
                    .contains("There is no managed task")
            );
            let data = if turn == 0 {
                assert_eq!(
                    request["messages"].as_array().unwrap().last().unwrap(),
                    &json!({"role":"user", "content":"Create answer.txt with 42 and check it."})
                );
                let calls: Vec<_> = [
                    ("repository_graph_status", json!({})),
                    ("apply_patch", json!({"edits":[{"operation":"create","path":"answer.txt","content":"42\n"}]})),
                    ("check", json!({})),
                ].into_iter().enumerate().map(|(index, (name,args))|
                    json!({"index":index,"id":format!("call-{index}"),"type":"function","function":{"name":name,"arguments":args.to_string()}})).collect();
                json!({"choices":[{"index":0,"delta":{"tool_calls":calls},"finish_reason":"tool_calls"}]})
            } else {
                let messages = request["messages"].as_array().unwrap();
                let result = |id| {
                    let message = messages
                        .iter()
                        .find(|message| message["tool_call_id"] == id)
                        .unwrap();
                    serde_json::from_str::<Value>(message["content"].as_str().unwrap()).unwrap()
                };
                let graph = result("call-0");
                assert_eq!(graph["status"], "success", "{graph}");
                assert!(
                    graph["content"]["result"]["snapshot_id"].is_string(),
                    "{graph}"
                );
                assert!(graph["content"]["result"]["task_view"].is_null(), "{graph}");
                assert_eq!(result("call-1")["status"], "success");
                assert_eq!(result("call-2")["content"]["passed"], true);
                json!({"choices":[{"index":0,"delta":{"content":"Created and checked answer.txt."},"finish_reason":"stop"}]})
            };
            requests_tx.send(turn).unwrap();
            if turn == 1 {
                // A provider can produce no streaming events while thinking.
                // Keep this gap above the old five-second per-event timeout.
                std::thread::sleep(Duration::from_secs(6));
            }

            let sse = format!("data: {data}\n\ndata: [DONE]\n\n");
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}", sse.len()).unwrap();
        }
    });
    let config = fixture.root.join("nano.toml");
    let mut provider_config = format!("base_url = {url:?}\nmodel = 'fixture-model'\n");
    if cfg!(feature = "nano-mcp") {
        let peers = fixture.root.join("mcp.toml");
        let executable = toml::Value::String(env!("CARGO_BIN_EXE_ferrus").into());
        private_config(
            &peers,
            &format!(
                "[[servers]]\nid = 'graph'\ncommand = {executable}\nargs = ['serve', '--role', 'executor']\nallow = ['repository_search']\ninherit_managed_binding = true\n"
            ),
        );
        provider_config.push_str(&format!(
            "mcp_config_file = {}\n",
            toml::Value::String(peers.to_string_lossy().into_owned())
        ));
    }

    private_config(&config, &provider_config);
    let mut command = fixture.command(&config);
    command
        .current_dir(&fixture.root)
        .args(["--interactive", "--taskless"])
        .env_remove("FERRUS_TASK_ID")
        .env_remove("FERRUS_BASELINE_TREE")
        .env("RUST_LOG", "ferrus=debug")
        .env("FERRUS_RUN_ID", "nano-direct-run");
    // Linux also exercises the direct launch with a Windows-sized main-thread
    // stack instead of relying on the host's larger default stack.
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: the closure only calls OS resource-limit functions and
        // constructs an error from errno; it does not allocate or acquire locks.
        unsafe {
            command.pre_exec(|| {
                let mut limit = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                if libc::getrlimit(libc::RLIMIT_STACK, &mut limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }

                limit.rlim_cur = 1024 * 1024;
                if libc::setrlimit(libc::RLIMIT_STACK, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }

                Ok(())
            });
        }
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
    let journal_path = fixture
        .data
        .join("nano/sessions/nano-direct-run/events.jsonl");
    let wait_for_input = |child: &mut Process, stage: &str| {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let event = receiver
                // Graph refresh, checks, or a non-streaming model response may
                // legitimately be silent for more than five seconds. Apply the
                // overall deadline instead of an accidental per-event deadline.
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| {
                    panic!(
                        "{stage}: {error}; {}\njournal tail:\n{}",
                        child.failure_diagnostics(),
                        log_tail(&journal_path).unwrap_or_else(|error| error.to_string())
                    )
                });
            assert_ne!(event["event"]["type"], "ended", "{event}");
            if event["event"]["phase"] == "input_requested" {
                break;
            }

            assert!(Instant::now() < deadline, "{}", child.failure_diagnostics());
        }
    };
    wait_for_input(&mut child, "initial input request");
    assert!(
        requests_rx.try_recv().is_err(),
        "Opening /executor must not call the provider"
    );
    writeln!(
        stdin,
        "{}",
        json!({"version":1,"command":{"steer":{"text":"Create answer.txt with 42 and check it.","input_id":"direct-input-1"}}})
    )
    .unwrap();
    wait_for_input(&mut child, "response after editing and checks");
    assert_eq!(requests_rx.recv_timeout(Duration::from_secs(5)).unwrap(), 0);
    assert_eq!(requests_rx.recv_timeout(Duration::from_secs(5)).unwrap(), 1);
    assert_eq!(
        fs::read_to_string(fixture.root.join("answer.txt")).unwrap(),
        "42\n"
    );
    assert!(!fixture.workspace.join("answer.txt").exists());
    writeln!(stdin, "{}", json!({"version":1,"command":"cancel"})).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
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
    let journal = fs::read_to_string(
        fixture
            .data
            .join("nano/sessions/nano-direct-run/events.jsonl"),
    )
    .unwrap();
    let records: Vec<Value> = journal
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(records[0]["event"]["identity"]["task_id"].is_null());
    assert!(
        records
            .iter()
            .any(|record| record["event"]["event"] == "user_input"
                && record["event"]["input_id"] == "direct-input-1")
    );
    let db = Connection::open(fixture.data.join("ferrus.db")).unwrap();
    let task: (String, Option<String>, u32, u32) = db.query_row(
        "SELECT status, claimed_by, check_retries, executor_dispatches FROM tasks WHERE id = 't-001'", [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    ).unwrap();
    assert_eq!(task, ("pending".into(), None, 0, 0));
}
