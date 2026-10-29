//! Tests for review.

use super::*;

#[test]
fn headless_http_rejection_fails_the_task_and_preserves_safe_diagnostics() {
    run_rejected_provider(400, 1, "provider_protocol", "nano_provider_protocol");
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

#[test]
fn taskless_interactive_rejects_a_managed_run_binding_before_inference() {
    let fixture = Fixture::new();
    let config = fixture.root.join("nano.toml");
    private_config(
        &config,
        "base_url = 'http://127.0.0.1:1234/v1'\nmodel = 'fixture-model'\n",
    );
    let mut command = fixture.command(&config);
    command
        .current_dir(&fixture.root)
        .args(["--interactive", "--taskless"])
        .env_remove("FERRUS_TASK_ID")
        .env_remove("FERRUS_BASELINE_TREE");
    let mut child = Process::spawn(&mut command);
    let mut stdin = child.0.stdin.take().unwrap();
    writeln!(stdin, "{}", json!({"version":1,"command":"start"})).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(!status.success());
            assert!(
                child.stderr().contains("Interactive run binding mismatch"),
                "{}",
                child.stderr()
            );
            break;
        }

        assert!(Instant::now() < deadline, "{}", child.failure_diagnostics());
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(!fixture.data.join("nano/sessions").exists());
    let state: String = Connection::open(fixture.data.join("ferrus.db"))
        .unwrap()
        .query_row("SELECT status FROM tasks WHERE id='t-001'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(state, "pending");
}

#[test]
fn headless_native_reviewer_uses_the_managed_protocol_and_stops_after_approval() {
    let f = Fixture::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let settings = f.data.join("review-provider.toml");
    private_config(
        &settings,
        &format!("base_url='{endpoint}'\nmodel='review-model'\n"),
    );
    let db = Connection::open(f.data.join("ferrus.db")).unwrap();
    db.execute("UPDATE tasks SET status='reviewing',baseline_snapshot_id='snapshot:fixture',repository_view_snapshot_id='snapshot:fixture',repository_view_status='unavailable',repository_view_lifecycle='frozen_submitted',repository_view_tree_algorithm='git-sha1',repository_view_tree_digest=?1 WHERE id='t-001'",[&f.baseline]).unwrap();
    db.execute("INSERT INTO runs(id,task_id,role,agent,status,started_at,updated_at,workspace_path,baseline_snapshot_id,repository_view_snapshot_id,repository_view_status,repository_view_lifecycle,repository_view_tree_algorithm,repository_view_tree_digest) VALUES('native-review','t-001','supervisor','supervisor:nano:t-001','running','2026-10-17T00:00:00Z','2026-10-17T00:00:00Z',?1,'snapshot:fixture','snapshot:fixture','unavailable','frozen_submitted','git-sha1',?2)",rusqlite::params![f.root.to_str().unwrap(),f.baseline]).unwrap();
    let server = std::thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        for name in ["review_pending", "approve"] {
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    Err(error) => panic!("review mock: {error}"),
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
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }

                if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse::<usize>().unwrap();
                }
            }

            assert!((1..512 * 1024).contains(&length));
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let request: Value = serde_json::from_slice(&body).unwrap();
            let names: Vec<_> = request["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["function"]["name"].as_str().unwrap())
                .collect();
            for denied in [
                "exec",
                "apply_patch",
                "submit",
                "enqueue_task",
                "create_spec",
                "archive_spec",
                "respond_consult",
            ] {
                assert!(!names.contains(&denied));
            }

            assert!(names.contains(&"approve"));
            let data = json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":name,"type":"function","function":{"name":name,"arguments":"{}"}}]},"finish_reason":"tool_calls"}]});
            let body = format!("data: {data}\n\ndata: [DONE]\n\n");
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        }
    });
    let mut command = ferrus(&f.root);
    command
        .args(["--debug", "nano", "run", "--supervise", "--config"])
        .arg(&settings)
        .env("FERRUS_PROJECT_ROOT", &f.root)
        .env("FERRUS_AGENT_ID", "supervisor:nano:t-001")
        .env("FERRUS_TASK_ID", "t-001")
        .env("FERRUS_RUN_ID", "native-review")
        .env("RUST_LOG", "ferrus=debug")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let mut child = Process::spawn(&mut command);
    let mut input = child.0.stdin.take().unwrap();
    let mut reader = BufReader::new(child.0.stdout.take().unwrap());
    let mut ready = String::new();
    reader.read_line(&mut ready).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&ready).unwrap()["event"]["type"],
        "ready"
    );
    writeln!(input, "{{\"version\":1,\"command\":\"start\"}}").unwrap();
    let (tx, rx) = mpsc::channel();
    let output = std::thread::spawn(move || {
        for line in reader.lines() {
            tx.send(serde_json::from_str::<Value>(&line.unwrap()).unwrap())
                .unwrap();
        }
    });
    let mut events = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        events.extend(rx.try_iter());
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success(), "{}", child.stderr());
            break;
        }

        assert!(
            Instant::now() < deadline,
            "{}\nevents: {events:?}\njournal: {:?}",
            child.failure_diagnostics(),
            log_tail(&f.data.join("nano/sessions/native-review/events.jsonl"))
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    output.join().unwrap();
    events.extend(rx.try_iter());
    server.join().unwrap();
    assert!(events.iter().any(|frame| frame["event"]["type"] == "ended"
        && frame["event"]["reason"]["reason"] == "review_completed"
        && frame["event"]["durable"] == true));
    assert_eq!(
        db.query_row("SELECT status FROM tasks WHERE id='t-001'", [], |row| {
            row.get::<_, String>(0)
        })
        .unwrap(),
        "complete"
    );
}
