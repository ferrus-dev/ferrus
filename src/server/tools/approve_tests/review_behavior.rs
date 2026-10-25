//! Tests for review behavior.

use super::*;

#[tokio::test]
async fn canonical_approval_lock_replaces_reused_live_pid_without_inode_lock() {
    let dir = TempDir::new().unwrap();
    let lock_path = dir.path().join("canonical-approval.lock");
    tokio::fs::write(
        &lock_path,
        format!("pid={}\ntask_id=t-old\n", std::process::id()),
    )
    .await
    .unwrap();

    let lock = tokio::time::timeout(
        Duration::from_secs(2),
        acquire_canonical_approval_lock_at(&lock_path, "t-007"),
    )
    .await
    .unwrap()
    .unwrap();

    let contents = tokio::fs::read_to_string(&lock_path).await.unwrap();
    assert!(contents.contains("task_id=t-007"));
    drop(lock);
}

#[tokio::test]
async fn approve_updates_agent_review_task_in_database() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup().await;
    crate::project::record_task_status(
        "t-007",
        ".ferrus/tasks/t-007.md",
        crate::project::TaskStatus::Reviewing,
    )
    .await
    .unwrap();
    crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "supervisor:codex:7", 60)
        .await
        .unwrap();
    run("supervisor:codex:7").await.unwrap();

    crate::test_support::assert_no_state_json();
    let tasks = crate::project::list_tasks().await.unwrap();
    let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
    assert_eq!(task.status, "complete");
    assert_eq!(task.claimed_by, None);

    teardown(previous);
}

#[tokio::test]
async fn approve_keeps_task_reviewing_when_spec_update_fails() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup().await;
    tokio::fs::create_dir_all("docs/specs/spec.md")
        .await
        .unwrap();
    crate::project::record_task_status_with_origin(
        "t-009",
        ".ferrus/tasks/t-009.md",
        crate::project::TaskStatus::Reviewing,
        Some("docs/specs/spec.md"),
        Some("m1.0"),
    )
    .await
    .unwrap();
    crate::project::claim_task("t-009", ".ferrus/tasks/t-009.md", "supervisor:codex:9", 60)
        .await
        .unwrap();

    let error = run("supervisor:codex:9").await.unwrap_err().to_string();

    assert!(
        error.replace('\\', "/").contains("docs/specs/spec.md"),
        "unexpected error: {error}"
    );
    let tasks = crate::project::list_tasks().await.unwrap();
    let task = tasks.iter().find(|task| task.id == "t-009").unwrap();
    assert_eq!(task.status, "reviewing");
    assert_eq!(task.claimed_by.as_deref(), Some("supervisor:codex:9"));

    teardown(previous);
}

#[tokio::test]
async fn approve_rolls_back_patch_when_spec_update_fails() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (dir, previous) = setup().await;
    if !git(dir.path(), ["init"]).success() {
        teardown(previous);
        return;
    }

    tokio::fs::write("file.txt", "old\n").await.unwrap();
    assert!(git(dir.path(), ["add", "file.txt"]).success());
    assert!(
        git(
            dir.path(),
            [
                "-c",
                "user.email=ferrus@example.invalid",
                "-c",
                "user.name=Ferrus",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "initial",
            ],
        )
        .success()
    );
    tokio::fs::write("file.txt", "new\n").await.unwrap();
    let patch = git_output(dir.path(), ["diff", "--binary", "HEAD", "--", "file.txt"]);
    tokio::fs::write("file.txt", "old\n").await.unwrap();
    assert!(!patch.trim().is_empty());
    tokio::fs::create_dir_all("docs/specs/spec.md")
        .await
        .unwrap();

    store::write_patch_for_run_dir(".ferrus/runs/t-010", &patch)
        .await
        .unwrap();
    crate::project::record_task_status_with_origin(
        "t-010",
        ".ferrus/tasks/t-010.md",
        crate::project::TaskStatus::Reviewing,
        Some("docs/specs/spec.md"),
        Some("m1.0"),
    )
    .await
    .unwrap();
    crate::project::claim_task("t-010", ".ferrus/tasks/t-010.md", "supervisor:codex:10", 60)
        .await
        .unwrap();

    let error = run("supervisor:codex:10").await.unwrap_err().to_string();

    assert!(
        error.replace('\\', "/").contains("docs/specs/spec.md"),
        "unexpected error: {error}"
    );
    let file = tokio::fs::read_to_string("file.txt").await.unwrap();
    assert_eq!(file.replace("\r\n", "\n"), "old\n");
    let tasks = crate::project::list_tasks().await.unwrap();
    let task = tasks.iter().find(|task| task.id == "t-010").unwrap();
    assert_eq!(task.status, "reviewing");
    assert_eq!(task.claimed_by.as_deref(), Some("supervisor:codex:10"));

    teardown(previous);
}
