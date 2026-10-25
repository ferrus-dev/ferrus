use super::*;
use crate::nano::{
    provider::ProviderSettings,
    session::{Budget, EndReason, LaunchEvidence, Limits, SessionIdentity},
};
use std::{fs, process::Command};

#[test]
fn graph_calls_use_the_discovered_peer_id() {
    for tool in [
        "repository_graph_status",
        "repository_search",
        "repository_context",
    ] {
        assert!(is_graph_call(
            &format!("mcp_repo_{tool}"),
            "{}",
            Some("repo")
        ));
        assert!(!is_graph_call(
            &format!("mcp_graph_{tool}"),
            "{}",
            Some("repo")
        ));
    }

    assert!(!is_graph_call(
        "mcp_repo_repository_unrelated",
        "{}",
        Some("repo")
    ));
    assert!(!is_graph_call("mcp_repo_repository_search", "{}", None));
}

fn write_records(path: &Path, records: &[Record]) -> Result<()> {
    let mut file = File::create(path)?;
    for record in records {
        serde_json::to_writer(&mut file, record)?;
        writeln!(file)?;
    }

    Ok(())
}

fn set_settings_digest(records: &mut [Record]) -> Result<String> {
    let SessionEvent::Started {
        provider: Some(provider),
        limits,
        launch_evidence: Some(evidence),
        ..
    } = &mut records[0].event
    else {
        anyhow::bail!("Expected a Nano start with launch evidence");
    };
    let digest = effective_settings_sha256(provider, limits, evidence)?;
    evidence.settings_sha256 = Some(digest.clone());
    Ok(digest)
}

fn workload_launch(case_id: &str, cache: CacheState) -> Result<serde_json::Value> {
    let suite = suite()?;
    let workload = &suite.workloads[case_id];
    Ok(serde_json::to_value(LaunchWorkloadEvidence {
        case_id: case_id.to_owned(),
        task_sha256: task_sha256(workload),
        check_commands_sha256: crate::checks::commands_sha256(&workload.checks)?,
        graph_snapshot_id: matches!(&cache, CacheState::Warm).then(|| "snapshot".into()),
        cache: Some(cache),
    })?)
}

fn submission_payload(task: &str, gate: &str, cycles: u32, case_id: &str) -> Result<String> {
    let suite = suite()?;
    let checks = crate::checks::commands_sha256(&suite.workloads[case_id].checks)?;
    Ok(serde_json::json!({"task_id": task, "check_gate": gate, "review_cycles": cycles, "check_commands_sha256": checks}).to_string())
}

fn accepted_external_database(path: &Path, task: &str, run: &str, tree: &str) -> Result<()> {
    let db = Connection::open(path)?;
    db.execute_batch(
        "CREATE TABLE tasks(id TEXT, status TEXT, review_cycles INTEGER);
         CREATE TABLE runs(id TEXT, task_id TEXT, status TEXT, role TEXT);
         CREATE TABLE events(id INTEGER PRIMARY KEY, type TEXT, run_id TEXT, payload_json TEXT);",
    )?;
    db.execute("INSERT INTO tasks VALUES (?1, 'complete', 0)", [task])?;
    db.execute(
        "INSERT INTO runs VALUES (?1, ?2, 'completed', 'executor')",
        params![run, task],
    )?;
    db.execute(
        "INSERT INTO events(type, run_id, payload_json) VALUES ('run_started', ?1, ?2)",
        params![run, serde_json::json!({"baseline_tree": tree, "evaluation": workload_launch("local_bug_fix", CacheState::Cold)?}).to_string()],
    )?;
    db.execute(
        "INSERT INTO events(type, run_id, payload_json) VALUES ('submission_committed', ?1, ?2)",
        params![run, submission_payload(task, "passed", 0, "local_bug_fix")?],
    )?;
    Ok(())
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for item in fs::read_dir(from)? {
        let item = item?;
        let source = item.path();
        let target = to.join(item.file_name());
        if item.file_type()?.is_dir() {
            copy_tree(&source, &target)?;
        } else {
            fs::copy(source, target)?;
        }
    }

    Ok(())
}

#[test]
fn fixture_git_trees_match_the_pinned_suite() -> Result<()> {
    let suite = suite()?;
    let task_table = include_str!("../../tests/fixtures/nano_eval/tasks.md");
    for (case, workload) in &suite.workloads {
        let row = task_table
            .lines()
            .find(|line| line.starts_with(&format!("| `{case}` |")))
            .context("Missing task table row")?;
        let cells: Vec<_> = row.split('|').map(str::trim).collect();
        assert_eq!(cells[2], workload.task);
        assert!(cells[3].contains(&format!("`{}`", workload.checks[0])));
        if let Some(windows) = &workload.windows_checks {
            assert!(cells[3].contains(&format!("`{}`", windows[0])));
        }
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/nano_eval/cases");
    for (case, expected) in suite.cases {
        let temp = tempfile::tempdir()?;
        copy_tree(&root.join(case), temp.path())?;
        ensure!(
            Command::new("git")
                .args(["init", "-q", "--object-format=sha1"])
                .env("GIT_DEFAULT_HASH", "sha256")
                .current_dir(temp.path())
                .status()?
                .success()
        );
        ensure!(
            Command::new("git")
                .args(["config", "core.autocrlf", "false"])
                .current_dir(temp.path())
                .status()?
                .success()
        );
        for args in [&["add", "-A"][..]] {
            ensure!(
                Command::new("git")
                    .args(args)
                    .current_dir(temp.path())
                    .status()?
                    .success()
            );
        }

        let output = Command::new("git")
            .arg("write-tree")
            .env("GIT_DEFAULT_HASH", "sha256")
            .current_dir(temp.path())
            .output()?;
        ensure!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout)?.trim(), expected);
    }

    Ok(())
}

#[test]
fn cache_label_requires_a_consistent_prelaunch_graph_state() {
    assert_eq!(
        launch_cache(false, CanonicalGraphStatus::Fresh, true),
        Some(CacheState::Disabled)
    );
    assert_eq!(
        launch_cache(true, CanonicalGraphStatus::Unknown, false),
        Some(CacheState::Cold)
    );
    assert_eq!(
        launch_cache(true, CanonicalGraphStatus::Fresh, true),
        Some(CacheState::Warm)
    );
    assert_eq!(launch_cache(true, CanonicalGraphStatus::Stale, true), None);
    assert_eq!(
        launch_cache(true, CanonicalGraphStatus::Unknown, true),
        None
    );
    assert_eq!(launch_cache(true, CanonicalGraphStatus::Fresh, false), None);
}

#[test]
fn submission_alone_is_not_an_accepted_evaluation() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let db_path = temp.path().join("ferrus.db");
    let db = Connection::open(&db_path)?;
    db.execute_batch(
        r#"CREATE TABLE tasks(id TEXT, status TEXT, review_cycles INTEGER);
        CREATE TABLE runs(id TEXT, task_id TEXT, status TEXT, role TEXT);
        CREATE TABLE events(id INTEGER PRIMARY KEY, type TEXT, run_id TEXT, payload_json TEXT);
        INSERT INTO tasks VALUES ('task', 'reviewing', 0);
        INSERT INTO runs VALUES ('run', 'task', 'completed', 'executor');
        INSERT INTO events(type, run_id, payload_json) VALUES ('run_started', 'run', '{}');
        INSERT INTO events(type, run_id, payload_json) VALUES ('check_passed', 'run', '{}');
        INSERT INTO events(type, run_id, payload_json) VALUES ('submission_committed', 'run', '{"task_id":"task","check_gate":"passed","review_cycles":0}');
        INSERT INTO events(type, run_id, payload_json) VALUES ('submitted', 'run', '{"check_gate":"passed"}');"#,
    )?;
    let identity = SessionIdentity {
        session_id: "run".into(),
        project_id: "project".into(),
        task_id: Some("task".into()),
        run_id: Some("run".into()),
    };
    let journal = temp.path().join("events.jsonl");
    let start_tree = suite()?.cases["local_bug_fix"].clone();
    db.execute(
        "UPDATE events SET payload_json = ?1 WHERE type = 'run_started'",
        [serde_json::json!({"baseline_tree": start_tree, "evaluation": workload_launch("local_bug_fix", CacheState::Cold)?}).to_string()],
    )?;
    db.execute(
        "UPDATE events SET payload_json = ?1 WHERE type = 'submission_committed'",
        [submission_payload("task", "passed", 0, "local_bug_fix")?],
    )?;
    let mut records = [
        Record {
            version: 1,
            session_id: "run".into(),
            sequence: 1,
            budget: Budget::default(),
            event: SessionEvent::Started {
                identity,
                limits: Limits::default(),
                input: "task".into(),
                system_prompt: None,
                launch_evidence: Some(Box::new(LaunchEvidence {
                    baseline_tree: start_tree.clone(),
                    native_context_enabled: true,
                    working_set_enabled: false,
                    graph_peer_mode: Some(GraphPeerMode::Absent),
                    graph_peer_id: None,
                    mcp_peer_count: Some(0),
                    mcp_tool_count: Some(0),
                    graph_peer_timeout_ms: None,
                    settings_sha256: None,
                })),
                inherited_budget: None,
                provider: Some(Box::new(ProviderSettings {
                    api: "openai_chat_completions_v1".into(),
                    base_url: "http://127.0.0.1:1234/v1".into(),
                    model: "mock-model".into(),
                    context_tokens: Some(32768),
                    max_output_tokens: Some(4096),
                    temperature: Some(0.0),
                    reasoning_effort: None,
                    request_timeout_ms: 120000,
                    wire_bytes: 4 * 1024 * 1024,
                    event_bytes: 256 * 1024,
                    max_tool_calls: 64,
                    include_usage: true,
                })),
            },
        },
        Record {
            version: 1,
            session_id: "run".into(),
            sequence: 2,
            budget: Budget::default(),
            event: SessionEvent::Ended {
                reason: EndReason::Cancelled,
            },
        },
    ];
    let nano_digest = set_settings_digest(&mut records)?;
    write_records(&journal, &records)?;
    let attempt = Attempt {
        case_id: "local_bug_fix".into(),
        variant: Variant::NanoNative,
        cache: CacheState::Cold,
        sample: 1,
        start_tree: suite()?.cases["local_bug_fix"].clone(),
        model: "mock-model".into(),
        settings_sha256: nano_digest.clone(),
        native_context_enabled: Some(true),
        working_set_enabled: Some(false),
        database: db_path,
        task_id: "task".into(),
        run_id: "run".into(),
        journal: Some(journal.clone()),
        external_usage: None,
        timing: Timing {
            source: "fixture".into(),
            total_ms: 10,
            index_ms: None,
            refresh_ms: None,
            peak_rss_bytes: None,
        },
        harness_notes: vec![],
    };
    let baseline_attempt = attempt.clone();
    let report = build(Manifest {
        version: 1,
        attempts: vec![attempt],
    })?;
    assert!(!report.attempts[0].accepted);
    assert_eq!(report.groups[0].samples, 1);
    let cold_launch = workload_launch("local_bug_fix", CacheState::Cold)?;
    for field in ["task_sha256", "check_commands_sha256"] {
        let mut wrong = cold_launch.clone();
        wrong[field] = serde_json::json!("0".repeat(64));
        db.execute(
            "UPDATE events SET payload_json = ?1 WHERE type = 'run_started'",
            [serde_json::json!({"baseline_tree": start_tree, "evaluation": wrong}).to_string()],
        )?;
        assert!(
            build(Manifest {
                version: 1,
                attempts: vec![baseline_attempt.clone()]
            })
            .is_err()
        );
    }

    db.execute(
        "UPDATE events SET payload_json = ?1 WHERE type = 'run_started'",
        [serde_json::json!({"baseline_tree": start_tree, "evaluation": workload_launch("local_bug_fix", CacheState::Warm)?}).to_string()],
    )?;
    assert!(
        build(Manifest {
            version: 1,
            attempts: vec![baseline_attempt.clone()]
        })
        .is_err()
    );
    let mut warm_attempt = baseline_attempt.clone();
    warm_attempt.cache = CacheState::Warm;
    assert!(
        build(Manifest {
            version: 1,
            attempts: vec![warm_attempt]
        })
        .is_ok()
    );
    db.execute(
        "UPDATE events SET payload_json = ?1 WHERE type = 'run_started'",
        [serde_json::json!({"baseline_tree": start_tree, "evaluation": cold_launch}).to_string()],
    )?;
    db.execute(
        "UPDATE events SET payload_json = ?1 WHERE type = 'submission_committed'",
        [serde_json::json!({"task_id":"task", "check_gate":"passed", "review_cycles":0, "check_commands_sha256":"0".repeat(64)}).to_string()],
    )?;
    assert!(
        build(Manifest {
            version: 1,
            attempts: vec![baseline_attempt.clone()]
        })
        .is_err()
    );
    db.execute(
        "UPDATE events SET payload_json = ?1 WHERE type = 'submission_committed'",
        [submission_payload("task", "passed", 0, "local_bug_fix")?],
    )?;
    let mut wrong_settings = baseline_attempt.clone();
    wrong_settings.settings_sha256 = "0".repeat(64);
    assert!(
        build(Manifest {
            version: 1,
            attempts: vec![wrong_settings]
        })
        .is_err()
    );
    if let SessionEvent::Started {
        provider: Some(provider),
        ..
    } = &mut records[0].event
    {
        provider.temperature = Some(0.5);
    }

    write_records(&journal, &records)?;
    assert!(
        build(Manifest {
            version: 1,
            attempts: vec![baseline_attempt.clone()]
        })
        .is_err()
    );
    if let SessionEvent::Started {
        provider: Some(provider),
        ..
    } = &mut records[0].event
    {
        provider.temperature = Some(0.0);
    }

    write_records(&journal, &records)?;
    let mut wrong_model = baseline_attempt.clone();
    wrong_model.model = "other-model".into();
    assert!(
        build(Manifest {
            version: 1,
            attempts: vec![wrong_model]
        })
        .is_err()
    );
    let mut mcp_attempt = baseline_attempt.clone();
    mcp_attempt.variant = Variant::NanoMcp;
    mcp_attempt.native_context_enabled = Some(false);
    if let SessionEvent::Started {
        launch_evidence: Some(evidence),
        ..
    } = &mut records[0].event
    {
        evidence.native_context_enabled = false;
    }

    mcp_attempt.settings_sha256 = set_settings_digest(&mut records)?;
    write_records(&journal, &records)?;
    assert!(
        build(Manifest {
            version: 1,
            attempts: vec![mcp_attempt.clone()]
        })
        .is_err()
    );
    if let SessionEvent::Started {
        launch_evidence: Some(evidence),
        ..
    } = &mut records[0].event
    {
        evidence.graph_peer_mode = Some(GraphPeerMode::Complete);
    }

    mcp_attempt.settings_sha256 = set_settings_digest(&mut records)?;
    write_records(&journal, &records)?;
    assert!(
        build(Manifest {
            version: 1,
            attempts: vec![mcp_attempt.clone()]
        })
        .is_err()
    );
    if let SessionEvent::Started {
        launch_evidence: Some(evidence),
        ..
    } = &mut records[0].event
    {
        evidence.graph_peer_id = Some("repo".into());
        evidence.mcp_peer_count = Some(2);
        evidence.mcp_tool_count = Some(4);
    }

    mcp_attempt.settings_sha256 = set_settings_digest(&mut records)?;
    write_records(&journal, &records)?;
    assert!(
        build(Manifest {
            version: 1,
            attempts: vec![mcp_attempt.clone()]
        })
        .is_err()
    );
    if let SessionEvent::Started {
        launch_evidence: Some(evidence),
        ..
    } = &mut records[0].event
    {
        evidence.mcp_peer_count = Some(1);
        evidence.mcp_tool_count = Some(3);
    }

    mcp_attempt.settings_sha256 = set_settings_digest(&mut records)?;
    write_records(&journal, &records)?;
    assert!(
        build(Manifest {
            version: 1,
            attempts: vec![mcp_attempt.clone()]
        })
        .is_err()
    );
    if let SessionEvent::Started {
        launch_evidence: Some(evidence),
        ..
    } = &mut records[0].event
    {
        evidence.graph_peer_timeout_ms = Some(10_000);
    }

    mcp_attempt.settings_sha256 = set_settings_digest(&mut records)?;
    write_records(&journal, &records)?;
    assert!(
        build(Manifest {
            version: 1,
            attempts: vec![mcp_attempt.clone()]
        })
        .is_ok()
    );
    let first_digest = mcp_attempt.settings_sha256.clone();
    if let SessionEvent::Started {
        launch_evidence: Some(evidence),
        ..
    } = &mut records[0].event
    {
        evidence.graph_peer_timeout_ms = Some(11_000);
    }

    let second_digest = set_settings_digest(&mut records)?;
    assert_ne!(first_digest, second_digest);
    write_records(&journal, &records)?;
    assert!(
        build(Manifest {
            version: 1,
            attempts: vec![mcp_attempt.clone()]
        })
        .is_err()
    );
    mcp_attempt.settings_sha256 = second_digest;
    assert!(
        build(Manifest {
            version: 1,
            attempts: vec![mcp_attempt.clone()]
        })
        .is_ok()
    );
    let mut disabled_attempt = baseline_attempt.clone();
    disabled_attempt.variant = Variant::NanoGraphDisabled;
    disabled_attempt.cache = CacheState::Disabled;
    disabled_attempt.native_context_enabled = Some(false);
    disabled_attempt.settings_sha256 = mcp_attempt.settings_sha256.clone();
    assert!(
        build(Manifest {
            version: 1,
            attempts: vec![disabled_attempt.clone()]
        })
        .is_err()
    );
    if let SessionEvent::Started {
        launch_evidence: Some(evidence),
        ..
    } = &mut records[0].event
    {
        evidence.graph_peer_mode = Some(GraphPeerMode::Absent);
        evidence.graph_peer_id = None;
        evidence.graph_peer_timeout_ms = None;
        evidence.mcp_peer_count = Some(1);
        evidence.mcp_tool_count = Some(1);
    }

    disabled_attempt.settings_sha256 = set_settings_digest(&mut records)?;
    write_records(&journal, &records)?;
    db.execute(
        "UPDATE events SET payload_json = ?1 WHERE type = 'run_started'",
        [serde_json::json!({"baseline_tree": start_tree, "evaluation": workload_launch("local_bug_fix", CacheState::Disabled)?}).to_string()],
    )?;
    assert!(
        build(Manifest {
            version: 1,
            attempts: vec![disabled_attempt.clone()]
        })
        .is_err()
    );
    if let SessionEvent::Started {
        launch_evidence: Some(evidence),
        ..
    } = &mut records[0].event
    {
        evidence.mcp_peer_count = Some(0);
        evidence.mcp_tool_count = Some(0);
    }

    disabled_attempt.settings_sha256 = set_settings_digest(&mut records)?;
    write_records(&journal, &records)?;
    assert!(
        build(Manifest {
            version: 1,
            attempts: vec![disabled_attempt]
        })
        .is_ok()
    );
    db.execute(
        "UPDATE events SET payload_json = ?1 WHERE type = 'run_started'",
        [serde_json::json!({"baseline_tree": start_tree, "evaluation": workload_launch("local_bug_fix", CacheState::Cold)?}).to_string()],
    )?;
    if let SessionEvent::Started {
        launch_evidence: Some(evidence),
        ..
    } = &mut records[0].event
    {
        evidence.native_context_enabled = true;
    }

    assert_eq!(set_settings_digest(&mut records)?, nano_digest);
    write_records(&journal, &records)?;
    let mut wrong_flags = baseline_attempt.clone();
    wrong_flags.variant = Variant::NanoMcp;
    wrong_flags.native_context_enabled = Some(false);
    assert!(
        build(Manifest {
            version: 1,
            attempts: vec![wrong_flags]
        })
        .is_err()
    );
    let wrong_tree = baseline_attempt.clone();
    if let SessionEvent::Started {
        launch_evidence: Some(evidence),
        ..
    } = &mut records[0].event
    {
        evidence.baseline_tree = "0".repeat(40);
    }

    let mut file = File::create(&journal)?;
    for record in &records {
        serde_json::to_writer(&mut file, record)?;
        writeln!(file)?;
    }

    drop(file);
    assert!(
        build(Manifest {
            version: 1,
            attempts: vec![wrong_tree.clone()]
        })
        .is_err()
    );
    if let SessionEvent::Started {
        launch_evidence: Some(evidence),
        ..
    } = &mut records[0].event
    {
        evidence.baseline_tree = start_tree.clone();
    }

    let mut file = File::create(&journal)?;
    for record in &records {
        serde_json::to_writer(&mut file, record)?;
        writeln!(file)?;
    }

    drop(file);
    db.execute("UPDATE tasks SET status='complete'", [])?;
    db.execute("DELETE FROM events WHERE type = 'submitted'", [])?;
    let attempt = Attempt {
        case_id: "local_bug_fix".into(),
        variant: Variant::ExternalMcp,
        cache: CacheState::Cold,
        sample: 1,
        start_tree: suite()?.cases["local_bug_fix"].clone(),
        model: "mock-model".into(),
        settings_sha256: "0".repeat(64),
        native_context_enabled: None,
        working_set_enabled: None,
        database: temp.path().join("ferrus.db"),
        task_id: "task".into(),
        run_id: "run".into(),
        journal: None,
        external_usage: None,
        timing: Timing {
            source: "fixture".into(),
            total_ms: 10,
            index_ms: None,
            refresh_ms: None,
            peak_rss_bytes: None,
        },
        harness_notes: vec![],
    };
    let accepted_attempt = attempt.clone();
    let mut other_model = attempt.clone();
    other_model.model = "other-model".into();
    let mut other_settings = attempt.clone();
    other_settings.settings_sha256 = "1".repeat(64);
    db.execute_batch(
        "INSERT INTO tasks VALUES ('task-2', 'complete', 0);
         INSERT INTO runs VALUES ('run-2', 'task-2', 'completed', 'executor');
         INSERT INTO tasks VALUES ('task-3', 'complete', 0);
         INSERT INTO runs VALUES ('run-3', 'task-3', 'completed', 'executor');",
    )?;
    for (task_id, run_id) in [("task-2", "run-2"), ("task-3", "run-3")] {
        db.execute(
            "INSERT INTO events(type, run_id, payload_json) VALUES ('run_started', ?1, ?2)",
            params![
                run_id,
                serde_json::json!({"baseline_tree": start_tree, "evaluation": workload_launch("local_bug_fix", CacheState::Cold)?}).to_string()
            ],
        )?;
        db.execute(
            "INSERT INTO events(type, run_id, payload_json) VALUES ('submission_committed', ?1, ?2)",
            params![run_id, submission_payload(task_id, "passed", 0, "local_bug_fix")?],
        )?;
    }

    other_model.task_id = "task-2".into();
    other_model.run_id = "run-2".into();
    other_settings.task_id = "task-3".into();
    other_settings.run_id = "run-3".into();
    assert!(
        build(Manifest {
            version: 1,
            attempts: vec![attempt.clone(), other_model.clone()],
        })
        .is_err()
    );
    other_model.database = temp.path().join("sample-2.db");
    other_settings.database = temp.path().join("sample-3.db");
    accepted_external_database(&other_model.database, "task-2", "run-2", &start_tree)?;
    accepted_external_database(&other_settings.database, "task-3", "run-3", &start_tree)?;
    let report = build(Manifest {
        version: 1,
        attempts: vec![attempt.clone(), other_model, other_settings],
    })?;
    assert_eq!(report.groups.len(), 3);
    assert!(report.attempts.iter().all(|row| row.accepted));
    assert!(
        build(Manifest {
            version: 1,
            attempts: vec![attempt.clone(), attempt.clone()],
        })
        .is_err()
    );
    let mut repeated = attempt.clone();
    repeated.sample = 2;
    assert!(
        build(Manifest {
            version: 1,
            attempts: vec![attempt.clone(), repeated],
        })
        .is_err()
    );
    db.execute(
        "UPDATE events SET payload_json = '{\"baseline_tree\":\"wrong\"}' WHERE type = 'run_started'",
        [],
    )?;
    assert!(
        build(Manifest {
            version: 1,
            attempts: vec![attempt.clone()],
        })
        .is_err()
    );
    db.execute(
        "UPDATE events SET payload_json = ?1 WHERE type = 'run_started'",
        [serde_json::json!({"baseline_tree": start_tree, "evaluation": workload_launch("local_bug_fix", CacheState::Cold)?}).to_string()],
    )?;
    let report = build(Manifest {
        version: 1,
        attempts: vec![attempt],
    })?;
    assert!(report.attempts[0].accepted);
    assert_eq!(report.attempts[0].approved_events, 0);
    db.execute(
        "UPDATE events SET payload_json = ?1 WHERE type = 'submission_committed'",
        [submission_payload("task", "skipped", 0, "local_bug_fix")?],
    )?;
    let report = build(Manifest {
        version: 1,
        attempts: vec![accepted_attempt.clone()],
    })?;
    assert!(!report.attempts[0].accepted);
    db.execute(
        "UPDATE events SET payload_json = ?1 WHERE type = 'submission_committed'",
        [submission_payload("task", "passed", 0, "local_bug_fix")?],
    )?;
    db.execute("UPDATE tasks SET review_cycles=1", [])?;
    let report = build(Manifest {
        version: 1,
        attempts: vec![accepted_attempt.clone()],
    })?;
    assert!(!report.attempts[0].accepted);
    db.execute_batch(
        r#"INSERT INTO runs VALUES ('later', 'task', 'completed', 'executor');
        INSERT INTO events(type, run_id, payload_json) VALUES ('rejected', 'run', '{}');
        INSERT INTO events(type, run_id, payload_json) VALUES ('approved', 'later', '{}');"#,
    )?;
    db.execute(
        "INSERT INTO events(type, run_id, payload_json) VALUES ('submission_committed', 'later', ?1)",
        [submission_payload("task", "passed", 1, "local_bug_fix")?],
    )?;
    assert!(
        build(Manifest {
            version: 1,
            attempts: vec![accepted_attempt.clone()],
        })
        .is_err()
    );
    db.execute("UPDATE runs SET role='reviewer' WHERE id='later'", [])?;
    let report = build(Manifest {
        version: 1,
        attempts: vec![accepted_attempt],
    })?;
    assert!(!report.attempts[0].accepted);
    assert!(!report.attempts[0].final_check_gate_passed);
    let inherited = Budget {
        model_turns: 3,
        tool_calls: 2,
        reported_input_tokens: 50,
        estimated_output_tokens: 10,
        ..Default::default()
    };
    records[0].budget = inherited.clone();
    records[1].budget = inherited.clone();
    if let SessionEvent::Started {
        inherited_budget, ..
    } = &mut records[0].event
    {
        *inherited_budget = Some(inherited);
    }

    let mut file = File::create(baseline_attempt.journal.as_ref().unwrap())?;
    for record in &records {
        serde_json::to_writer(&mut file, record)?;
        writeln!(file)?;
    }

    drop(file);
    let report = build(Manifest {
        version: 1,
        attempts: vec![baseline_attempt.clone()],
    })?;
    let metrics = &report.attempts[0].metrics;
    assert_eq!(metrics.model_turns, Some(0));
    assert_eq!(metrics.tool_calls, Some(0));
    assert_eq!(metrics.input_tokens_reported, Some(0));
    assert_eq!(metrics.output_tokens_estimated, Some(0));
    let activation = Record {
        event: SessionEvent::InteractionOpened,
        sequence: 2,
        ..records[0].clone()
    };
    records[1].sequence = 3;
    write_records(
        baseline_attempt.journal.as_ref().unwrap(),
        &[records[0].clone(), activation, records[1].clone()],
    )?;
    let error = nano_metrics(&baseline_attempt)
        .err()
        .expect("interactive attempt must be rejected");
    assert!(
        error
            .to_string()
            .contains("Headless evaluation cannot include an interactive")
    );
    Ok(())
}
