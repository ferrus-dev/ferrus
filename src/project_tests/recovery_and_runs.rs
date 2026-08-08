use super::*;

#[tokio::test]
async fn runtime_doctor_checks_detect_missing_active_artifacts() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    let database_path = current_database_path().await.unwrap();
    let mut checks = Vec::new();

    add_runtime_doctor_checks(&mut checks, &database_path).await;

    assert!(checks.iter().any(|check| {
        !check.ok && check.message == "task artifact exists for t-001 at .ferrus/tasks/t-001.md"
    }));
    assert!(checks.iter().any(|check| {
        !check.ok
            && check.message == "run artifact directory exists for t-001 at .ferrus/runs/t-001"
    }));

    teardown(previous);
}

#[tokio::test]
async fn runtime_doctor_checks_accept_consistent_active_task() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    tokio::fs::create_dir_all(".ferrus/tasks").await.unwrap();
    tokio::fs::write(".ferrus/tasks/t-001.md", "task")
        .await
        .unwrap();
    tokio::fs::create_dir_all(".ferrus/runs/t-001")
        .await
        .unwrap();
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

    teardown(previous);
}

#[tokio::test]
async fn runtime_doctor_ignores_unknown_current_compatibility_task() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    tokio::fs::create_dir_all(".ferrus/tasks").await.unwrap();
    tokio::fs::write(".ferrus/tasks/t-001.md", "task")
        .await
        .unwrap();
    tokio::fs::create_dir_all(".ferrus/runs/t-001")
        .await
        .unwrap();
    record_run_started("supervisor", "supervisor:codex", std::process::id())
        .await
        .unwrap();
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

    teardown(previous);
}

#[tokio::test]
async fn migrate_retires_legacy_current_task_row() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    record_task_status(
        CURRENT_TASK_ID,
        CURRENT_TASK_PATH,
        crate::project::TaskStatus::Executing,
    )
    .await
    .unwrap();
    let run = record_run_started("supervisor", "supervisor:codex", std::process::id())
        .await
        .unwrap();
    assert_eq!(run.task_id, CURRENT_TASK_ID);

    retire_legacy_current_task_row().await.unwrap();

    let tasks = list_tasks().await.unwrap();
    let current = tasks
        .iter()
        .find(|task| task.id == CURRENT_TASK_ID)
        .unwrap();
    assert_eq!(current.status, TaskStatus::Reset.as_str());
    let runs = list_runs(10).await.unwrap();
    let run = runs
        .iter()
        .find(|candidate| candidate.id == run.id)
        .unwrap();
    assert_eq!(run.task_id, "t-001");

    teardown(previous);
}

#[tokio::test]
async fn recover_expired_task_leases_releases_stale_claims() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    claim_task("t-001", ".ferrus/tasks/t-001.md", "executor:codex:1", 0)
        .await
        .unwrap();

    let recovered = recover_expired_task_leases().await.unwrap();
    let tasks = list_tasks().await.unwrap();
    let events = list_events(10, None).await.unwrap();

    assert_eq!(recovered, 1);
    assert_eq!(tasks[0].claimed_by, None);
    assert_eq!(tasks[0].lease_until, None);
    assert_eq!(tasks[0].last_heartbeat, None);
    assert!(
        events
            .iter()
            .any(|event| event.event_type == "task_lease_expired")
    );

    teardown(previous);
}

#[tokio::test]
async fn live_active_run_task_ids_can_be_filtered_by_role() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    record_task_status(
        "t-002",
        ".ferrus/tasks/t-002.md",
        crate::project::TaskStatus::Consultation,
    )
    .await
    .unwrap();
    let workspace = std::env::current_dir()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    record_run_started_for_task_with_workspace(
        "run-executor-t-001",
        "executor",
        "executor:codex:t-001",
        std::process::id(),
        Some("t-001"),
        workspace.clone(),
    )
    .await
    .unwrap();
    record_run_started_for_task_with_workspace(
        "run-supervisor-t-002",
        "supervisor",
        "supervisor:codex:t-002",
        std::process::id(),
        Some("t-002"),
        workspace,
    )
    .await
    .unwrap();

    let supervisor_tasks = live_active_run_task_ids_for_role("supervisor")
        .await
        .unwrap();
    let live_agents = live_active_run_agents().await.unwrap();

    assert_eq!(
        supervisor_tasks,
        std::collections::HashSet::from(["t-002".to_string()])
    );
    assert_eq!(
        live_agents,
        std::collections::HashSet::from([
            "executor:codex:t-001".to_string(),
            "supervisor:codex:t-002".to_string()
        ])
    );
    teardown(previous);
}

#[tokio::test]
async fn recover_expired_task_leases_preserves_claims_with_live_runs() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    claim_task("t-001", ".ferrus/tasks/t-001.md", "executor:codex:t-001", 0)
        .await
        .unwrap();
    record_run_started_for_task_with_workspace(
        &allocate_run_id("executor", "executor:codex:t-001"),
        "executor",
        "executor:codex:t-001",
        std::process::id(),
        Some("t-001"),
        path_string(&std::env::current_dir().unwrap()),
    )
    .await
    .unwrap();
    let database_path = current_database_path().await.unwrap();

    let preview = preview_runtime_recovery_from(&database_path).await.unwrap();
    let recovered = recover_expired_task_leases().await.unwrap();
    let tasks = list_tasks().await.unwrap();

    assert_eq!(preview.expired_task_leases, 0);
    assert_eq!(recovered, 0);
    assert_eq!(tasks[0].claimed_by.as_deref(), Some("executor:codex:t-001"));
    assert!(tasks[0].lease_until.is_some());

    teardown(previous);
}

#[tokio::test]
async fn runtime_doctor_checks_database_task_artifacts() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (dir, previous) = setup_project().await;
    tokio::fs::create_dir_all(dir.path().join(".ferrus/tasks"))
        .await
        .unwrap();
    tokio::fs::create_dir_all(dir.path().join(".ferrus/runs/t-010"))
        .await
        .unwrap();
    tokio::fs::write(dir.path().join(".ferrus/tasks/t-010.md"), "task")
        .await
        .unwrap();
    record_task_status(
        "t-010",
        ".ferrus/tasks/t-010.md",
        crate::project::TaskStatus::Executing,
    )
    .await
    .unwrap();
    let database_path = current_database_path().await.unwrap();
    let mut checks = Vec::new();

    add_runtime_doctor_checks(&mut checks, &database_path).await;

    assert!(
        checks
            .iter()
            .any(|check| check.ok && check.message == "task rows can be read from ferrus.db")
    );
    assert!(
        checks
            .iter()
            .any(|check| check.ok && check.message.contains("task artifact exists for t-010"))
    );
    assert!(checks.iter().any(|check| {
        check.ok
            && check
                .message
                .contains("run artifact directory exists for t-010")
    }));

    teardown(previous);
}

#[tokio::test]
async fn preview_orphaned_worktrees_ignores_active_tasks_and_runs() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (dir, previous) = setup_project().await;
    let workspace = dir.path();
    let worktrees_dir = workspace.join(".ferrus/projects/test-project/worktrees");
    tokio::fs::create_dir_all(worktrees_dir.join("t-active"))
        .await
        .unwrap();
    tokio::fs::create_dir_all(worktrees_dir.join("t-run"))
        .await
        .unwrap();
    tokio::fs::create_dir_all(worktrees_dir.join("t-orphan"))
        .await
        .unwrap();
    tokio::fs::create_dir_all(worktrees_dir.join(BASELINE_WORKTREE_METADATA_DIR))
        .await
        .unwrap();

    record_task_status(
        "t-active",
        ".ferrus/tasks/t-active.md",
        crate::project::TaskStatus::Addressing,
    )
    .await
    .unwrap();
    let run = record_run_started_with_workspace(
        "executor-run-t-run",
        "executor",
        "executor:codex:t-run",
        std::process::id(),
        path_string(&worktrees_dir.join("t-run")),
    )
    .await
    .unwrap();
    record_task_status(
        "t-run",
        ".ferrus/tasks/t-run.md",
        crate::project::TaskStatus::Complete,
    )
    .await
    .unwrap();
    let attached =
        attach_running_run_to_task("executor:codex:t-run", "t-run", ".ferrus/tasks/t-run.md")
            .await
            .unwrap();
    assert_eq!(attached.as_deref(), Some(run.id.as_str()));

    let registration = ProjectRegistration {
        local_ref: LocalProjectRef {
            project_id: "test-project".to_string(),
            name: "test".to_string(),
            data_dir: path_string(&workspace.join(".ferrus/projects/test-project")),
        },
        metadata: ProjectMetadata {
            id: "test-project".to_string(),
            name: "test".to_string(),
            workspace_dir: path_string(workspace),
            ferrus_dir: path_string(&workspace.join(".ferrus")),
            vcs: None,
            origin_repo: None,
            default_branch: None,
            current_head: None,
            created_at: "2026-05-16T10:00:00Z".to_string(),
            last_opened_at: "2026-05-16T10:00:00Z".to_string(),
            version: PROJECT_VERSION,
        },
        data_dir: workspace.join(".ferrus/projects/test-project"),
        database_path: workspace.join(".ferrus/projects/test-project/ferrus.db"),
    };
    let orphaned = orphaned_worktrees_for(&registration).await.unwrap();

    assert_eq!(orphaned, vec![worktrees_dir.join("t-orphan")]);

    teardown(previous);
}

#[tokio::test]
async fn preview_runtime_recovery_reports_pending_work_without_mutating() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    claim_task("t-001", ".ferrus/tasks/t-001.md", "executor:codex:1", 0)
        .await
        .unwrap();
    let database_path = current_database_path().await.unwrap();
    let mut checks = Vec::new();

    let preview = preview_runtime_recovery_from(&database_path).await.unwrap();
    add_recovery_doctor_checks(&mut checks, &database_path).await;
    let tasks = list_tasks().await.unwrap();

    assert_eq!(preview.interrupted_runs, 0);
    assert_eq!(preview.expired_task_leases, 1);
    assert_eq!(tasks[0].claimed_by.as_deref(), Some("executor:codex:1"));
    assert!(checks.iter().any(|check| {
        !check.ok
            && check
                .message
                .contains("expired task lease recovery pending (1")
    }));

    teardown(previous);
}

#[tokio::test]
async fn list_runs_and_events_reads_runtime_rows() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;

    let run = record_run_started("executor", "executor:codex:1", std::process::id())
        .await
        .unwrap();
    record_task_status(
        "t-002",
        ".ferrus/tasks/t-002.md",
        crate::project::TaskStatus::Executing,
    )
    .await
    .unwrap();
    let attached =
        attach_running_run_to_task("executor:codex:1", "t-002", ".ferrus/tasks/t-002.md")
            .await
            .unwrap();
    assert_eq!(attached.as_deref(), Some(run.id.as_str()));
    record_runtime_event(
        Some(run.id.clone()),
        "test_event",
        serde_json::json!({ "ok": true }),
    )
    .await
    .unwrap();
    record_run_finished(&run.id, 0).await.unwrap();

    let runs = list_runs(10).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].id, run.id);
    assert_eq!(runs[0].task_id, "t-002");
    assert_eq!(runs[0].role, "executor");
    assert_eq!(runs[0].agent, "executor:codex:1");
    assert_eq!(runs[0].status, "completed");
    assert!(runs[0].pid.is_none());
    assert!(!runs[0].started_at.is_empty());
    assert!(!runs[0].updated_at.is_empty());

    let events = list_events(10, Some(run.id.clone())).await.unwrap();
    assert!(events.iter().any(|event| event.event_type == "run_started"));
    assert!(
        events
            .iter()
            .any(|event| event.event_type == "run_task_attached")
    );
    assert!(events.iter().any(|event| event.event_type == "test_event"));
    assert!(
        events
            .iter()
            .any(|event| event.event_type == "run_finished")
    );
    assert!(
        events
            .iter()
            .all(|event| event.run_id.as_deref() == Some(run.id.as_str()))
    );

    teardown(previous);
}

#[tokio::test]
async fn record_run_started_can_use_preallocated_run_id() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    let run_id = allocate_run_id("executor", "executor:codex:t-002");

    let run = record_run_started_with_id(
        &run_id,
        "executor",
        "executor:codex:t-002",
        std::process::id(),
    )
    .await
    .unwrap();

    assert_eq!(run.id, run_id);
    let runs = list_runs(10).await.unwrap();
    assert!(runs.iter().any(|run| run.id == run_id));

    teardown(previous);
}

#[tokio::test]
async fn record_run_started_can_store_explicit_workspace_path() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (dir, previous) = setup_project().await;
    let run_id = allocate_run_id("executor", "executor:codex:t-003");
    let workspace_path = path_string(&dir.path().join("worktrees").join("t-003"));

    let run = record_run_started_with_workspace(
        &run_id,
        "executor",
        "executor:codex:t-003",
        std::process::id(),
        workspace_path.clone(),
    )
    .await
    .unwrap();

    assert_eq!(run.id, run_id);
    assert_eq!(run.workspace_path, workspace_path);
    let runs = list_runs(10).await.unwrap();
    assert!(
        runs.iter()
            .any(|run| run.id == run_id && run.workspace_path == workspace_path)
    );

    teardown(previous);
}

#[tokio::test]
async fn record_run_started_can_target_requested_task_without_lease() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (dir, previous) = setup_project().await;
    record_task_status(
        "t-012",
        ".ferrus/tasks/t-012.md",
        crate::project::TaskStatus::AwaitingHuman,
    )
    .await
    .unwrap();
    let run_id = allocate_run_id("executor", "executor:codex:t-012");
    let workspace_path = path_string(&dir.path().join("worktrees").join("t-012"));

    let run = record_run_started_for_task_with_workspace(
        &run_id,
        "executor",
        "executor:codex:t-012",
        std::process::id(),
        Some("t-012"),
        workspace_path.clone(),
    )
    .await
    .unwrap();

    assert_eq!(run.id, run_id);
    assert_eq!(run.task_id, "t-012");
    let context = runtime_task_context_for_agent("executor:codex:t-012")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(context.task_id, "t-012");
    assert_eq!(context.task_path, ".ferrus/tasks/t-012.md");
    assert_eq!(
        context.workspace_path.as_deref(),
        Some(workspace_path.as_str())
    );

    teardown(previous);
}

#[tokio::test]
async fn consultation_attachment_is_exclusive_to_one_supervisor_run() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    record_task_status(
        "t-007",
        ".ferrus/tasks/t-007.md",
        crate::project::TaskStatus::Executing,
    )
    .await
    .unwrap();
    let task_view = RepositoryViewReference::materialized(
        SnapshotId::new("consultation-baseline").unwrap(),
        None,
        SnapshotId::new("consultation-view").unwrap(),
        RepositoryViewStatus::Available,
    )
    .unwrap();
    record_task_repository_view("t-007", &task_view)
        .await
        .unwrap();
    record_run_started_for_task_with_workspace(
        "r-consulted-executor",
        "executor",
        "executor:codex:t-007",
        42,
        Some("t-007"),
        "/tmp/task-t-007".to_string(),
    )
    .await
    .unwrap();
    record_task_consultation_requested("t-007", crate::project::TaskStatus::Executing)
        .await
        .unwrap();
    let first_run = record_run_started("supervisor", "supervisor:codex:1", std::process::id())
        .await
        .unwrap();
    let second_run = record_run_started("supervisor", "supervisor:codex:2", std::process::id())
        .await
        .unwrap();

    let first = attach_running_run_to_next_consultation("supervisor:codex:1")
        .await
        .unwrap();
    let second = attach_running_run_to_next_consultation("supervisor:codex:2")
        .await
        .unwrap();

    assert_eq!(
        first.as_ref().map(|context| context.task_id.as_str()),
        Some("t-007")
    );
    assert_eq!(
        first
            .as_ref()
            .and_then(|context| context.repository_workspace_path.as_deref()),
        Some("/tmp/task-t-007")
    );
    assert_eq!(
        first.as_ref().map(|context| &context.repository_view),
        Some(&task_view)
    );
    assert!(second.is_none());

    let runs = list_runs(10).await.unwrap();
    let first = runs.iter().find(|run| run.id == first_run.id).unwrap();
    let second = runs.iter().find(|run| run.id == second_run.id).unwrap();
    assert_eq!(first.task_id, "t-007");
    assert_eq!(second.task_id, CURRENT_TASK_ID);

    teardown(previous);
}

#[tokio::test]
async fn targeted_consultation_attachment_does_not_steal_another_task() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    record_task_status(
        "t-007",
        ".ferrus/tasks/t-007.md",
        crate::project::TaskStatus::Executing,
    )
    .await
    .unwrap();
    record_task_status(
        "t-008",
        ".ferrus/tasks/t-008.md",
        crate::project::TaskStatus::Executing,
    )
    .await
    .unwrap();
    record_task_consultation_requested("t-007", crate::project::TaskStatus::Executing)
        .await
        .unwrap();
    record_task_consultation_requested("t-008", crate::project::TaskStatus::Executing)
        .await
        .unwrap();
    let first_run = record_run_started("supervisor", "supervisor:codex:t-008", std::process::id())
        .await
        .unwrap();
    let second_run = record_run_started("supervisor", "supervisor:codex:t-009", std::process::id())
        .await
        .unwrap();

    let first = attach_running_run_to_consultation("t-008", "supervisor:codex:t-008")
        .await
        .unwrap();
    let second = attach_running_run_to_consultation("t-009", "supervisor:codex:t-009")
        .await
        .unwrap();

    assert_eq!(
        first.as_ref().map(|context| context.task_id.as_str()),
        Some("t-008")
    );
    assert!(second.is_none());

    let runs = list_runs(10).await.unwrap();
    let first = runs.iter().find(|run| run.id == first_run.id).unwrap();
    let second = runs.iter().find(|run| run.id == second_run.id).unwrap();
    assert_eq!(first.task_id, "t-008");
    assert_eq!(second.task_id, CURRENT_TASK_ID);

    teardown(previous);
}

#[tokio::test]
async fn list_registered_projects_reads_valid_and_invalid_entries() {
    let dir = TempDir::new().unwrap();
    let projects_dir = dir.path().join("projects");
    let valid_dir = projects_dir.join("PVALID");
    let invalid_dir = projects_dir.join("PBROKEN");
    std::fs::create_dir_all(&valid_dir).unwrap();
    std::fs::create_dir_all(&invalid_dir).unwrap();
    std::fs::write(valid_dir.join("ferrus.db"), "").unwrap();
    write_toml(
        &valid_dir.join("project.toml"),
        &ProjectMetadata {
            id: "PVALID".to_string(),
            name: "ferrus".to_string(),
            workspace_dir: "/tmp/ferrus".to_string(),
            ferrus_dir: "/tmp/ferrus/.ferrus".to_string(),
            vcs: Some("git".to_string()),
            origin_repo: None,
            default_branch: Some("main".to_string()),
            current_head: None,
            created_at: "2026-05-16T10:00:00Z".to_string(),
            last_opened_at: "2026-05-17T10:00:00Z".to_string(),
            version: PROJECT_VERSION,
        },
    )
    .await
    .unwrap();
    std::fs::write(invalid_dir.join("project.toml"), "not = [toml").unwrap();

    let projects = list_registered_projects_from(&projects_dir).await.unwrap();

    assert_eq!(projects.len(), 2);
    let valid = projects
        .iter()
        .find(|project| project.id == "PVALID")
        .unwrap();
    assert_eq!(valid.name.as_deref(), Some("ferrus"));
    assert_eq!(valid.workspace_dir.as_deref(), Some("/tmp/ferrus"));
    assert!(valid.database_exists);
    assert!(valid.error.is_none());

    let invalid = projects
        .iter()
        .find(|project| project.id == "PBROKEN")
        .unwrap();
    assert!(invalid.name.is_none());
    assert!(!invalid.database_exists);
    assert!(invalid.error.is_some());
}

#[tokio::test]
async fn touch_current_project_updates_last_opened_without_rewriting_local_ref() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (dir, previous) = setup_project().await;
    let workspace = dir.path();
    let data_dir = workspace.join(".ferrus/projects/test-project");
    let metadata_path = data_dir.join("project.toml");
    let created_at = "2026-05-16T10:00:00Z";
    write_toml(
        &metadata_path,
        &ProjectMetadata {
            id: "test-project".to_string(),
            name: "old-name".to_string(),
            workspace_dir: "/old/workspace".to_string(),
            ferrus_dir: "/old/workspace/.ferrus".to_string(),
            vcs: None,
            origin_repo: None,
            default_branch: None,
            current_head: None,
            created_at: created_at.to_string(),
            last_opened_at: "2026-05-16T11:00:00Z".to_string(),
            version: PROJECT_VERSION,
        },
    )
    .await
    .unwrap();
    let local_ref_before = tokio::fs::read_to_string(workspace.join(".ferrus/project.toml"))
        .await
        .unwrap();

    let registration = touch_current_project().await.unwrap();
    let metadata = read_project_metadata_from(&metadata_path).await.unwrap();
    let local_ref_after = tokio::fs::read_to_string(workspace.join(".ferrus/project.toml"))
        .await
        .unwrap();
    let canonical_workspace = tokio::fs::canonicalize(workspace).await.unwrap();

    assert_eq!(registration.local_ref.project_id, "test-project");
    assert_eq!(metadata.id, "test-project");
    assert_eq!(metadata.created_at, created_at);
    assert_ne!(metadata.last_opened_at, "2026-05-16T11:00:00Z");
    assert_eq!(metadata.workspace_dir, path_string(&canonical_workspace));
    assert_eq!(local_ref_after, local_ref_before);

    teardown(previous);
}
