//! Tests for review checks.

use super::*;

#[tokio::test]
async fn approve_rejects_changes_outside_canonical_sparse_checkout() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (dir, previous) = setup().await;
    if !git(dir.path(), ["init"]).success() {
        teardown(previous);
        return;
    }

    tokio::fs::create_dir_all("included").await.unwrap();
    tokio::fs::create_dir_all("skipped").await.unwrap();
    tokio::fs::write("included/file.txt", "included\n")
        .await
        .unwrap();
    tokio::fs::write("skipped/file.txt", "baseline\n")
        .await
        .unwrap();
    assert!(git(dir.path(), ["add", "included", "skipped"]).success());
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

    tokio::fs::write("skipped/file.txt", "submitted\n")
        .await
        .unwrap();
    assert!(git(dir.path(), ["add", "skipped/file.txt"]).success());
    let submitted_tree = git_output(dir.path(), ["write-tree"]).trim().to_string();
    let patch = git_output(dir.path(), ["diff", "--cached", "--binary", "HEAD"]);
    assert!(!patch.trim().is_empty());
    assert!(git(dir.path(), ["reset", "--hard", "HEAD"]).success());
    assert!(
        git(dir.path(), ["sparse-checkout", "set", "included"]).success(),
        "Git must support sparse-checkout check-rules for this test"
    );
    assert!(!dir.path().join("skipped/file.txt").exists());
    prepare_frozen_review(dir.path(), &baseline_tree, &submitted_tree, &patch).await;

    let error = run("supervisor:codex:7").await.unwrap_err().to_string();

    assert!(
        error.contains("outside the canonical sparse checkout"),
        "{error}"
    );
    assert!(!dir.path().join("skipped/file.txt").exists());
    assert_eq!(git_output(dir.path(), ["write-tree"]).trim(), baseline_tree);
    let integration_error = store::read_integration_error_for_run_dir(".ferrus/runs/t-007")
        .await
        .unwrap();
    assert!(integration_error.contains("outside the canonical sparse checkout"));
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
async fn approve_rolls_back_patch_when_post_apply_checks_fail() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (dir, previous) = setup().await;
    tokio::fs::write(
        "ferrus.toml",
        "[repository_graph]\nenabled = true\n\n[checks]\ncommands = [\"git show HEAD:file.txt > generated.txt\", \"git add file.txt generated.txt\", \"git grep -q base -- file.txt\"]\n\n[limits]\nmax_check_retries = 20\nmax_review_cycles = 3\nmax_feedback_lines = 30\nwait_timeout_secs = 1\n\n[lease]\nttl_secs = 60\n",
    )
    .await
    .unwrap();
    if !git(dir.path(), ["init"]).success() {
        teardown(previous);
        return;
    }

    // Keep the rollback byte-exact on Windows too. Otherwise Git's global
    // autocrlf setting can turn the restored LF source into CRLF, correctly
    // making the canonical source manifest stale after the rollback.
    assert!(git(dir.path(), ["config", "core.autocrlf", "false"]).success());
    tokio::fs::write("file.txt", "base\n").await.unwrap();
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
    tokio::fs::write("file.txt", "broken\n").await.unwrap();
    let patch = git_output(dir.path(), ["diff", "--binary", "HEAD", "--", "file.txt"]);
    tokio::fs::write("file.txt", "base\n").await.unwrap();
    assert!(!patch.trim().is_empty());

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
    let worktree_before =
        crate::repository_graph::source::capture_worktree_tree(dir.path()).unwrap();
    let index_before = git_output(dir.path(), ["write-tree"]);

    let error = run("supervisor:codex:7").await.unwrap_err().to_string();

    assert!(error.contains("configured checks failed"));
    assert!(error.contains("rolled back"));
    let file = tokio::fs::read_to_string("file.txt").await.unwrap();
    assert_eq!(file.replace("\r\n", "\n"), "base\n");
    assert!(!dir.path().join("generated.txt").exists());
    assert_eq!(git_output(dir.path(), ["write-tree"]), index_before);
    assert_eq!(
        crate::repository_graph::source::capture_worktree_tree(dir.path()).unwrap(),
        worktree_before
    );
    let integration_error = store::read_integration_error_for_run_dir(".ferrus/runs/t-007")
        .await
        .unwrap();
    assert!(integration_error.contains("configured checks failed"));
    assert!(integration_error.contains("git grep -q base -- file.txt"));
    let tasks = crate::project::list_tasks().await.unwrap();
    let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
    assert_eq!(task.status, "reviewing");
    assert!(
        task.failure_reason
            .as_deref()
            .is_some_and(|reason| { reason.contains("Commands failed") })
    );
    assert_eq!(
        crate::project::canonical_graph_reference()
            .await
            .unwrap()
            .status,
        crate::project::CanonicalGraphStatus::Unknown
    );

    teardown(previous);
}

#[tokio::test]
async fn failed_integration_marks_actual_partial_canonical_manifest_stale() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (dir, previous) = setup().await;
    tokio::fs::write(
        "ferrus.toml",
        "[repository_graph]\nenabled = true\n\n[checks]\ncommands = []\n",
    )
    .await
    .unwrap();
    tokio::fs::write("stable.txt", "stable\n").await.unwrap();
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
    let context = crate::project::runtime_task_context_for_agent("supervisor:codex:7")
        .await
        .unwrap()
        .unwrap();
    let observer = CanonicalIntegrationObserver::capture(dir.path()).await;

    tokio::fs::write("partial.txt", "left behind\n")
        .await
        .unwrap();
    assert!(observer.finish(&context, dir.path(), false).await);

    let canonical = crate::project::canonical_graph_reference().await.unwrap();
    assert_eq!(
        canonical.status,
        crate::project::CanonicalGraphStatus::Stale
    );
    assert!(canonical.source.is_some());
    let task = crate::project::list_tasks()
        .await
        .unwrap()
        .into_iter()
        .find(|task| task.id == "t-007")
        .unwrap();
    assert_eq!(task.status, crate::project::TaskStatus::Reviewing.as_str());

    teardown(previous);
}
