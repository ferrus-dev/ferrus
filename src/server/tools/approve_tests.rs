//! Approval tests for canonical integration, rollback, workspace preservation, and lock ownership.

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

async fn prepare_frozen_review(
    root: &std::path::Path,
    baseline_tree: &str,
    submitted_tree: &str,
    patch: &str,
) {
    crate::project::pin_executor_baseline_tree(root, "t-007", baseline_tree)
        .await
        .unwrap();
    let submitted_tree =
        crate::repository_graph::source::parse_git_tree_digest(submitted_tree).unwrap();
    crate::repository_graph::source::pin_submitted_tree(root, "t-007", &submitted_tree).unwrap();
    store::write_patch_for_run_dir(".ferrus/runs/t-007", patch)
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
}

fn teardown(previous: std::path::PathBuf) {
    std::env::set_current_dir(previous).unwrap();
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

#[path = "approve_tests/conflicts.rs"]
mod conflicts_tests;
#[path = "approve_tests/review_behavior.rs"]
mod review_behavior_tests;
#[path = "approve_tests/review_checks.rs"]
mod review_checks_tests;
#[path = "approve_tests/review_workspaces.rs"]
mod review_workspaces_tests;

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canonical_approval_guard_contention_is_cancellable() {
    let dir = TempDir::new().unwrap();
    let lock_path = dir.path().join("canonical-approval.lock");
    let guard = acquire_canonical_approval_lock_guard(&lock_path)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            acquire_canonical_approval_lock_at(&lock_path, "direct-run"),
        )
        .await
        .is_err()
    );
    assert!(!lock_path.exists());
    drop(guard);
    let lock = tokio::time::timeout(
        Duration::from_secs(5),
        acquire_canonical_approval_lock_at(&lock_path, "t-review"),
    )
    .await
    .unwrap()
    .unwrap();
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
async fn approve_rejects_submodule_gitlink_changes_without_completing_task() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (dir, previous) = setup().await;
    if !git(dir.path(), ["init"]).success() {
        teardown(previous);
        return;
    }

    tokio::fs::write("file.txt", "baseline\n").await.unwrap();
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

    let gitlink_target = git_output(dir.path(), ["rev-parse", "HEAD"])
        .trim()
        .to_string();
    let cache_info = format!("160000,{gitlink_target},dependency");
    assert!(
        git(
            dir.path(),
            ["update-index", "--add", "--cacheinfo", &cache_info],
        )
        .success()
    );
    let submitted_tree = git_output(dir.path(), ["write-tree"]).trim().to_string();
    let patch = git_output(dir.path(), ["diff", "--cached", "--binary", "HEAD"]);
    assert!(!patch.trim().is_empty());
    assert!(git(dir.path(), ["reset", "--mixed", "HEAD"]).success());
    prepare_frozen_review(dir.path(), &baseline_tree, &submitted_tree, &patch).await;

    let error = run("supervisor:codex:7").await.unwrap_err().to_string();

    assert!(error.contains("change submodules are not supported"));
    assert!(!dir.path().join("dependency").exists());
    assert_eq!(git_output(dir.path(), ["write-tree"]).trim(), baseline_tree);
    let integration_error = store::read_integration_error_for_run_dir(".ferrus/runs/t-007")
        .await
        .unwrap();
    assert!(integration_error.contains("change submodules are not supported"));
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
async fn canonical_approval_lock_preserves_a_live_external_owner() {
    let dir = TempDir::new().unwrap();
    let lock_path = dir.path().join("canonical-approval.lock");
    let owner_path = canonical_approval_lock_owner_path(&lock_path);
    let contents = "pid=2147483647\ntask_id=t-external\n";
    std::fs::write(&lock_path, contents).unwrap();
    let owner = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&owner_path)
        .unwrap();
    owner.lock_exclusive().unwrap();
    // Bypass the in-process owner registry, as another Ferrus process would.
    assert!(
        !remove_stale_canonical_approval_lock(&lock_path)
            .await
            .unwrap()
    );
    assert_eq!(std::fs::read_to_string(&lock_path).unwrap(), contents);
    assert!(owner_path.exists());
    drop(owner);
    let lock = acquire_canonical_approval_lock_at(&lock_path, "t-next")
        .await
        .unwrap();
    drop(lock);
    assert!(!lock_path.exists());
}

#[tokio::test]
async fn approve_rollback_preserves_files_ignored_before_integration() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (dir, previous) = setup().await;
    if !git(dir.path(), ["init"]).success() {
        teardown(previous);
        return;
    }

    assert!(git(dir.path(), ["config", "core.autocrlf", "false"]).success());
    tokio::fs::write(
        "ferrus.toml",
        "[checks]\ncommands = [\"git grep -q secret.txt -- .gitignore\"]\n\n[limits]\nmax_check_retries = 20\nmax_review_cycles = 3\nmax_feedback_lines = 30\nwait_timeout_secs = 1\n\n[lease]\nttl_secs = 60\n",
    )
    .await
    .unwrap();
    tokio::fs::write(".gitignore", "secret.txt\n")
        .await
        .unwrap();
    tokio::fs::write("file.txt", "baseline\n").await.unwrap();
    assert!(git(dir.path(), ["add", "ferrus.toml", ".gitignore", "file.txt"]).success());
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

    tokio::fs::write(".gitignore", "").await.unwrap();
    assert!(git(dir.path(), ["add", ".gitignore"]).success());
    let submitted_tree = git_output(dir.path(), ["write-tree"]).trim().to_string();
    let patch = git_output(dir.path(), ["diff", "--cached", "--binary", "HEAD"]);
    assert!(!patch.trim().is_empty());
    assert!(git(dir.path(), ["reset", "--mixed", "HEAD"]).success());
    tokio::fs::write(".gitignore", "secret.txt\n")
        .await
        .unwrap();
    let ignored_contents = b"canonical ignored bytes\0\xff";
    tokio::fs::write("secret.txt", ignored_contents)
        .await
        .unwrap();
    let worktree_before =
        crate::repository_graph::source::capture_worktree_tree(dir.path()).unwrap();
    prepare_frozen_review(dir.path(), &baseline_tree, &submitted_tree, &patch).await;

    let error = run("supervisor:codex:7").await.unwrap_err().to_string();

    assert!(error.contains("configured checks failed"), "{error}");
    assert!(error.contains("rolled back"), "{error}");
    assert_eq!(
        tokio::fs::read("secret.txt").await.unwrap(),
        ignored_contents
    );
    assert_eq!(
        tokio::fs::read_to_string(".gitignore").await.unwrap(),
        "secret.txt\n"
    );
    assert_eq!(git_output(dir.path(), ["write-tree"]).trim(), baseline_tree);
    assert_eq!(
        crate::repository_graph::source::capture_worktree_tree(dir.path()).unwrap(),
        worktree_before
    );
    let task = crate::project::list_tasks()
        .await
        .unwrap()
        .into_iter()
        .find(|task| task.id == "t-007")
        .unwrap();
    assert_eq!(task.status, "reviewing");

    teardown(previous);
}

#[test]
fn approval_git_commands_do_not_inherit_an_open_control_pipe() {
    use std::{io::Write, time::Instant};
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    const CHILD: &str = "FERRUS_TEST_APPROVAL_GIT_STDIN";
    if std::env::var_os(CHILD).is_some() {
        let directory = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new("git")
                .arg("init")
                .arg("--quiet")
                .arg(directory.path())
                .output()
                .unwrap()
                .status
                .success()
        );
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            // Batch mode must see EOF even while the host control pipe stays open.
            let output = tokio::time::timeout(
                Duration::from_secs(5),
                approval_git_command()
                    .arg("-C")
                    .arg(directory.path())
                    .args(["cat-file", "--batch"])
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .expect("approval Git inherited the host control pipe")
            .unwrap();
            assert!(output.status.success());
            assert!(output.stdout.is_empty());
        });
        return;
    }

    let log = tempfile::NamedTempFile::new().unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "server::tools::approve::tests::approval_git_commands_do_not_inherit_an_open_control_pipe", "--nocapture"])
        .env(CHILD, "1")
        .stdin(Stdio::piped())
        .stdout(log.reopen().unwrap())
        .stderr(log.reopen().unwrap())
        .spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    writeln!(input, "host control frame").unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                status.success(),
                "{}",
                std::fs::read_to_string(log.path()).unwrap()
            );
            break;
        }

        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("approval Git did not finish with the host pipe open");
        }

        std::thread::sleep(Duration::from_millis(10));
    }

    drop(input);
}
