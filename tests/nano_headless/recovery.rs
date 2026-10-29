//! Tests for recovery.

use super::*;

#[test]
fn headless_configured_token_budget_is_journaled_and_reported_before_inference() {
    let fixture = Fixture::new();
    let config = fixture.root.join("nano.toml");
    private_config(
        &config,
        "base_url = 'http://127.0.0.1:1234/v1'\nmodel = 'fixture-model'\nsession_tokens = 1\n",
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
    let ready = rx.recv_timeout(Duration::from_secs(30)).unwrap();
    assert_eq!(ready["event"]["type"], "ready");
    writeln!(stdin, "{{\"version\":1,\"command\":\"start\"}}").unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(!status.success());
            break;
        }

        assert!(Instant::now() < deadline, "{}", child.failure_diagnostics());
        std::thread::sleep(Duration::from_millis(10));
    }

    output.join().unwrap();
    let events: Vec<_> = rx.try_iter().collect();
    assert!(events.iter().any(|event| event["event"]["type"] == "ended"
        && event["event"]["reason"] == json!({"reason":"limit","detail":"tokens"})
        && event["event"]["durable"] == true));
    assert!(child.stderr().contains("0 consumed, 1 limit"));
    let journal =
        fs::read_to_string(fixture.data.join("nano/sessions/nano-e2e-run/events.jsonl")).unwrap();
    let records: Vec<Value> = journal
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records[0]["event"]["limits"]["tokens"], 1);
    assert!(
        !records
            .iter()
            .any(|record| record["event"]["event"] == "model_started")
    );
    let state: (String, String) = Connection::open(fixture.data.join("ferrus.db"))
        .unwrap()
        .query_row(
            "SELECT status,failure_reason FROM tasks WHERE id='t-001'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, ("failed".into(), "nano_limit_tokens".into()));
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
