//! Tests for consultation.

use super::*;

#[tokio::test]
async fn consultation_workspace_uses_latest_executor_task_worktree() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let previous = std::env::current_dir().unwrap();
    std::fs::create_dir_all(dir.path().join(".ferrus")).unwrap();
    std::env::set_current_dir(dir.path()).unwrap();

    let data_dir = dir.path().join("runtime");
    let workspace_dir = data_dir.join("worktrees").join("t-010");
    tokio::fs::create_dir_all(&workspace_dir).await.unwrap();
    tokio::fs::create_dir_all(&data_dir).await.unwrap();
    let local_ref = crate::project::LocalProjectRef {
        project_id: "test-project".to_string(),
        name: "test".to_string(),
        data_dir: data_dir.to_string_lossy().into_owned(),
    };
    tokio::fs::write(
        ".ferrus/project.toml",
        toml::to_string_pretty(&local_ref).unwrap(),
    )
    .await
    .unwrap();
    let metadata = crate::project::ProjectMetadata {
        id: "test-project".to_string(),
        name: "test".to_string(),
        workspace_dir: dir.path().to_string_lossy().into_owned(),
        ferrus_dir: dir.path().join(".ferrus").to_string_lossy().into_owned(),
        vcs: Some("git".to_string()),
        origin_repo: None,
        default_branch: None,
        current_head: None,
        created_at: "2026-06-14T00:00:00Z".to_string(),
        last_opened_at: "2026-06-14T00:00:00Z".to_string(),
        version: 1,
    };
    tokio::fs::write(
        data_dir.join("project.toml"),
        toml::to_string_pretty(&metadata).unwrap(),
    )
    .await
    .unwrap();
    crate::project::record_task_status(
        "t-010",
        ".ferrus/tasks/t-010.md",
        crate::project::TaskStatus::Consultation,
    )
    .await
    .unwrap();
    crate::project::record_run_started_with_workspace(
        "run-executor-t-010",
        ROLE_EXECUTOR,
        "executor:codex:t-010",
        std::process::id(),
        workspace_dir.to_string_lossy().into_owned(),
    )
    .await
    .unwrap();
    crate::project::attach_running_run_to_task(
        "executor:codex:t-010",
        "t-010",
        ".ferrus/tasks/t-010.md",
    )
    .await
    .unwrap();

    let workspace = latest_executor_workspace_for_task("t-010")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(workspace.workspace_dir, workspace_dir);
    assert_eq!(
        workspace.project_root,
        std::fs::canonicalize(dir.path()).unwrap()
    );

    std::env::set_current_dir(previous).unwrap();
}

#[tokio::test]
async fn consultation_selection_skips_existing_responses_and_live_supervisors() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let previous = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();
    store::write_consult_response_for_run_dir(".ferrus/runs/t-001", "existing answer")
        .await
        .unwrap();
    store::write_consult_response_for_run_dir(".ferrus/runs/t-003", "")
        .await
        .unwrap();
    let tasks = vec![
        task_record("t-001", crate::project::TaskStatus::Consultation),
        task_record("t-002", crate::project::TaskStatus::Consultation),
        task_record("t-003", crate::project::TaskStatus::Consultation),
    ];

    let mut live_supervisor_task_ids = HashSet::new();
    live_supervisor_task_ids.insert("t-002".to_string());
    let selected = actionable_consultation_tasks(&tasks, 2, &live_supervisor_task_ids)
        .await
        .unwrap();

    assert_eq!(
        selected
            .iter()
            .map(|task| task.id.as_str())
            .collect::<Vec<_>>(),
        vec!["t-003"]
    );
    std::env::set_current_dir(previous).unwrap();
}
