use super::*;
use tempfile::TempDir;

#[test]
fn tracked_pathspec_batches_keeps_short_lists_in_one_batch() {
    let tracked = vec!["a.txt".to_string(), "b.txt".to_string()];
    let batches = tracked_pathspec_batches(&tracked);
    assert_eq!(batches, vec![tracked.as_slice()]);
}

#[test]
fn tracked_pathspec_batches_splits_when_argv_budget_exceeded() {
    let path = "x".repeat(1024);
    let count = (PATHSPEC_ARGV_BUDGET / (path.len() + 1)) + 5;
    let tracked = vec![path; count];
    let batches = tracked_pathspec_batches(&tracked);
    assert!(batches.len() > 1, "expected multiple batches");
    assert_eq!(
        batches.iter().map(|batch| batch.len()).sum::<usize>(),
        count,
        "every path must appear exactly once across batches"
    );
    for batch in &batches {
        assert!(!batch.is_empty(), "batches must be non-empty");
    }
}

#[test]
fn tracked_pathspec_batches_keeps_oversized_single_path() {
    let tracked = vec!["y".repeat(PATHSPEC_ARGV_BUDGET * 2)];
    let batches = tracked_pathspec_batches(&tracked);
    assert_eq!(batches, vec![tracked.as_slice()]);
}

#[test]
fn abandoned_submission_releases_its_tree_pin() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    assert!(
        std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .arg("init")
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(root.join("submitted.rs"), "pub struct Submitted;\n").unwrap();
    let tree = crate::repository_graph::source::capture_worktree_tree(root).unwrap();
    crate::repository_graph::source::pin_submitted_tree(root, "t-abandoned", &tree).unwrap();
    let references = git_output(root, ["show-ref"]);
    let reference = references
        .split_ascii_whitespace()
        .find(|value| value.starts_with("refs/ferrus/reviews/"))
        .unwrap()
        .to_string();

    drop(SubmittedTreePinCleanup {
        workspace_root: Some(root.to_path_buf()),
        task_id: "t-abandoned".to_string(),
        armed: true,
    });

    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["show-ref", "--verify", "--quiet", &reference])
        .output()
        .unwrap();
    assert!(!output.status.success());
}

#[tokio::test]
async fn frozen_tree_patch_ignores_later_worktree_changes() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    assert!(
        std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .arg("init")
            .arg("--quiet")
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(root.join("submitted.rs"), "pub struct Baseline;\n").unwrap();
    let baseline = crate::repository_graph::source::capture_worktree_tree(root).unwrap();
    std::fs::write(root.join("submitted.rs"), "pub struct Submitted;\n").unwrap();
    let submitted = crate::repository_graph::source::capture_worktree_tree(root).unwrap();
    std::fs::write(root.join("submitted.rs"), "pub struct LaterEdit;\n").unwrap();

    let patch = tree_patch_between(root, &baseline, &submitted)
        .await
        .unwrap();

    assert!(patch.contains("+pub struct Submitted;"));
    assert!(!patch.contains("LaterEdit"));
}

async fn setup() -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let previous = std::env::current_dir().unwrap();
    std::fs::create_dir_all(dir.path().join(".ferrus")).unwrap();
    std::env::set_current_dir(dir.path()).unwrap();
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
    tokio::fs::write(
        "ferrus.toml",
        "[checks]\ncommands = []\n\n[limits]\nmax_check_retries = 20\nmax_review_cycles = 3\nmax_feedback_lines = 30\nwait_timeout_secs = 60\n",
    )
    .await
    .unwrap();
    (dir, previous)
}

fn teardown(previous: std::path::PathBuf) {
    std::env::set_current_dir(previous).unwrap();
}

#[tokio::test]
async fn workspace_patch_excludes_seeded_baseline_changes() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let previous = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();

    std::process::Command::new("git")
        .args(["init"])
        .status()
        .unwrap();
    tokio::fs::write("tracked.txt", "base\n").await.unwrap();
    std::process::Command::new("git")
        .args(["add", "tracked.txt"])
        .status()
        .unwrap();
    let commit_status = std::process::Command::new("git")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=Test User",
            "commit",
            "-m",
            "initial",
        ])
        .status()
        .unwrap();
    assert!(commit_status.success());

    tokio::fs::write("tracked.txt", "base\napproved\n")
        .await
        .unwrap();
    tokio::fs::write("seeded.txt", "seeded canonical file\n")
        .await
        .unwrap();
    std::process::Command::new("git")
        .args(["add", "-A", "."])
        .status()
        .unwrap();
    let baseline = std::process::Command::new("git")
        .arg("write-tree")
        .output()
        .unwrap();
    assert!(baseline.status.success());
    let baseline = String::from_utf8_lossy(&baseline.stdout).trim().to_string();
    std::process::Command::new("git")
        .args(["read-tree", "HEAD"])
        .status()
        .unwrap();

    tokio::fs::write("tracked.txt", "base\napproved\ncurrent\n")
        .await
        .unwrap();
    tokio::fs::write("current.txt", "current task file\n")
        .await
        .unwrap();

    let patch = workspace_patch_against_baseline(&baseline).await.unwrap();

    assert!(patch.contains("+current"));
    assert!(patch.contains("current.txt"));
    assert!(!patch.contains("seeded.txt"));
    assert!(!patch.contains("+approved"));

    teardown(previous);
}

#[tokio::test]
async fn workspace_patch_includes_untracked_greenfield_files_without_mutating_index() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let previous = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();

    std::process::Command::new("git")
        .args(["init"])
        .status()
        .unwrap();
    let empty_index = dir.path().join(".git/empty-index");
    assert!(git_env(dir.path(), &empty_index, ["read-tree", "--empty"]).success());
    let baseline = git_env_output(dir.path(), &empty_index, ["write-tree"]);
    std::fs::remove_file(&empty_index).unwrap();

    tokio::fs::create_dir_all("src").await.unwrap();
    tokio::fs::create_dir_all("target/debug").await.unwrap();
    tokio::fs::write(".gitignore", ".ferrus/\n/target\n**/*.rs.bk\n")
        .await
        .unwrap();
    tokio::fs::write("Cargo.toml", "[package]\nname = \"demo\"\n")
        .await
        .unwrap();
    tokio::fs::write("src/main.rs", "fn main() {}\n")
        .await
        .unwrap();
    tokio::fs::write("target/debug/ignored", "ignored\n")
        .await
        .unwrap();

    let patch = workspace_patch_against_baseline(baseline.trim())
        .await
        .unwrap();

    assert!(patch.contains("diff --git a/.gitignore b/.gitignore"));
    assert!(patch.contains("+/target"));
    assert!(patch.contains("diff --git a/Cargo.toml b/Cargo.toml"));
    assert!(patch.contains("diff --git a/src/main.rs b/src/main.rs"));
    assert!(!patch.contains("target/debug/ignored"));
    assert_eq!(git_output(dir.path(), ["ls-files", "--stage"]), "");

    teardown(previous);
}

#[tokio::test]
async fn workspace_patch_uses_stored_baseline_when_env_is_absent() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let previous = std::env::current_dir().unwrap();
    let _env_guard = EnvVarGuard::remove(ENV_BASELINE_TREE);
    std::env::set_current_dir(dir.path()).unwrap();

    std::process::Command::new("git")
        .args(["init"])
        .status()
        .unwrap();
    let data_dir = dir.path().join("runtime");
    tokio::fs::create_dir_all(&data_dir).await.unwrap();
    tokio::fs::create_dir_all(".ferrus").await.unwrap();
    let local_ref = project::LocalProjectRef {
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
    let empty_index = dir.path().join(".git/empty-index");
    assert!(git_env(dir.path(), &empty_index, ["read-tree", "--empty"]).success());
    let baseline = git_env_output(dir.path(), &empty_index, ["write-tree"]);
    std::fs::remove_file(&empty_index).unwrap();
    let baseline_path = data_dir.join("worktrees/.baseline-trees/t-test.txt");
    tokio::fs::create_dir_all(baseline_path.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(&baseline_path, &baseline).await.unwrap();

    tokio::fs::write("Cargo.toml", "[package]\nname = \"demo\"\n")
        .await
        .unwrap();

    let context = runtime_context_with_workspace(dir.path());
    let patch = workspace_patch(&context).await.unwrap();

    assert!(patch.contains("diff --git a/Cargo.toml b/Cargo.toml"));
    assert!(patch.contains("+name = \"demo\""));
    assert_eq!(git_output(dir.path(), ["ls-files", "--stage"]), "");

    teardown(previous);
}

#[tokio::test]
async fn workspace_patch_includes_changes_to_seeded_untracked_files() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let previous = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();

    std::process::Command::new("git")
        .args(["init"])
        .status()
        .unwrap();
    tokio::fs::write(".gitignore", ".ferrus/\n").await.unwrap();
    std::process::Command::new("git")
        .args(["add", "-A", "."])
        .status()
        .unwrap();
    let baseline = std::process::Command::new("git")
        .arg("write-tree")
        .output()
        .unwrap();
    assert!(baseline.status.success());
    let baseline = String::from_utf8_lossy(&baseline.stdout).trim().to_string();
    std::process::Command::new("git")
        .args(["read-tree", "--empty"])
        .status()
        .unwrap();
    tokio::fs::write(".gitignore", ".ferrus/\n/target\n**/*.rs.bk\n")
        .await
        .unwrap();

    let patch = workspace_patch_against_baseline(&baseline).await.unwrap();

    assert!(patch.contains("diff --git a/.gitignore b/.gitignore"));
    assert!(patch.contains("+/target"));
    assert!(patch.contains("+**/*.rs.bk"));
    assert_eq!(git_output(dir.path(), ["ls-files", "--stage"]), "");

    teardown(previous);
}

#[tokio::test]
async fn isolated_workspace_detection_falls_back_to_project_metadata() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let previous = std::env::current_dir().unwrap();
    let canonical = dir.path().join("canonical");
    let worktree = dir.path().join("worktree");
    let data_dir = dir.path().join("runtime");
    tokio::fs::create_dir_all(canonical.join(".ferrus"))
        .await
        .unwrap();
    tokio::fs::create_dir_all(worktree.join(".ferrus"))
        .await
        .unwrap();
    tokio::fs::create_dir_all(&data_dir).await.unwrap();
    let local_ref = project::LocalProjectRef {
        project_id: "test-project".to_string(),
        name: "test".to_string(),
        data_dir: data_dir.to_string_lossy().into_owned(),
    };
    tokio::fs::write(
        worktree.join(".ferrus/project.toml"),
        toml::to_string_pretty(&local_ref).unwrap(),
    )
    .await
    .unwrap();
    let metadata = project::ProjectMetadata {
        id: "test-project".to_string(),
        name: "test".to_string(),
        workspace_dir: canonical.to_string_lossy().into_owned(),
        ferrus_dir: canonical.join(".ferrus").to_string_lossy().into_owned(),
        vcs: Some("git".to_string()),
        origin_repo: None,
        default_branch: None,
        current_head: None,
        created_at: "2026-01-01T00:00:00Z".to_string(),
        last_opened_at: "2026-01-01T00:00:00Z".to_string(),
        version: 1,
    };
    tokio::fs::write(
        data_dir.join("project.toml"),
        toml::to_string_pretty(&metadata).unwrap(),
    )
    .await
    .unwrap();
    std::env::set_current_dir(&worktree).unwrap();

    let context = runtime_context_with_workspace(&worktree);

    assert!(is_isolated_executor_workspace(&context).await);

    teardown(previous);
}

#[tokio::test]
async fn submit_reclaims_expired_same_agent_lease_before_guarding() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup().await;
    crate::project::record_task_status(
        "t-001",
        ".ferrus/tasks/t-001.md",
        crate::project::TaskStatus::Executing,
    )
    .await
    .unwrap();
    crate::project::claim_task("t-001", ".ferrus/tasks/t-001.md", "executor:codex:1", 0)
        .await
        .unwrap();

    run(
        Some("executor:codex:1"),
        "## Summary\nDone.\n\n## How to verify manually\nInspect it.\n".to_string(),
    )
    .await
    .unwrap();

    let tasks = crate::project::list_tasks().await.unwrap();
    let task = tasks.iter().find(|task| task.id == "t-001").unwrap();
    assert_eq!(task.status, "reviewing");
    assert_eq!(task.claimed_by, None);
    assert_eq!(
        tokio::fs::read_to_string(".ferrus/runs/t-001/SUBMISSION.md")
            .await
            .unwrap(),
        "## Summary\nDone.\n\n## How to verify manually\nInspect it.\n"
    );

    teardown(previous);
}

#[tokio::test]
async fn submit_pass_clears_database_retry_metadata() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup().await;
    crate::project::record_task_status(
        "t-001",
        ".ferrus/tasks/t-001.md",
        crate::project::TaskStatus::Executing,
    )
    .await
    .unwrap();
    crate::project::record_task_check_failed("t-001", "fmt failed", 2)
        .await
        .unwrap();
    crate::project::claim_task("t-001", ".ferrus/tasks/t-001.md", "executor:codex:1", 60)
        .await
        .unwrap();

    run(
        Some("executor:codex:1"),
        "## Summary\nDone.\n\n## How to verify manually\nInspect it.\n".to_string(),
    )
    .await
    .unwrap();

    crate::test_support::assert_no_state_json();
    let tasks = crate::project::list_tasks().await.unwrap();
    let task = tasks.iter().find(|task| task.id == "t-001").unwrap();
    assert_eq!(task.status, "reviewing");
    assert_eq!(task.check_retries, 0);
    assert_eq!(task.failure_reason, None);
    assert_eq!(task.claimed_by, None);

    teardown(previous);
}

#[tokio::test]
async fn submit_writes_submission_to_agent_runtime_task_context() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup().await;
    crate::project::record_task_status(
        "t-007",
        ".ferrus/tasks/t-007.md",
        crate::project::TaskStatus::Executing,
    )
    .await
    .unwrap();
    crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "executor:codex:7", 60)
        .await
        .unwrap();

    run(
        Some("executor:codex:7"),
        "## Summary\nDone.\n\n## How to verify manually\nInspect it.\n".to_string(),
    )
    .await
    .unwrap();

    assert_eq!(
        tokio::fs::read_to_string(".ferrus/runs/t-007/SUBMISSION.md")
            .await
            .unwrap(),
        "## Summary\nDone.\n\n## How to verify manually\nInspect it.\n"
    );
    let tasks = crate::project::list_tasks().await.unwrap();
    let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
    assert_eq!(task.status, "reviewing");
    assert_eq!(task.check_retries, 0);
    assert_eq!(task.claimed_by, None);
    crate::test_support::assert_no_state_json();

    teardown(previous);
}

#[tokio::test]
async fn submit_clears_stale_integration_error_on_success() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup().await;
    crate::project::record_task_status(
        "t-007",
        ".ferrus/tasks/t-007.md",
        crate::project::TaskStatus::Addressing,
    )
    .await
    .unwrap();
    crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "executor:codex:7", 60)
        .await
        .unwrap();
    store::write_integration_error_for_run_dir(
        ".ferrus/runs/t-007",
        "# Integration Error\n\nold conflict\n",
    )
    .await
    .unwrap();

    run(
        Some("executor:codex:7"),
        "## Summary\nFixed.\n\n## How to verify manually\nInspect it.\n".to_string(),
    )
    .await
    .unwrap();

    assert_eq!(
        store::read_integration_error_for_run_dir(".ferrus/runs/t-007")
            .await
            .unwrap(),
        ""
    );
    let tasks = crate::project::list_tasks().await.unwrap();
    let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
    assert_eq!(task.status, "reviewing");

    teardown(previous);
}

#[tokio::test]
async fn canonical_submit_clears_stale_isolated_patch() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup().await;
    crate::project::record_task_status(
        "t-007",
        ".ferrus/tasks/t-007.md",
        crate::project::TaskStatus::Addressing,
    )
    .await
    .unwrap();
    crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "executor:codex:7", 60)
        .await
        .unwrap();
    store::write_patch_for_run_dir(
        ".ferrus/runs/t-007",
        "diff --git a/old.txt b/old.txt\n+stale\n",
    )
    .await
    .unwrap();

    run(
        Some("executor:codex:7"),
        "## Summary\nFixed in canonical workspace.\n\n## How to verify manually\nInspect it.\n"
            .to_string(),
    )
    .await
    .unwrap();

    assert_eq!(
        store::read_patch_for_run_dir(".ferrus/runs/t-007")
            .await
            .unwrap(),
        ""
    );
    let tasks = crate::project::list_tasks().await.unwrap();
    let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
    assert_eq!(task.status, "reviewing");

    teardown(previous);
}

#[tokio::test]
async fn submit_uses_database_context_when_state_json_is_absent() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup().await;
    crate::project::record_task_status(
        "t-007",
        ".ferrus/tasks/t-007.md",
        crate::project::TaskStatus::Executing,
    )
    .await
    .unwrap();
    crate::project::record_task_check_failed("t-007", "fmt failed", 2)
        .await
        .unwrap();
    crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "executor:codex:7", 60)
        .await
        .unwrap();

    run(
        Some("executor:codex:7"),
        "## Summary\nDone.\n\n## How to verify manually\nInspect it.\n".to_string(),
    )
    .await
    .unwrap();

    crate::test_support::assert_no_state_json();
    assert_eq!(
        tokio::fs::read_to_string(".ferrus/runs/t-007/SUBMISSION.md")
            .await
            .unwrap(),
        "## Summary\nDone.\n\n## How to verify manually\nInspect it.\n"
    );
    let tasks = crate::project::list_tasks().await.unwrap();
    let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
    assert_eq!(task.status, "reviewing");
    assert_eq!(task.check_retries, 0);
    assert_eq!(task.failure_reason, None);
    assert_eq!(task.claimed_by, None);

    teardown(previous);
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

fn git_env<const N: usize>(
    cwd: &std::path::Path,
    index: &std::path::Path,
    args: [&str; N],
) -> std::process::ExitStatus {
    std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .env("GIT_INDEX_FILE", index)
        .args(args)
        .status()
        .unwrap()
}

fn git_env_output<const N: usize>(
    cwd: &std::path::Path,
    index: &std::path::Path,
    args: [&str; N],
) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .env("GIT_INDEX_FILE", index)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn runtime_context_with_workspace(workspace: &std::path::Path) -> RuntimeTaskContext {
    RuntimeTaskContext {
        task_id: "t-test".to_string(),
        task_path: ".ferrus/tasks/t-test.md".to_string(),
        spec_path: None,
        milestone_id: None,
        run_dir: ".ferrus/runs/t-test".to_string(),
        status: project::TaskStatus::Executing.as_str().to_string(),
        paused_status: None,
        check_retries: 0,
        review_cycles: 0,
        failure_reason: None,
        run_id: None,
        run_role: Some("executor".to_string()),
        workspace_path: Some(workspace.to_string_lossy().into_owned()),
        repository_workspace_path: Some(workspace.to_string_lossy().into_owned()),
        repository_view: project::RepositoryViewReference::default(),
    }
}

struct EnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn remove(key: &'static str) -> Self {
        let previous = std::env::var_os(key);
        unsafe {
            std::env::remove_var(key);
        }
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        unsafe {
            match self.previous.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}
