//! Managed headless task process scenarios.

use super::*;

pub(super) fn run_rejected_provider(
    status_code: u16,
    attempts: usize,
    reason: &str,
    failure_code: &str,
) {
    let fixture = Fixture::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        for _ in 0..attempts {
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
            write!(stream, "HTTP/1.1 {status_code} Rejected\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
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
    assert_eq!(ended["event"]["reason"]["reason"], reason);
    if reason == "limit" {
        assert_eq!(ended["event"]["reason"]["detail"], "retries");
    }

    assert_eq!(ended["event"]["durable"], true);
    assert!(!events.iter().any(|event| event["event"]["type"] == "error"));
    let stderr = child.stderr();
    assert!(
        stderr.contains(if status_code == 400 {
            "Unsupported"
        } else {
            "Transport"
        }),
        "{stderr}"
    );
    assert!(stderr.contains(&status_code.to_string()), "{stderr}");
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
        json!({"source":"http","status":status_code})
    );
    assert_eq!(failed["event"]["retryable"], attempts > 1);
    assert_eq!(
        records
            .iter()
            .filter(|record| record["event"]["event"] == "model_started")
            .count(),
        attempts
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
    assert_eq!(state, ("failed".into(), failure_code.into(), None, 0));
    assert_eq!(
        fs::read_to_string(fixture.root.join(".ferrus/tasks/t-001.md")).unwrap(),
        "Create answer.txt, check and submit."
    );
}

pub(super) fn run_headless_task(
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
