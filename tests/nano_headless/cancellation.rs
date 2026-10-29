//! Tests for cancellation.

use super::*;

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
