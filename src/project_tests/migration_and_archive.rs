//! Runtime tests for legacy migration and completed specification archival.

use super::*;

#[tokio::test]
async fn archive_completed_spec_writes_outcome_and_moves_artifacts() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (dir, previous) = setup_project().await;
    record_task_status(
        "t-001",
        ".ferrus/tasks/t-001.md",
        crate::project::TaskStatus::Reset,
    )
    .await
    .unwrap();
    tokio::fs::create_dir_all("docs/specs").await.unwrap();
    tokio::fs::create_dir_all(".ferrus/tasks").await.unwrap();
    tokio::fs::create_dir_all(".ferrus/runs/t-007")
        .await
        .unwrap();
    let spec_path = "docs/specs/2026-07-05-archive.md";
    tokio::fs::write(
        spec_path,
        "# Archive\n\n## Milestones\n\n- [x] #1.0 Done\n  - ID: m1.0\n  - Depends on: none\n",
    )
    .await
    .unwrap();
    tokio::fs::write(".ferrus/tasks/t-007.md", "task text")
        .await
        .unwrap();
    tokio::fs::write(".ferrus/runs/t-007/SUBMISSION.md", "submission")
        .await
        .unwrap();
    record_task_status_with_origin(
        "t-007",
        ".ferrus/tasks/t-007.md",
        TaskStatus::Complete,
        Some(spec_path),
        Some("m1.0"),
    )
    .await
    .unwrap();

    let result = archive_completed_spec(spec_path, "Delivered the archive workflow.")
        .await
        .unwrap();

    let spec = tokio::fs::read_to_string(spec_path).await.unwrap();
    assert!(spec.contains("## Outcome"));
    assert!(spec.contains("Delivered the archive workflow."));
    assert!(!std::path::Path::new(".ferrus/tasks/t-007.md").exists());
    assert!(!std::path::Path::new(".ferrus/runs/t-007").exists());
    assert!(
        std::path::Path::new(&result.archive_dir)
            .join("manifest.toml")
            .exists()
    );
    assert!(
        std::path::Path::new(&result.archive_dir)
            .join("tasks/t-007.md")
            .exists()
    );
    assert!(
        std::path::Path::new(&result.archive_dir)
            .join("runs/t-007/SUBMISSION.md")
            .exists()
    );
    let task = list_tasks_for_spec(spec_path)
        .await
        .unwrap()
        .into_iter()
        .find(|task| task.id == "t-007")
        .unwrap();
    assert!(task.path.contains("archive"));
    let archived_task_path = task.path.clone();
    assert_eq!(
        read_last_spec_archive_path().await.unwrap().as_deref(),
        Some(result.archive_dir.as_str())
    );
    let database_path = current_database_path().await.unwrap();
    let mut checks = Vec::new();
    add_runtime_doctor_checks(&mut checks, &database_path).await;
    assert!(
        checks.iter().all(|check| check.ok),
        "unexpected failed checks: {:?}",
        checks
            .iter()
            .filter(|check| !check.ok)
            .map(|check| check.message.as_str())
            .collect::<Vec<_>>()
    );

    let error = archive_completed_spec(spec_path, "Second archive attempt.")
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("no checkout task or run artifacts remain"));
    assert!(std::path::Path::new(&archived_task_path).exists());
    assert!(
        std::path::Path::new(&result.archive_dir)
            .join("runs/t-007/SUBMISSION.md")
            .exists()
    );
    assert_eq!(
        read_last_spec_archive_path().await.unwrap().as_deref(),
        Some(result.archive_dir.as_str())
    );

    teardown(previous);
    drop(dir);
}

#[tokio::test]
async fn project_selection_round_trips_through_runtime_database() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;

    write_project_selection(&ProjectSelection {
        selected_spec: Some("docs/specs/spec.md".to_string()),
    })
    .await
    .unwrap();

    let selection = read_project_selection().await.unwrap();
    assert_eq!(
        selection,
        ProjectSelection {
            selected_spec: Some("docs/specs/spec.md".to_string()),
        }
    );
    teardown(previous);
}

#[tokio::test]
async fn last_spec_path_round_trips_through_runtime_database() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;

    assert_eq!(read_last_spec_path().await.unwrap(), None);
    write_last_spec_path("docs/specs/spec.md").await.unwrap();
    assert_eq!(
        read_last_spec_path().await.unwrap().as_deref(),
        Some("docs/specs/spec.md")
    );
    clear_last_spec_path().await.unwrap();
    assert_eq!(read_last_spec_path().await.unwrap(), None);

    teardown(previous);
}

#[tokio::test]
async fn project_runtime_state_migrates_temporary_runtime_metadata_table() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    let database_path = current_database_path().await.unwrap();
    {
        let connection = Connection::open(&database_path).unwrap();
        connection
            .execute(
                r#"
                CREATE TABLE IF NOT EXISTS runtime_metadata (
                    key TEXT PRIMARY KEY,
                    value TEXT,
                    updated_at TEXT NOT NULL
                )
                "#,
                [],
            )
            .unwrap();
        for (key, value) in [
            ("selected_spec", "docs/specs/spec.md"),
            ("last_spec_path", "docs/specs/spec.md"),
        ] {
            connection
                .execute(
                    "INSERT OR REPLACE INTO runtime_metadata (key, value, updated_at) VALUES (?1, ?2, ?3)",
                    params![key, value, timestamp()],
                )
                .unwrap();
        }
    }

    let selection = read_project_selection().await.unwrap();
    let last_spec_path = read_last_spec_path().await.unwrap();

    assert_eq!(
        selection.selected_spec.as_deref(),
        Some("docs/specs/spec.md")
    );
    assert_eq!(last_spec_path.as_deref(), Some("docs/specs/spec.md"));

    teardown(previous);
}

#[tokio::test]
async fn migration_preserves_idle_legacy_selected_spec() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    let legacy = legacy_state::LegacyStateData {
        state: Some(LegacyTaskState::Idle),
        selected_spec: Some("docs/specs/selected.md".to_string()),
        ..Default::default()
    };

    migrate_legacy_project_selection(&legacy).await.unwrap();

    let selection = read_project_selection().await.unwrap();
    assert_eq!(
        selection.selected_spec.as_deref(),
        Some("docs/specs/selected.md")
    );

    teardown(previous);
}

#[tokio::test]
async fn migration_preserves_legacy_consultation_resume_state() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    let legacy = legacy_state::LegacyStateData {
        state: Some(LegacyTaskState::Consultation),
        paused_state: Some(LegacyTaskState::Addressing),
        check_retries: 2,
        review_cycles: 1,
        failure_reason: Some("legacy check failure".to_string()),
        ..Default::default()
    };

    migrate_legacy_active_task(&legacy).await.unwrap();
    let task = list_tasks().await.unwrap().remove(0);
    assert_eq!(task.status, TaskStatus::Consultation.as_str());
    assert_eq!(task.paused_status.as_deref(), Some("addressing"));
    assert_eq!(task.check_retries, 2);
    assert_eq!(task.review_cycles, 1);
    assert_eq!(task.failure_reason.as_deref(), Some("legacy check failure"));

    let restored = restore_task_from_consultation("t-001").await.unwrap();
    assert!(matches!(
        restored,
        TaskConsultRestore::Restored { status } if status == "addressing"
    ));

    teardown(previous);
}

#[tokio::test]
async fn migration_preserves_legacy_human_answer_resume_state() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    let legacy = legacy_state::LegacyStateData {
        state: Some(LegacyTaskState::AwaitingHuman),
        paused_state: Some(LegacyTaskState::Reviewing),
        awaiting_human_by: Some("supervisor:codex:t-001".to_string()),
        check_retries: 2,
        review_cycles: 1,
        ..Default::default()
    };

    migrate_legacy_active_task(&legacy).await.unwrap();
    let task = list_tasks().await.unwrap().remove(0);
    assert_eq!(task.status, TaskStatus::AwaitingHuman.as_str());
    assert_eq!(task.paused_status, None);
    assert_eq!(
        task_awaiting_human_status("t-001")
            .await
            .unwrap()
            .as_deref(),
        Some("reviewing")
    );
    assert_eq!(
        task_human_question_owner("t-001").await.unwrap().as_deref(),
        Some("supervisor:codex:t-001")
    );
    assert_eq!(task.check_retries, 2);
    assert_eq!(task.review_cycles, 1);

    let restored = restore_task_from_human_answer("t-001").await.unwrap();
    assert!(matches!(
        restored,
        TaskHumanAnswerRestore::Restored { status } if status == "reviewing"
    ));

    teardown(previous);
}

#[tokio::test]
async fn migration_copies_paused_legacy_interaction_artifacts() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    tokio::fs::create_dir_all(".ferrus/tasks").await.unwrap();
    tokio::fs::write(".ferrus/TASK.md", "legacy task")
        .await
        .unwrap();
    for (path, contents) in [
        (".ferrus/QUESTION.md", "legacy question"),
        (".ferrus/ANSWER.md", "legacy answer"),
        (".ferrus/CONSULT_REQUEST.md", "legacy consult request"),
        (".ferrus/CONSULT_RESPONSE.md", "legacy consult response"),
    ] {
        tokio::fs::write(path, contents).await.unwrap();
    }

    copy_legacy_artifacts(true).await.unwrap();

    assert_eq!(
        tokio::fs::read_to_string(".ferrus/tasks/t-001.md")
            .await
            .unwrap(),
        "legacy task"
    );
    assert_eq!(
        tokio::fs::read_to_string(".ferrus/TASK.md").await.unwrap(),
        crate::templates::TASK_TEMPLATE
    );

    for (path, expected) in [
        (".ferrus/runs/t-001/QUESTION.md", "legacy question"),
        (".ferrus/runs/t-001/ANSWER.md", "legacy answer"),
        (
            ".ferrus/runs/t-001/CONSULT_REQUEST.md",
            "legacy consult request",
        ),
        (
            ".ferrus/runs/t-001/CONSULT_RESPONSE.md",
            "legacy consult response",
        ),
    ] {
        let contents = tokio::fs::read_to_string(path).await.unwrap();
        assert_eq!(contents, expected);
    }

    teardown(previous);
}

#[tokio::test]
async fn migration_does_not_create_phantom_task_artifact_for_idle_state() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    tokio::fs::create_dir_all(".ferrus/tasks").await.unwrap();
    tokio::fs::write(".ferrus/TASK.md", "legacy draft task")
        .await
        .unwrap();

    copy_legacy_artifacts(false).await.unwrap();

    assert!(!std::path::Path::new(".ferrus/tasks/t-001.md").exists());
    assert!(!std::path::Path::new(".ferrus/runs/t-001").exists());
    assert_eq!(
        tokio::fs::read_to_string(".ferrus/TASK.md").await.unwrap(),
        crate::templates::TASK_TEMPLATE
    );

    teardown(previous);
}
