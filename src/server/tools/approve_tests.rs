use super::*;
use crate::state::store;
use tempfile::TempDir;

async fn setup() -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let previous = std::env::current_dir().unwrap();
    std::fs::create_dir_all(dir.path().join(".ferrus/tasks")).unwrap();
    std::env::set_current_dir(dir.path()).unwrap();
    tokio::fs::write(
        "ferrus.toml",
        "[checks]\ncommands = []\n\n[limits]\nmax_check_retries = 20\nmax_review_cycles = 3\nmax_feedback_lines = 30\nwait_timeout_secs = 1\n\n[lease]\nttl_secs = 60\n",
    )
    .await
    .unwrap();
    let data_dir = dir.path().join(".ferrus/projects/test-project");
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
        vcs: None,
        origin_repo: None,
        default_branch: None,
        current_head: None,
        created_at: "2026-06-22T00:00:00Z".to_string(),
        last_opened_at: "2026-06-22T00:00:00Z".to_string(),
        version: 1,
    };
    tokio::fs::write(
        data_dir.join("project.toml"),
        toml::to_string_pretty(&metadata).unwrap(),
    )
    .await
    .unwrap();
    (dir, previous)
}

fn teardown(previous: std::path::PathBuf) {
    std::env::set_current_dir(previous).unwrap();
}

#[tokio::test]
async fn canonical_approval_lock_is_released_on_drop() {
    let dir = TempDir::new().unwrap();
    let lock_path = dir.path().join("canonical-approval.lock");

    let lock = acquire_canonical_approval_lock_at(&lock_path, "t-007")
        .await
        .unwrap();

    assert!(lock_path.exists());
    let contents = tokio::fs::read_to_string(&lock_path).await.unwrap();
    assert_eq!(
        canonical_approval_lock_pid(&contents),
        Some(std::process::id())
    );
    drop(lock);
    assert!(!lock_path.exists());
}

#[tokio::test]
async fn canonical_approval_lock_replaces_dead_owner() {
    let dir = TempDir::new().unwrap();
    let lock_path = dir.path().join("canonical-approval.lock");
    tokio::fs::write(&lock_path, "pid=2147483647\ntask_id=t-old\n")
        .await
        .unwrap();

    let lock = acquire_canonical_approval_lock_at(&lock_path, "t-007")
        .await
        .unwrap();

    let contents = tokio::fs::read_to_string(&lock_path).await.unwrap();
    assert!(contents.contains("task_id=t-007"));
    drop(lock);
}

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
async fn canonical_approval_lock_replaces_malformed_owner() {
    let dir = TempDir::new().unwrap();
    let lock_path = dir.path().join("canonical-approval.lock");
    tokio::fs::write(&lock_path, "task_id=t-old\n")
        .await
        .unwrap();

    let lock = acquire_canonical_approval_lock_at(&lock_path, "t-007")
        .await
        .unwrap();

    let contents = tokio::fs::read_to_string(&lock_path).await.unwrap();
    assert_eq!(
        canonical_approval_lock_pid(&contents),
        Some(std::process::id())
    );
    assert!(contents.contains("task_id=t-007"));
    drop(lock);
}

#[tokio::test]
async fn canonical_approval_lock_contention_preserves_existing_lock() {
    let dir = TempDir::new().unwrap();
    let lock_path = dir.path().join("canonical-approval.lock");
    let lock = acquire_canonical_approval_lock_at(&lock_path, "t-007")
        .await
        .unwrap();

    let acquired = try_create_canonical_approval_lock(&lock_path, "t-008")
        .await
        .unwrap();

    assert!(acquired.is_none());
    let contents = tokio::fs::read_to_string(&lock_path).await.unwrap();
    assert!(contents.contains("task_id=t-007"));
    assert!(!contents.contains("task_id=t-008"));
    let temp_files = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".canonical-approval.lock.")
                && entry.file_name().to_string_lossy().ends_with(".tmp")
        })
        .count();
    assert_eq!(temp_files, 0);
    drop(lock);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canonical_approval_lock_stale_recovery_admits_one_owner_at_a_time() {
    let dir = TempDir::new().unwrap();
    let lock_path = dir.path().join("canonical-approval.lock");
    tokio::fs::write(&lock_path, "pid=2147483647\ntask_id=t-old\n")
        .await
        .unwrap();

    let first = acquire_canonical_approval_lock_at(&lock_path, "t-007")
        .await
        .unwrap();
    let second_path = lock_path.clone();
    let second =
        tokio::spawn(
            async move { acquire_canonical_approval_lock_at(&second_path, "t-008").await },
        );

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !second.is_finished(),
        "second owner entered while first approval lock was still held"
    );
    let contents = tokio::fs::read_to_string(&lock_path).await.unwrap();
    assert!(contents.contains("task_id=t-007"));

    drop(first);
    let second_lock = tokio::time::timeout(Duration::from_secs(2), second)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let contents = tokio::fs::read_to_string(&lock_path).await.unwrap();
    assert!(contents.contains("task_id=t-008"));
    drop(second_lock);
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
async fn approve_uses_database_context_when_state_json_is_absent() {
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

    let tasks = crate::project::list_tasks().await.unwrap();
    let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
    assert_eq!(task.status, "complete");
    assert_eq!(task.claimed_by, None);

    teardown(previous);
}

#[tokio::test]
async fn approve_applies_scoped_patch_before_marking_task_complete() {
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
    tokio::fs::write(
        "ferrus.toml",
        "[repository_graph]\nenabled = true\n\n[checks]\ncommands = []\n\n[limits]\nmax_check_retries = 20\nmax_review_cycles = 3\nmax_feedback_lines = 30\nwait_timeout_secs = 1\n\n[lease]\nttl_secs = 60\n",
    )
    .await
    .unwrap();

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

    run("supervisor:codex:7").await.unwrap();

    let file = tokio::fs::read_to_string("file.txt").await.unwrap();
    assert_eq!(file.replace("\r\n", "\n"), "new\n");
    let tasks = crate::project::list_tasks().await.unwrap();
    let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
    assert_eq!(task.status, "complete");
    let canonical = crate::project::canonical_graph_reference().await.unwrap();
    assert_eq!(
        canonical.status,
        crate::project::CanonicalGraphStatus::Fresh
    );
    assert!(canonical.source.is_some());
    assert!(canonical.snapshot_id.is_some());

    teardown(previous);
}

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
        tokio::fs::read_to_string("file.txt").await.unwrap(),
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

#[tokio::test]
async fn approve_rolls_back_patch_when_post_apply_checks_fail() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (dir, previous) = setup().await;
    tokio::fs::write(
        "ferrus.toml",
        "[repository_graph]\nenabled = true\n\n[checks]\ncommands = [\"printf mutated > file.txt; printf generated > generated.txt; git add file.txt generated.txt; exit 1\"]\n\n[limits]\nmax_check_retries = 20\nmax_review_cycles = 3\nmax_feedback_lines = 30\nwait_timeout_secs = 1\n\n[lease]\nttl_secs = 60\n",
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
    assert!(integration_error.contains("printf mutated > file.txt"));
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

fn git<const N: usize>(cwd: &std::path::Path, args: [&str; N]) -> std::process::ExitStatus {
    std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .status()
        .unwrap()
}

fn git_path<const N: usize, const M: usize>(
    cwd: &std::path::Path,
    before: [&str; N],
    path: &std::path::Path,
    after: [&str; M],
) -> std::process::ExitStatus {
    std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(before)
        .arg(path)
        .args(after)
        .status()
        .unwrap()
}

fn git_output<const N: usize>(cwd: &std::path::Path, args: [&str; N]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn git_output_bytes<const N: usize>(cwd: &std::path::Path, args: [&str; N]) -> Vec<u8> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success());
    output.stdout
}
