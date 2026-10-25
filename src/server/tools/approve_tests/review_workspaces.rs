//! Tests for review workspaces.

use super::*;

#[tokio::test]
async fn approve_three_way_merges_disjoint_hunks_from_a_pinned_baseline() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (dir, previous) = setup().await;
    if !git(dir.path(), ["init"]).success() {
        teardown(previous);
        return;
    }

    let baseline_content = "first\nsecond\nthird\nfourth\nfifth\nsixth\n";
    tokio::fs::write("file.txt", baseline_content)
        .await
        .unwrap();
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
                "baseline",
            ],
        )
        .success()
    );
    let baseline_tree = git_output(dir.path(), ["rev-parse", "HEAD^{tree}"])
        .trim()
        .to_string();
    crate::project::pin_executor_baseline_tree(dir.path(), "t-007", &baseline_tree)
        .await
        .unwrap();

    tokio::fs::write(
        "file.txt",
        "first\nsecond\nthird\nfourth\nfifth\ntask change\n",
    )
    .await
    .unwrap();
    let submitted_tree =
        crate::repository_graph::source::capture_worktree_tree(dir.path()).unwrap();
    crate::repository_graph::source::pin_submitted_tree(dir.path(), "t-007", &submitted_tree)
        .unwrap();
    let patch = git_output(dir.path(), ["diff", "--binary", "HEAD", "--", "file.txt"]);
    tokio::fs::write(
        "file.txt",
        "canonical change\nsecond\nthird\nfourth\nfifth\nsixth\n",
    )
    .await
    .unwrap();
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
                "canonical advance",
            ],
        )
        .success()
    );

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
    let frozen_view = crate::project::RepositoryViewReference::materialized(
        crate::repository_graph::domain::SnapshotId::new("baseline-snapshot").unwrap(),
        None,
        crate::repository_graph::domain::SnapshotId::new("submitted-snapshot").unwrap(),
        crate::project::RepositoryViewStatus::Available,
    )
    .unwrap()
    .frozen(submitted_tree)
    .unwrap();
    crate::project::record_task_repository_view("t-007", &frozen_view)
        .await
        .unwrap();
    crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "supervisor:codex:7", 60)
        .await
        .unwrap();

    run("supervisor:codex:7").await.unwrap();

    assert_eq!(
        tokio::fs::read_to_string("file.txt")
            .await
            .unwrap()
            .replace("\r\n", "\n"),
        "canonical change\nsecond\nthird\nfourth\nfifth\ntask change\n"
    );

    teardown(previous);
}

#[tokio::test]
async fn approve_from_executor_worktree_updates_and_checks_canonical_workspace() {
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
    store::write_patch_for_run_dir(".ferrus/runs/t-007", &patch)
        .await
        .unwrap();
    tokio::fs::write(
        "ferrus.toml",
        "[checks]\ncommands = [\"git grep -q new -- file.txt\"]\n\n[limits]\nmax_check_retries = 20\nmax_review_cycles = 3\nmax_feedback_lines = 30\nwait_timeout_secs = 1\n\n[lease]\nttl_secs = 60\n",
    )
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

    let review_workspace = dir.path().join("review-worktree");
    assert!(
        git_path(
            dir.path(),
            ["worktree", "add", "--detach"],
            &review_workspace,
            ["HEAD"],
        )
        .success()
    );
    tokio::fs::create_dir_all(review_workspace.join(".ferrus"))
        .await
        .unwrap();
    tokio::fs::copy(
        ".ferrus/project.toml",
        review_workspace.join(".ferrus/project.toml"),
    )
    .await
    .unwrap();
    tokio::fs::copy("ferrus.toml", review_workspace.join("ferrus.toml"))
        .await
        .unwrap();
    std::env::set_current_dir(&review_workspace).unwrap();

    run("supervisor:codex:7").await.unwrap();

    assert_eq!(
        tokio::fs::read_to_string(dir.path().join("file.txt"))
            .await
            .unwrap()
            .replace("\r\n", "\n"),
        "new\n"
    );
    assert_eq!(
        tokio::fs::read_to_string(review_workspace.join("file.txt"))
            .await
            .unwrap()
            .replace("\r\n", "\n"),
        "old\n"
    );
    let task = crate::project::list_tasks()
        .await
        .unwrap()
        .into_iter()
        .find(|task| task.id == "t-007")
        .unwrap();
    assert_eq!(task.status, "complete");

    std::env::set_current_dir(dir.path()).unwrap();
    teardown(previous);
}

#[tokio::test]
async fn approve_removes_managed_worktree_after_completion() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (dir, previous) = setup().await;
    if !git(dir.path(), ["init"]).success() {
        teardown(previous);
        return;
    }

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

    let workspace_path = dir
        .path()
        .join(".ferrus/projects/test-project/worktrees/t-007");
    assert!(
        git_path(
            dir.path(),
            ["worktree", "add", "--detach"],
            &workspace_path,
            ["HEAD"],
        )
        .success()
    );
    assert!(workspace_path.is_dir());
    let baseline_tree = git_output(dir.path(), ["rev-parse", "HEAD^{tree}"])
        .trim()
        .to_string();
    crate::project::pin_executor_baseline_tree(dir.path(), "t-007", &baseline_tree)
        .await
        .unwrap();
    let baseline_metadata = dir
        .path()
        .join(".ferrus/projects/test-project/worktrees/.baseline-trees/t-007.txt");
    tokio::fs::create_dir_all(baseline_metadata.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(&baseline_metadata, format!("{baseline_tree}\n"))
        .await
        .unwrap();

    crate::project::record_task_status(
        "t-007",
        ".ferrus/tasks/t-007.md",
        crate::project::TaskStatus::Reviewing,
    )
    .await
    .unwrap();
    let run_record = crate::project::record_run_started_with_workspace(
        "supervisor-run-t-007",
        "supervisor",
        "supervisor:codex:7",
        std::process::id(),
        dir.path().to_string_lossy().into_owned(),
    )
    .await
    .unwrap();
    let attached = crate::project::attach_running_run_to_task(
        "supervisor:codex:7",
        "t-007",
        ".ferrus/tasks/t-007.md",
    )
    .await
    .unwrap();
    assert_eq!(attached.as_deref(), Some(run_record.id.as_str()));
    crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "supervisor:codex:7", 60)
        .await
        .unwrap();
    let context = crate::project::runtime_task_context_for_agent("supervisor:codex:7")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        context.workspace_path.as_deref(),
        Some(dir.path().to_string_lossy().as_ref())
    );

    run("supervisor:codex:7").await.unwrap();

    assert!(!workspace_path.exists());
    assert!(!baseline_metadata.exists());
    let baseline_ref = std::process::Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["rev-parse", "--verify", "refs/ferrus/baselines/t-007"])
        .output()
        .unwrap();
    assert!(!baseline_ref.status.success());
    let tasks = crate::project::list_tasks().await.unwrap();
    let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
    assert_eq!(task.status, "complete");

    teardown(previous);
}
