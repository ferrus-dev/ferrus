use super::*;

#[test]
fn startup_prepares_runtime_schema_for_read_only_graph_queries() {
    let dir = TempDir::new().unwrap();
    let database_path = dir.path().join("ferrus.db");
    Connection::open(&database_path)
        .unwrap()
        .execute_batch(
            r#"
            CREATE TABLE runtime_metadata (
                key TEXT PRIMARY KEY,
                value TEXT
            );
            "#,
        )
        .unwrap();

    prepare_runtime_database_for_read_only_operations_at(&database_path).unwrap();

    let connection =
        Connection::open_with_flags(&database_path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let state = connection
        .query_row(
            r#"
            SELECT canonical_graph_status, canonical_graph_snapshot_id
            FROM project_runtime_state
            WHERE row_id = 1
            "#,
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .unwrap();
    assert_eq!(state, ("unknown".to_string(), None));
    assert_eq!(
        runtime_schema_version(&connection).unwrap(),
        RUNTIME_SCHEMA_VERSION
    );
}

#[test]
fn runtime_schema_migrations_adopt_legacy_database_without_data_loss() {
    let dir = TempDir::new().unwrap();
    let database_path = dir.path().join("ferrus.db");
    let mut connection = Connection::open(&database_path).unwrap();
    connection
        .execute_batch(
            r#"
            PRAGMA foreign_keys = ON;
            CREATE TABLE tasks (
                id TEXT PRIMARY KEY,
                path TEXT NOT NULL,
                status TEXT NOT NULL
            );
            CREATE TABLE runs (
                id TEXT PRIMARY KEY,
                task_id TEXT NOT NULL,
                role TEXT NOT NULL,
                agent TEXT NOT NULL,
                status TEXT NOT NULL,
                started_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                pid INTEGER,
                workspace_path TEXT NOT NULL,
                FOREIGN KEY(task_id) REFERENCES tasks(id)
            );
            CREATE TABLE events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT,
                type TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                created_at TEXT NOT NULL,
                FOREIGN KEY(run_id) REFERENCES runs(id)
            );
            CREATE TABLE runtime_metadata (
                key TEXT PRIMARY KEY,
                value TEXT
            );
            INSERT INTO tasks (id, path, status)
            VALUES ('t-007', '.ferrus/tasks/t-007.md', 'reviewing');
            INSERT INTO runs (
                id, task_id, role, agent, status, started_at, updated_at, pid, workspace_path
            ) VALUES (
                'r-007', 't-007', 'executor', 'codex', 'reviewing',
                '2026-07-19T00:00:00Z', '2026-07-19T00:01:00Z', 42, '/tmp/worktree'
            );
            INSERT INTO events (run_id, type, payload_json, created_at)
            VALUES ('r-007', 'submission_recorded', '{}', '2026-07-19T00:01:00Z');
            INSERT INTO runtime_metadata (key, value)
            VALUES ('selected_spec', 'docs/specs/legacy.md');
            "#,
        )
        .unwrap();

    initialize_schema(&mut connection).unwrap();

    assert_eq!(
        runtime_schema_version(&connection).unwrap(),
        RUNTIME_SCHEMA_VERSION
    );
    let migrations: Vec<(u32, String)> = connection
        .prepare("SELECT version, name FROM runtime_schema_migrations ORDER BY version")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        migrations,
        vec![
            (1, "adopt_legacy_runtime_schema".to_string()),
            (2, "repository_view_references".to_string()),
            (3, "frozen_repository_views".to_string()),
            (4, "canonical_graph_state".to_string()),
        ]
    );
    assert_eq!(
        connection
            .query_row("SELECT status FROM tasks WHERE id = 't-007'", [], |row| row
                .get::<_, String>(0))
            .unwrap(),
        "reviewing"
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT workspace_path FROM runs WHERE id = 'r-007'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "/tmp/worktree"
    );
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM events", [], |row| row
                .get::<_, u32>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        read_project_selection_from_database(&connection)
            .unwrap()
            .selected_spec
            .as_deref(),
        Some("docs/specs/legacy.md")
    );
    for table in ["tasks", "runs"] {
        let status = connection
            .query_row(
                &format!("SELECT repository_view_status FROM {table} WHERE id = ?1"),
                [if table == "tasks" { "t-007" } else { "r-007" }],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        assert_eq!(status, "not_built");
        let lifecycle = connection
            .query_row(
                &format!("SELECT repository_view_lifecycle FROM {table} WHERE id = ?1"),
                [if table == "tasks" { "t-007" } else { "r-007" }],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        assert_eq!(lifecycle, "mutable");
    }
    assert_eq!(
        connection
            .query_row(
                "SELECT canonical_graph_status FROM project_runtime_state WHERE row_id = 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "unknown"
    );

    initialize_schema(&mut connection).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM runtime_schema_migrations",
                [],
                |row| { row.get::<_, u32>(0) }
            )
            .unwrap(),
        RUNTIME_SCHEMA_VERSION
    );
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get::<_, u32>(0))
            .unwrap(),
        1
    );
}

#[test]
fn runtime_schema_migration_rolls_back_a_failed_version() {
    let connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch(
            r#"
            CREATE TABLE tasks (id TEXT PRIMARY KEY, path TEXT NOT NULL, status TEXT NOT NULL);
            CREATE TABLE runs (
                id TEXT PRIMARY KEY,
                task_id TEXT NOT NULL,
                role TEXT NOT NULL,
                agent TEXT NOT NULL,
                status TEXT NOT NULL,
                started_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                pid INTEGER,
                workspace_path TEXT NOT NULL
            );
            CREATE TABLE idx_tasks_repository_view_baseline (value TEXT);
            "#,
        )
        .unwrap();
    let mut connection = connection;

    let error = initialize_schema(&mut connection).unwrap_err();

    assert!(error.to_string().contains("migration 2"));
    assert_eq!(runtime_schema_version(&connection).unwrap(), 1);
    assert!(!column_exists(&connection, "tasks", "baseline_snapshot_id").unwrap());
    assert!(!column_exists(&connection, "runs", "baseline_snapshot_id").unwrap());
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM runtime_schema_migrations WHERE version = 2",
                [],
                |row| row.get::<_, u32>(0)
            )
            .unwrap(),
        0
    );
}

#[test]
fn runtime_schema_rejects_newer_versions_without_mutation() {
    let mut connection = Connection::open_in_memory().unwrap();
    connection.pragma_update(None, "user_version", 99).unwrap();

    let error = initialize_schema(&mut connection).unwrap_err();

    assert!(error.to_string().contains("newer than supported"));
    assert_eq!(runtime_schema_version(&connection).unwrap(), 99);
    assert!(!table_exists(&connection, "runtime_schema_migrations").unwrap());
}

#[tokio::test]
async fn repository_view_references_round_trip_for_tasks_and_runs() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    let repository_view = RepositoryViewReference::new(
        Some(SnapshotId::new("snapshot-baseline").unwrap()),
        Some(OverlayRevisionId::new("overlay-1").unwrap()),
        RepositoryViewStatus::Stale,
    )
    .unwrap();
    record_task_repository_view("t-001", &repository_view)
        .await
        .unwrap();
    claim_task(
        "t-001",
        ".ferrus/tasks/t-001.md",
        "executor:codex:t-001",
        60,
    )
    .await
    .unwrap();
    record_run_started_for_task_with_workspace(
        "r-view",
        "executor",
        "executor:codex:t-001",
        42,
        Some("t-001"),
        "/tmp/worktree".to_string(),
    )
    .await
    .unwrap();

    assert_eq!(
        task_repository_view("t-001").await.unwrap(),
        Some(repository_view.clone())
    );
    assert_eq!(
        run_repository_view("r-view").await.unwrap(),
        Some(repository_view.clone())
    );
    assert_eq!(
        runtime_task_context_for_agent("executor:codex:t-001")
            .await
            .unwrap()
            .unwrap()
            .repository_view,
        repository_view
    );

    record_task_status(
        "t-002",
        ".ferrus/tasks/t-002.md",
        crate::project::TaskStatus::Executing,
    )
    .await
    .unwrap();
    record_run_started_for_task_with_workspace(
        "r-late-pin",
        "executor",
        "executor:codex:t-002",
        43,
        Some("t-002"),
        "/tmp/worktree-2".to_string(),
    )
    .await
    .unwrap();
    assert_eq!(
        run_repository_view("r-late-pin")
            .await
            .unwrap()
            .unwrap()
            .status,
        RepositoryViewStatus::NotBuilt
    );
    record_task_repository_view("t-002", &repository_view)
        .await
        .unwrap();
    assert_eq!(
        run_repository_view("r-late-pin").await.unwrap(),
        Some(repository_view)
    );
    teardown(previous);
}

#[tokio::test]
async fn task_repository_view_compare_and_set_rejects_stale_refresh() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    let database_path = current_database_path().await.unwrap();
    let expected = RepositoryViewReference::default();
    let newer = RepositoryViewReference::materialized(
        SnapshotId::new("baseline-newer").unwrap(),
        Some(OverlayRevisionId::new("overlay-newer").unwrap()),
        SnapshotId::new("view-newer").unwrap(),
        RepositoryViewStatus::Available,
    )
    .unwrap();
    let older = RepositoryViewReference::materialized(
        SnapshotId::new("baseline-older").unwrap(),
        Some(OverlayRevisionId::new("overlay-older").unwrap()),
        SnapshotId::new("view-older").unwrap(),
        RepositoryViewStatus::Available,
    )
    .unwrap();

    assert!(
        compare_and_record_task_repository_view_at(&database_path, "t-001", &expected, &newer,)
            .await
            .unwrap()
    );
    assert!(
        !compare_and_record_task_repository_view_at(&database_path, "t-001", &expected, &older,)
            .await
            .unwrap()
    );
    assert_eq!(task_repository_view("t-001").await.unwrap(), Some(newer));

    teardown(previous);
}

#[tokio::test]
async fn submitted_view_is_frozen_for_reviewer_and_rejection_resumes_mutable_task_view() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    let mutable = RepositoryViewReference::materialized(
        SnapshotId::new("snapshot-baseline").unwrap(),
        Some(OverlayRevisionId::new("overlay-1").unwrap()),
        SnapshotId::new("snapshot-composed").unwrap(),
        RepositoryViewStatus::Available,
    )
    .unwrap();
    record_task_repository_view("t-001", &mutable)
        .await
        .unwrap();
    record_run_started_for_task_with_workspace(
        "r-executor",
        "executor",
        "executor:codex:t-001",
        42,
        Some("t-001"),
        "/tmp/worktree".to_string(),
    )
    .await
    .unwrap();
    let frozen = mutable
        .frozen(Digest::new("git-tree-sha1", "0123456789abcdef0123456789abcdef01234567").unwrap())
        .unwrap();

    record_task_submitted(
        "t-001",
        ".ferrus/tasks/t-001.md",
        Some("r-executor"),
        Some(&frozen),
        false,
    )
    .await
    .unwrap();
    record_run_started_for_task_with_workspace(
        "r-reviewer",
        "supervisor",
        "supervisor:codex:t-001",
        43,
        Some("t-001"),
        "/tmp/canonical".to_string(),
    )
    .await
    .unwrap();
    claim_review_task_by_id("t-001", "supervisor:codex:t-001", 60)
        .await
        .unwrap();

    let reviewer = runtime_task_context_for_agent("supervisor:codex:t-001")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reviewer.run_role.as_deref(), Some("supervisor"));
    assert_eq!(reviewer.repository_workspace_path, None);
    assert_eq!(reviewer.repository_view, frozen);

    assert!(matches!(
        record_task_review_rejected("t-001", 3).await.unwrap(),
        TaskReviewRejection::Addressing { cycles: 1 }
    ));
    assert_eq!(
        task_repository_view("t-001")
            .await
            .unwrap()
            .unwrap()
            .lifecycle,
        TaskViewLifecycle::Mutable
    );
    assert_eq!(
        run_repository_view("r-reviewer")
            .await
            .unwrap()
            .unwrap()
            .lifecycle,
        TaskViewLifecycle::FrozenSubmitted
    );
    teardown(previous);
}

#[tokio::test]
async fn canonical_graph_invalidation_and_refresh_round_trip_without_task_mutation() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    let source = CanonicalSourceIdentity {
        source_revision_id: SourceRevisionId::new("canonical-revision-1").unwrap(),
        manifest_digest: Digest::new("sha256", "aa").unwrap(),
    };

    assert_eq!(
        canonical_graph_reference().await.unwrap(),
        CanonicalGraphReference::default()
    );
    record_canonical_graph_invalidation(
        "t-001",
        None,
        Some(&source),
        CanonicalInvalidationReason::ApprovedIntegration,
    )
    .await
    .unwrap();
    assert_eq!(
        canonical_graph_reference().await.unwrap(),
        CanonicalGraphReference {
            source: Some(source.clone()),
            snapshot_id: None,
            status: CanonicalGraphStatus::Stale,
        }
    );

    let snapshot = SnapshotId::new("canonical-snapshot-1").unwrap();
    let guard = canonical_graph_refresh_guard().await.unwrap();
    assert_eq!(
        record_canonical_graph_refresh(
            Some("t-001"),
            None,
            guard,
            &source,
            &snapshot,
            &BuildId::new("canonical-build-1").unwrap(),
        )
        .await
        .unwrap(),
        CanonicalGraphRefreshOutcome::Recorded
    );
    assert_eq!(
        canonical_graph_reference().await.unwrap(),
        CanonicalGraphReference {
            source: Some(source),
            snapshot_id: Some(snapshot.clone()),
            status: CanonicalGraphStatus::Fresh,
        }
    );

    record_canonical_graph_invalidation(
        "t-001",
        None,
        None,
        CanonicalInvalidationReason::SourceComparisonUnavailable,
    )
    .await
    .unwrap();
    let stale = canonical_graph_reference().await.unwrap();
    assert_eq!(stale.status, CanonicalGraphStatus::Stale);
    assert_eq!(stale.source, None);
    assert_eq!(stale.snapshot_id, Some(snapshot));
    assert_eq!(
        list_tasks().await.unwrap()[0].status,
        TaskStatus::Executing.as_str()
    );

    teardown(previous);
}

#[tokio::test]
async fn canonical_graph_refresh_does_not_overwrite_a_newer_invalidation() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    let first_source = CanonicalSourceIdentity {
        source_revision_id: SourceRevisionId::new("canonical-revision-1").unwrap(),
        manifest_digest: Digest::new("sha256", "aa").unwrap(),
    };
    let newer_source = CanonicalSourceIdentity {
        source_revision_id: SourceRevisionId::new("canonical-revision-2").unwrap(),
        manifest_digest: Digest::new("sha256", "bb").unwrap(),
    };

    record_canonical_graph_invalidation(
        "t-001",
        None,
        Some(&first_source),
        CanonicalInvalidationReason::ApprovedIntegration,
    )
    .await
    .unwrap();
    let refresh_guard = canonical_graph_refresh_guard().await.unwrap();
    record_canonical_graph_invalidation(
        "t-001",
        None,
        Some(&newer_source),
        CanonicalInvalidationReason::ApprovedIntegration,
    )
    .await
    .unwrap();

    assert_eq!(
        record_canonical_graph_refresh(
            Some("t-001"),
            None,
            refresh_guard,
            &first_source,
            &SnapshotId::new("older-snapshot").unwrap(),
            &BuildId::new("older-build").unwrap(),
        )
        .await
        .unwrap(),
        CanonicalGraphRefreshOutcome::Superseded
    );
    assert_eq!(
        canonical_graph_reference().await.unwrap(),
        CanonicalGraphReference {
            source: Some(newer_source),
            snapshot_id: None,
            status: CanonicalGraphStatus::Stale,
        }
    );

    teardown(previous);
}

#[tokio::test]
async fn failed_canonical_refresh_does_not_overwrite_a_newer_success() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    let source = CanonicalSourceIdentity {
        source_revision_id: SourceRevisionId::new("canonical-revision-1").unwrap(),
        manifest_digest: Digest::new("sha256", "aa").unwrap(),
    };

    record_canonical_graph_invalidation(
        "t-001",
        None,
        Some(&source),
        CanonicalInvalidationReason::ApprovedIntegration,
    )
    .await
    .unwrap();
    let failed_refresh_guard = canonical_graph_refresh_guard().await.unwrap();
    let successful_refresh_guard = canonical_graph_refresh_guard().await.unwrap();
    let snapshot = SnapshotId::new("canonical-snapshot-1").unwrap();
    assert_eq!(
        record_canonical_graph_refresh(
            Some("t-001"),
            None,
            successful_refresh_guard,
            &source,
            &snapshot,
            &BuildId::new("canonical-build-1").unwrap(),
        )
        .await
        .unwrap(),
        CanonicalGraphRefreshOutcome::Recorded
    );

    record_canonical_graph_refresh_failed_best_effort("t-001", None, failed_refresh_guard).await;

    assert_eq!(
        canonical_graph_reference().await.unwrap(),
        CanonicalGraphReference {
            source: Some(source),
            snapshot_id: Some(snapshot),
            status: CanonicalGraphStatus::Fresh,
        }
    );

    teardown(previous);
}

#[test]
fn repository_view_reference_rejects_overlay_without_baseline() {
    let result = RepositoryViewReference::new(
        None,
        Some(OverlayRevisionId::new("overlay-1").unwrap()),
        RepositoryViewStatus::Available,
    );

    assert!(result.is_err());
}

#[test]
fn archive_staging_copies_before_cleanup() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let previous = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();
    std::fs::create_dir_all("docs/specs").unwrap();
    std::fs::create_dir_all(".ferrus/tasks").unwrap();
    std::fs::create_dir_all(".ferrus/runs/t-007").unwrap();
    std::fs::write("docs/specs/spec.md", "# Spec\n").unwrap();
    std::fs::write(".ferrus/tasks/t-007.md", "task").unwrap();
    std::fs::write(".ferrus/runs/t-007/SUBMISSION.md", "submission").unwrap();
    let task = TaskRecord {
        id: "t-007".to_string(),
        path: ".ferrus/tasks/t-007.md".to_string(),
        spec_path: Some("docs/specs/spec.md".to_string()),
        milestone_id: Some("m1.0".to_string()),
        status: TaskStatus::Complete.as_str().to_string(),
        paused_status: None,
        claimed_by: None,
        lease_until: None,
        last_heartbeat: None,
        check_retries: 0,
        review_cycles: 0,
        failure_reason: None,
    };
    let archive_dir = dir.path().join("runtime/archive/specs/spec-archive");
    let manifest = SpecArchiveManifest::new("docs/specs/spec.md", "now", &[task.clone()]);

    let archived = stage_spec_archive_files(
        &archive_dir,
        "docs/specs/spec.md",
        &[task.clone()],
        &manifest,
    )
    .unwrap();

    assert_eq!(archived, (1, 1));
    assert!(std::path::Path::new(".ferrus/tasks/t-007.md").exists());
    assert!(std::path::Path::new(".ferrus/runs/t-007/SUBMISSION.md").exists());
    assert!(archive_dir.join("tasks/t-007.md").exists());
    assert!(archive_dir.join("runs/t-007/SUBMISSION.md").exists());

    cleanup_checkout_archive_artifacts(&[task]).unwrap();

    assert!(!std::path::Path::new(".ferrus/tasks/t-007.md").exists());
    assert!(!std::path::Path::new(".ferrus/runs/t-007").exists());
    std::env::set_current_dir(previous).unwrap();
}

#[tokio::test]
async fn retention_references_include_active_tasks_and_runs() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    record_task_status(
        "t-active",
        ".ferrus/tasks/t-active.md",
        TaskStatus::Reviewing,
    )
    .await
    .unwrap();
    record_task_status(
        "t-complete",
        ".ferrus/tasks/t-complete.md",
        TaskStatus::Complete,
    )
    .await
    .unwrap();
    record_task_status(
        "t-live-run",
        ".ferrus/tasks/t-live-run.md",
        TaskStatus::Complete,
    )
    .await
    .unwrap();
    let active = RepositoryViewReference::materialized(
        SnapshotId::new("baseline-active").unwrap(),
        Some(OverlayRevisionId::new("overlay-active").unwrap()),
        SnapshotId::new("view-active").unwrap(),
        RepositoryViewStatus::Available,
    )
    .unwrap();
    let completed = RepositoryViewReference::materialized(
        SnapshotId::new("baseline-complete").unwrap(),
        Some(OverlayRevisionId::new("overlay-complete").unwrap()),
        SnapshotId::new("view-complete").unwrap(),
        RepositoryViewStatus::Available,
    )
    .unwrap();
    let live_run = RepositoryViewReference::materialized(
        SnapshotId::new("baseline-live-run").unwrap(),
        Some(OverlayRevisionId::new("overlay-live-run").unwrap()),
        SnapshotId::new("view-live-run").unwrap(),
        RepositoryViewStatus::Available,
    )
    .unwrap();
    record_task_repository_view("t-active", &active)
        .await
        .unwrap();
    record_task_repository_view("t-complete", &completed)
        .await
        .unwrap();
    record_task_repository_view("t-live-run", &live_run)
        .await
        .unwrap();
    record_run_started_for_task_with_workspace(
        "r-live",
        "supervisor",
        "supervisor:codex:t-live-run",
        std::process::id(),
        Some("t-live-run"),
        "/tmp/canonical".to_string(),
    )
    .await
    .unwrap();

    let references = repository_graph_retention_references().await.unwrap();

    assert!(
        references
            .snapshot_ids
            .contains(&SnapshotId::new("baseline-active").unwrap())
    );
    assert!(
        references
            .snapshot_ids
            .contains(&SnapshotId::new("view-active").unwrap())
    );
    assert!(
        references
            .view_names
            .contains(&PublishedViewName::new("task-overlay:t-active").unwrap())
    );
    assert!(
        !references
            .snapshot_ids
            .contains(&SnapshotId::new("view-complete").unwrap())
    );
    assert!(
        !references
            .view_names
            .contains(&PublishedViewName::new("task-overlay:t-complete").unwrap())
    );
    assert!(
        references
            .snapshot_ids
            .contains(&SnapshotId::new("baseline-live-run").unwrap())
    );
    assert!(
        references
            .snapshot_ids
            .contains(&SnapshotId::new("view-live-run").unwrap())
    );
    assert!(
        references
            .view_names
            .contains(&PublishedViewName::new("task-overlay:t-live-run").unwrap())
    );

    std::env::set_current_dir(previous).unwrap();
}
