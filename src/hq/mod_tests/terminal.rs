//! Tests for terminal.

use super::*;

#[tokio::test]
async fn reset_marks_non_terminal_database_tasks_without_state_json() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let previous = std::env::current_dir().unwrap();
    std::fs::create_dir_all(dir.path().join(".ferrus")).unwrap();
    std::env::set_current_dir(dir.path()).unwrap();

    let data_dir = dir.path().join("runtime");
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
    tokio::fs::write(
        data_dir.join("project.toml"),
        toml::to_string_pretty(&crate::project::ProjectMetadata {
            id: "test-project".to_string(),
            name: "test".to_string(),
            workspace_dir: dir.path().to_string_lossy().into_owned(),
            ferrus_dir: dir.path().join(".ferrus").to_string_lossy().into_owned(),
            vcs: Some("git".to_string()),
            origin_repo: None,
            default_branch: None,
            current_head: None,
            created_at: "2026-07-26T00:00:00Z".to_string(),
            last_opened_at: "2026-07-26T00:00:00Z".to_string(),
            version: 1,
        })
        .unwrap(),
    )
    .await
    .unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .status()
            .unwrap()
            .success()
    );
    tokio::fs::write("seed.txt", "submitted content\n")
        .await
        .unwrap();
    let submitted_tree =
        crate::repository_graph::source::capture_worktree_tree(dir.path()).unwrap();
    crate::repository_graph::source::pin_submitted_tree(dir.path(), "t-004", &submitted_tree)
        .unwrap();
    crate::project::record_task_status(
        "t-001",
        ".ferrus/tasks/t-001.md",
        crate::project::TaskStatus::Pending,
    )
    .await
    .unwrap();
    crate::project::record_task_status(
        "t-002",
        ".ferrus/tasks/t-002.md",
        crate::project::TaskStatus::Executing,
    )
    .await
    .unwrap();
    crate::project::record_task_status(
        "t-003",
        ".ferrus/tasks/t-003.md",
        crate::project::TaskStatus::Complete,
    )
    .await
    .unwrap();
    crate::project::record_task_status(
        "t-004",
        ".ferrus/tasks/t-004.md",
        crate::project::TaskStatus::Reviewing,
    )
    .await
    .unwrap();
    tokio::fs::create_dir_all(".ferrus/tasks").await.unwrap();
    tokio::fs::create_dir_all(".ferrus/runs/t-001")
        .await
        .unwrap();
    tokio::fs::create_dir_all(".ferrus/runs/t-002")
        .await
        .unwrap();
    tokio::fs::create_dir_all(".ferrus/runs/t-003")
        .await
        .unwrap();
    tokio::fs::create_dir_all(".ferrus/runs/t-004")
        .await
        .unwrap();
    tokio::fs::write(".ferrus/tasks/t-001.md", "pending task")
        .await
        .unwrap();
    tokio::fs::write(".ferrus/tasks/t-002.md", "executing task")
        .await
        .unwrap();
    tokio::fs::write(".ferrus/tasks/t-003.md", "complete task")
        .await
        .unwrap();
    tokio::fs::write(".ferrus/tasks/t-004.md", "reviewing task")
        .await
        .unwrap();
    tokio::fs::write(".ferrus/runs/t-001/QUESTION.md", "stale question")
        .await
        .unwrap();
    tokio::fs::write(".ferrus/runs/t-002/CONSULT_REQUEST.md", "stale consult")
        .await
        .unwrap();
    tokio::fs::write(".ferrus/runs/t-003/SUBMISSION.md", "complete submission")
        .await
        .unwrap();
    tokio::fs::write(".ferrus/runs/t-004/SUBMISSION.md", "abandoned submission")
        .await
        .unwrap();
    let pinned_review_refs = std::process::Command::new("git")
        .args(["for-each-ref", "--format=%(refname)", "refs/ferrus/reviews"])
        .output()
        .unwrap();
    assert!(pinned_review_refs.status.success());
    assert_eq!(
        String::from_utf8_lossy(&pinned_review_refs.stdout)
            .lines()
            .count(),
        1
    );

    let (_state_tx, state_rx) = watch::channel::<Option<WatchedState>>(None);
    let (msg_tx, _msg_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut ctx = HqContext::new(state_rx, Display(msg_tx), false);
    ctx.do_reset(false).await.unwrap();

    crate::test_support::assert_no_state_json();
    let tasks = crate::project::list_tasks().await.unwrap();
    let status = |id: &str| {
        tasks
            .iter()
            .find(|task| task.id == id)
            .map(|task| task.status.as_str())
    };
    assert_eq!(status("t-001"), Some("reset"));
    assert_eq!(status("t-002"), Some("reset"));
    assert_eq!(status("t-003"), Some("complete"));
    assert_eq!(status("t-004"), Some("reset"));
    assert!(!std::path::Path::new(".ferrus/tasks/t-001.md").exists());
    assert!(!std::path::Path::new(".ferrus/tasks/t-002.md").exists());
    assert!(std::path::Path::new(".ferrus/tasks/t-003.md").exists());
    assert!(!std::path::Path::new(".ferrus/tasks/t-004.md").exists());
    assert!(!std::path::Path::new(".ferrus/runs/t-001").exists());
    assert!(!std::path::Path::new(".ferrus/runs/t-002").exists());
    assert!(std::path::Path::new(".ferrus/runs/t-003/SUBMISSION.md").exists());
    assert!(!std::path::Path::new(".ferrus/runs/t-004").exists());
    let review_refs = std::process::Command::new("git")
        .args(["for-each-ref", "--format=%(refname)", "refs/ferrus/reviews"])
        .output()
        .unwrap();
    assert!(review_refs.status.success());
    assert!(review_refs.stdout.is_empty());

    crate::repository_graph::source::pin_submitted_tree(
        dir.path(),
        "t-failed-reset",
        &submitted_tree,
    )
    .unwrap();
    crate::project::record_task_status(
        "t-failed-reset",
        ".ferrus/tasks/t-failed-reset",
        crate::project::TaskStatus::Reviewing,
    )
    .await
    .unwrap();
    tokio::fs::create_dir_all(".ferrus/tasks/t-failed-reset")
        .await
        .unwrap();

    ctx.do_reset(false).await.unwrap_err();

    let failed_reset_task = crate::project::list_tasks()
        .await
        .unwrap()
        .into_iter()
        .find(|task| task.id == "t-failed-reset")
        .unwrap();
    assert_eq!(
        failed_reset_task.status,
        crate::project::TaskStatus::Reviewing.as_str()
    );
    let retained_review_refs = std::process::Command::new("git")
        .args(["for-each-ref", "--format=%(refname)", "refs/ferrus/reviews"])
        .output()
        .unwrap();
    assert!(retained_review_refs.status.success());
    assert_eq!(
        String::from_utf8_lossy(&retained_review_refs.stdout)
            .lines()
            .count(),
        1
    );

    std::env::set_current_dir(previous).unwrap();
}

#[test]
fn occupied_executor_slots_merge_live_db_runs_and_headless_handles() {
    let live_db_task_ids = HashSet::from(["t-001".to_string(), "t-003".to_string()]);
    let slots = occupied_executor_slots_from_handles(
        live_db_task_ids,
        [
            "executor:codex:t-001",
            "executor:codex:t-002",
            "executor:codex:1",
        ],
    );

    assert_eq!(slots, 4);
    assert_eq!(
        task_id_from_scoped_agent_name("executor:codex:t-004"),
        Some("t-004")
    );
    assert_eq!(task_id_from_scoped_agent_name("executor:codex:1"), None);
    assert_eq!(
        task_id_from_scoped_agent_name("supervisor:codex:t-004"),
        None
    );
}
