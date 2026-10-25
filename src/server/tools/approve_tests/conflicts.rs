//! Tests for conflicts.

use super::*;

#[tokio::test]
async fn approve_preserves_ignored_file_that_conflicts_with_submitted_addition() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (dir, previous) = setup().await;
    if !git(dir.path(), ["init"]).success() {
        teardown(previous);
        return;
    }

    tokio::fs::write(".gitignore", "ignored.txt\n")
        .await
        .unwrap();
    tokio::fs::write("file.txt", "baseline\n").await.unwrap();
    assert!(git(dir.path(), ["add", ".gitignore", "file.txt"]).success());
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
                "baseline",
            ],
        )
        .success()
    );
    let baseline_tree = git_output(dir.path(), ["rev-parse", "HEAD^{tree}"])
        .trim()
        .to_string();

    tokio::fs::write("ignored.txt", "submitted\n")
        .await
        .unwrap();
    assert!(git(dir.path(), ["add", "-f", "ignored.txt"]).success());
    let submitted_tree = git_output(dir.path(), ["write-tree"]).trim().to_string();
    let patch = git_output(dir.path(), ["diff", "--cached", "--binary", "HEAD"]);
    assert!(!patch.trim().is_empty());
    assert!(git(dir.path(), ["reset", "--mixed", "HEAD"]).success());
    tokio::fs::write("ignored.txt", "canonical local content\n")
        .await
        .unwrap();
    prepare_frozen_review(dir.path(), &baseline_tree, &submitted_tree, &patch).await;

    let error = run("supervisor:codex:7").await.unwrap_err().to_string();

    assert!(error.contains("ignored local content"), "{error}");
    assert_eq!(
        tokio::fs::read_to_string("ignored.txt").await.unwrap(),
        "canonical local content\n"
    );
    assert_eq!(git_output(dir.path(), ["write-tree"]).trim(), baseline_tree);
    let integration_error = store::read_integration_error_for_run_dir(".ferrus/runs/t-007")
        .await
        .unwrap();
    assert!(integration_error.contains("ignored local content"));
    let task = crate::project::list_tasks()
        .await
        .unwrap()
        .into_iter()
        .find(|task| task.id == "t-007")
        .unwrap();
    assert_eq!(task.status, "reviewing");

    teardown(previous);
}

#[tokio::test]
async fn approve_patch_conflict_records_recoverable_integration_error() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (dir, previous) = setup().await;
    if !git(dir.path(), ["init"]).success() {
        teardown(previous);
        return;
    }

    tokio::fs::write("file.txt", "old\n").await.unwrap();
    tokio::fs::write("deleted.txt", "delete me\n")
        .await
        .unwrap();
    assert!(git(dir.path(), ["add", "file.txt", "deleted.txt"]).success());
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
    tokio::fs::write("file.txt", "conflicting local change\n")
        .await
        .unwrap();
    tokio::fs::write("staged.txt", "staged canonical addition\n")
        .await
        .unwrap();
    assert!(git(dir.path(), ["add", "staged.txt"]).success());
    tokio::fs::remove_file("deleted.txt").await.unwrap();
    tokio::fs::write("untracked.bin", [0_u8, 159, 146, 150])
        .await
        .unwrap();
    assert!(!patch.trim().is_empty());

    let status_before = git_output_bytes(
        dir.path(),
        [
            "status",
            "--porcelain=v2",
            "-z",
            "--",
            "file.txt",
            "staged.txt",
            "deleted.txt",
            "untracked.bin",
        ],
    );
    let index_before = git_output(dir.path(), ["write-tree"]);
    let worktree_before =
        crate::repository_graph::source::capture_worktree_tree(dir.path()).unwrap();

    store::write_patch_for_run_dir(".ferrus/runs/t-007", &patch)
        .await
        .unwrap();
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

    let error = run("supervisor:codex:7").await.unwrap_err().to_string();

    assert!(error.contains("INTEGRATION_ERROR.md"));
    assert!(error.contains("Reject this review"));
    let integration_error = store::read_integration_error_for_run_dir(".ferrus/runs/t-007")
        .await
        .unwrap();
    assert!(integration_error.contains("Cannot approve task t-007"));
    assert!(integration_error.contains("Suggested next step"));
    let tasks = crate::project::list_tasks().await.unwrap();
    let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
    assert_eq!(task.status, "reviewing");
    assert!(
        task.failure_reason
            .as_deref()
            .is_some_and(|reason| { reason.contains("could not be merged") })
    );
    assert_eq!(
        git_output_bytes(
            dir.path(),
            [
                "status",
                "--porcelain=v2",
                "-z",
                "--",
                "file.txt",
                "staged.txt",
                "deleted.txt",
                "untracked.bin",
            ],
        ),
        status_before
    );
    assert_eq!(git_output(dir.path(), ["write-tree"]), index_before);
    assert_eq!(
        crate::repository_graph::source::capture_worktree_tree(dir.path()).unwrap(),
        worktree_before
    );

    teardown(previous);
}
