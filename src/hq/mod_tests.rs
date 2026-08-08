use super::*;

fn task_record(id: &str, status: crate::project::TaskStatus) -> TaskRecord {
    TaskRecord {
        id: id.to_string(),
        path: format!(".ferrus/tasks/{id}.md"),
        spec_path: None,
        milestone_id: None,
        status: status.as_str().to_string(),
        paused_status: None,
        claimed_by: None,
        lease_until: None,
        last_heartbeat: None,
        check_retries: 0,
        review_cycles: 0,
        failure_reason: None,
    }
}

#[cfg(unix)]
fn failed_exit_status() -> std::process::ExitStatus {
    use std::os::unix::process::ExitStatusExt;

    std::process::ExitStatus::from_raw(1 << 8)
}

#[cfg(windows)]
fn failed_exit_status() -> std::process::ExitStatus {
    use std::os::windows::process::ExitStatusExt;

    std::process::ExitStatus::from_raw(1)
}

#[test]
fn interactive_exit_error_names_role_agent_and_status() {
    let message = interactive_exit_error(
        ROLE_SUPERVISOR,
        "codex",
        failed_exit_status(),
        "broken config",
    );

    assert!(message.contains("supervisor agent (codex) exited with"));
    assert!(message.contains("stderr:\nbroken config"));
}

#[test]
fn format_agent_details_normalizes_version_and_appends_model() {
    assert_eq!(
        format_agent_details(
            "claude-code",
            "2.1.143 (Claude Code)",
            AgentDisplayConfig {
                model: Some("claude-opus-4-6".to_string()),
                effort: Some("high".to_string()),
            }
        ),
        "2.1.143 (claude-opus-4-6, effort: high)"
    );
    assert_eq!(
        format_agent_details(
            "codex",
            "codex-cli 0.132.0",
            AgentDisplayConfig {
                model: Some("gpt-5.4".to_string()),
                effort: None,
            }
        ),
        "0.132.0 (gpt-5.4)"
    );
}

#[test]
fn format_agent_details_omits_missing_parts() {
    assert_eq!(
        format_agent_details("opencode", "opencode 0.6.0", AgentDisplayConfig::default()),
        "0.6.0"
    );
    assert_eq!(
        format_agent_details(
            "goose",
            "",
            AgentDisplayConfig {
                model: Some("qwen3-coder".to_string()),
                effort: Some("medium".to_string()),
            }
        ),
        "(qwen3-coder, effort: medium)"
    );
    assert_eq!(
        format_agent_details(
            "claude-code",
            "",
            AgentDisplayConfig {
                model: None,
                effort: Some("high".to_string()),
            }
        ),
        "(effort: high)"
    );
    assert_eq!(
        format_agent_details("qwen-code", "", AgentDisplayConfig::default()),
        ""
    );
}

#[tokio::test]
async fn run_plan_selects_ready_milestones_and_skips_existing_tasks() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let previous = std::env::current_dir().unwrap();
    std::fs::create_dir_all(dir.path().join(".ferrus")).unwrap();
    std::fs::create_dir_all(dir.path().join("docs/specs")).unwrap();
    std::env::set_current_dir(dir.path()).unwrap();

    let data_dir = dir.path().join("runtime");
    tokio::fs::create_dir_all(&data_dir).await.unwrap();
    let local_ref = crate::project::LocalProjectRef {
        project_id: "test-project".to_string(),
        name: "test".to_string(),
        data_dir: data_dir.to_string_lossy().into_owned(),
    };
    let local_ref_toml = toml::to_string_pretty(&local_ref).unwrap();
    tokio::fs::write(".ferrus/project.toml", local_ref_toml)
        .await
        .unwrap();
    let spec_path = "docs/specs/spec.md";
    tokio::fs::write(
        spec_path,
        "## Milestones\n\
         - [x] #1.0 Foundation\n\
           - ID: m1.0\n\
           - Depends on: none\n\n\
         - [ ] #1.1 Ready one\n\
           - ID: m1.1\n\
           - Depends on: m1.0\n\n\
         - [ ] #1.2 Already queued\n\
           - ID: m1.2\n\
           - Depends on: m1.0\n\n\
         - [ ] #2.0 Blocked\n\
           - ID: m2.0\n\
           - Depends on: m1.1\n",
    )
    .await
    .unwrap();
    crate::project::record_task_status_with_origin(
        "t-002",
        ".ferrus/tasks/t-002.md",
        crate::project::TaskStatus::Pending,
        Some(spec_path),
        Some("m1.2"),
    )
    .await
    .unwrap();

    let plan = build_run_plan(spec_path).await.unwrap();

    assert_eq!(plan.eligible.len(), 1);
    assert_eq!(plan.eligible[0].id, "m1.1");
    assert!(plan.skipped.iter().any(|milestone| {
        milestone.id == "m1.2" && milestone.reason == "task t-002 is pending"
    }));
    assert!(
        plan.skipped
            .iter()
            .any(|milestone| milestone.id == "m2.0" && milestone.reason == "waiting for m1.1")
    );

    std::env::set_current_dir(previous).unwrap();
}

#[tokio::test]
async fn executor_workspace_includes_uncommitted_canonical_changes() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let previous = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();

    std::process::Command::new("git")
        .args(["init"])
        .status()
        .unwrap();
    tokio::fs::write(".gitignore", ".ferrus/\n").await.unwrap();
    tokio::fs::write("tracked.txt", "base\n").await.unwrap();
    std::process::Command::new("git")
        .args(["add", ".gitignore", "tracked.txt"])
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

    let data_dir = dir.path().join("runtime");
    tokio::fs::create_dir_all(&data_dir).await.unwrap();
    tokio::fs::create_dir_all(".ferrus").await.unwrap();
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

    tokio::fs::write("tracked.txt", "base\napproved\n")
        .await
        .unwrap();
    tokio::fs::write("new.txt", "new approved file\n")
        .await
        .unwrap();

    let workspace = prepare_executor_workspace("t-001").await.unwrap();

    let tracked_content = tokio::fs::read_to_string(workspace.workspace_dir.join("tracked.txt"))
        .await
        .unwrap()
        .replace("\r\n", "\n");
    assert_eq!(tracked_content, "base\napproved\n");
    assert_eq!(
        tokio::fs::read_to_string(workspace.workspace_dir.join("new.txt"))
            .await
            .unwrap(),
        "new approved file\n"
    );
    assert!(
        !workspace
            .workspace_dir
            .join("runtime/worktrees/t-001/tracked.txt")
            .exists()
    );
    let baseline_tree = workspace.baseline_tree.as_deref().unwrap();
    let baseline_ref = std::process::Command::new("git")
        .args(["rev-parse", "--verify", "refs/ferrus/baselines/t-001"])
        .output()
        .unwrap();
    assert!(baseline_ref.status.success());
    assert_eq!(
        String::from_utf8_lossy(&baseline_ref.stdout).trim(),
        baseline_tree
    );
    assert!(
        std::process::Command::new("git")
            .args(["gc", "--prune=now"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        std::process::Command::new("git")
            .args(["cat-file", "-e", &format!("{baseline_tree}^{{tree}}")])
            .status()
            .unwrap()
            .success()
    );

    std::env::set_current_dir(previous).unwrap();
}

#[tokio::test]
async fn executor_workspace_falls_back_to_canonical_for_non_git_project() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let previous = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();

    let data_dir = dir.path().join("runtime");
    tokio::fs::create_dir_all(&data_dir).await.unwrap();
    tokio::fs::create_dir_all(".ferrus").await.unwrap();
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

    let workspace = prepare_executor_workspace("t-001").await.unwrap();
    let canonical_project_root = tokio::fs::canonicalize(dir.path()).await.unwrap();

    assert_eq!(workspace.project_root, canonical_project_root);
    assert_eq!(workspace.workspace_dir, canonical_project_root);
    assert_eq!(workspace.baseline_tree, None);
    assert!(!data_dir.join("worktrees/t-001").exists());

    std::env::set_current_dir(previous).unwrap();
}

#[tokio::test]
async fn executor_parallel_limit_caps_non_git_projects() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let previous = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();

    let data_dir = dir.path().join("runtime");
    tokio::fs::create_dir_all(&data_dir).await.unwrap();
    tokio::fs::create_dir_all(".ferrus").await.unwrap();
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

    assert_eq!(executor_parallel_limit(4).await.unwrap(), 1);

    std::env::set_current_dir(previous).unwrap();
}

#[tokio::test]
async fn executor_workspace_starts_from_unborn_git_project() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let previous = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();

    std::process::Command::new("git")
        .args(["init"])
        .status()
        .unwrap();
    tokio::fs::write(
        ".gitignore",
        ".ferrus/\n.codex/config.toml\n.claude/mcp-executor.json\n",
    )
    .await
    .unwrap();
    tokio::fs::create_dir_all(".codex").await.unwrap();
    tokio::fs::write(
        ".codex/config.toml",
        "[mcp_servers.ferrus-executor]\ncommand = \"ferrus\"\n",
    )
    .await
    .unwrap();
    tokio::fs::create_dir_all(".claude").await.unwrap();
    tokio::fs::write(
        ".claude/mcp-executor.json",
        "{\"mcpServers\":{\"ferrus-executor\":{\"command\":\"ferrus\",\"args\":[]}}}",
    )
    .await
    .unwrap();
    tokio::fs::write("seed.txt", "uncommitted project file\n")
        .await
        .unwrap();

    let data_dir = dir.path().join("runtime");
    tokio::fs::create_dir_all(&data_dir).await.unwrap();
    tokio::fs::create_dir_all(".ferrus").await.unwrap();
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

    let workspace = prepare_executor_workspace("t-unborn").await.unwrap();

    assert!(workspace.baseline_tree.is_some());
    assert_eq!(
        tokio::fs::read_to_string(workspace.workspace_dir.join("seed.txt"))
            .await
            .unwrap(),
        "uncommitted project file\n"
    );
    assert!(
        !workspace
            .workspace_dir
            .join("runtime/worktrees/t-unborn/seed.txt")
            .exists()
    );
    assert_eq!(
        tokio::fs::read_to_string(workspace.workspace_dir.join(".codex/config.toml"))
            .await
            .unwrap(),
        "[mcp_servers.ferrus-executor]\ncommand = \"ferrus\"\n"
    );
    assert_eq!(
        tokio::fs::read_to_string(workspace.workspace_dir.join(".claude/mcp-executor.json"))
            .await
            .unwrap(),
        "{\"mcpServers\":{\"ferrus-executor\":{\"command\":\"ferrus\",\"args\":[]}}}"
    );
    assert_eq!(
        tokio::fs::read_to_string(workspace.workspace_dir.join(".ferrus/project.toml"))
            .await
            .unwrap(),
        toml::to_string_pretty(&local_ref).unwrap()
    );
    let baseline_tree = workspace.baseline_tree.clone();
    assert_eq!(
        tokio::fs::read_to_string(executor_workspace_baseline_path(&data_dir, "t-unborn"))
            .await
            .unwrap()
            .trim(),
        baseline_tree.as_deref().unwrap()
    );
    assert!(
        std::process::Command::new("git")
            .args(["update-ref", "-d", "refs/ferrus/baselines/t-unborn"])
            .status()
            .unwrap()
            .success()
    );

    let resumed_workspace = prepare_executor_workspace("t-unborn").await.unwrap();
    assert_eq!(resumed_workspace.baseline_tree, baseline_tree);
    let restored_ref = std::process::Command::new("git")
        .args(["rev-parse", "--verify", "refs/ferrus/baselines/t-unborn"])
        .output()
        .unwrap();
    assert!(restored_ref.status.success());
    assert_eq!(
        String::from_utf8_lossy(&restored_ref.stdout).trim(),
        baseline_tree.as_deref().unwrap()
    );

    std::env::set_current_dir(previous).unwrap();
}

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

#[tokio::test]
async fn reconcile_runtime_schedule_does_not_require_state_json() {
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
    crate::project::record_task_status(
        "t-001",
        ".ferrus/tasks/t-001.md",
        crate::project::TaskStatus::Complete,
    )
    .await
    .unwrap();

    let (_state_tx, state_rx) = watch::channel::<Option<WatchedState>>(None);
    let (msg_tx, _msg_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut ctx = HqContext::new(state_rx, Display(msg_tx), false);
    ctx.seed_completed_task_announcements().await.unwrap();

    ctx.reconcile_runtime_schedule().await.unwrap();

    crate::test_support::assert_no_state_json();
    std::env::set_current_dir(previous).unwrap();
}

#[tokio::test]
async fn reconcile_runtime_schedule_announces_new_completed_tasks_once() {
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
    crate::project::record_task_status(
        "t-001",
        ".ferrus/tasks/t-001.md",
        crate::project::TaskStatus::Pending,
    )
    .await
    .unwrap();

    let (_state_tx, state_rx) = watch::channel::<Option<WatchedState>>(None);
    let (msg_tx, mut msg_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut ctx = HqContext::new(state_rx, Display(msg_tx), false);
    ctx.seed_completed_task_announcements().await.unwrap();

    crate::project::record_task_status(
        "t-001",
        ".ferrus/tasks/t-001.md",
        crate::project::TaskStatus::Complete,
    )
    .await
    .unwrap();

    ctx.reconcile_runtime_schedule().await.unwrap();

    match msg_rx
        .try_recv()
        .expect("completion message should be sent")
    {
        tui::UiMessage::Success(text) => assert_eq!(text, "Task t-001 completed."),
        _ => panic!("expected success message"),
    }
    assert!(msg_rx.try_recv().is_err());

    ctx.reconcile_runtime_schedule().await.unwrap();
    assert!(msg_rx.try_recv().is_err());

    std::env::set_current_dir(previous).unwrap();
}

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
async fn human_answer_for_dead_waiter_is_not_restored_before_delivery() {
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
    crate::project::record_task_status(
        "t-011",
        ".ferrus/tasks/t-011.md",
        crate::project::TaskStatus::Executing,
    )
    .await
    .unwrap();
    crate::project::claim_task(
        "t-011",
        ".ferrus/tasks/t-011.md",
        "executor:codex:t-011",
        60,
    )
    .await
    .unwrap();
    crate::project::record_task_human_question_requested(
        "t-011",
        crate::project::TaskStatus::Executing,
        "executor:codex:t-011",
    )
    .await
    .unwrap();
    store::write_question_for_run_dir(".ferrus/runs/t-011", "Which branch?")
        .await
        .unwrap();

    let (_state_tx, state_rx) = watch::channel::<Option<WatchedState>>(None);
    let (msg_tx, _msg_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut ctx = HqContext::new(state_rx, Display(msg_tx), false);

    let error = ctx
        .answer_scoped_human_question("Use the short branch.".to_string())
        .await
        .unwrap_err()
        .to_string();

    assert!(error.contains("Executor agent is not configured"));
    let task = crate::project::list_tasks()
        .await
        .unwrap()
        .into_iter()
        .find(|task| task.id == "t-011")
        .unwrap();
    assert_eq!(task.status, "awaiting_human");
    assert_eq!(
        store::read_answer_for_run_dir(".ferrus/runs/t-011")
            .await
            .unwrap(),
        "Use the short branch."
    );
    assert_eq!(
        store::read_question_for_run_dir(".ferrus/runs/t-011")
            .await
            .unwrap(),
        "Which branch?"
    );

    std::env::set_current_dir(previous).unwrap();
}

#[test]
fn run_plan_prompt_context_uses_selected_prefix_only() {
    let plan = RunPlan {
        spec_path: "docs/specs/spec.md".to_string(),
        eligible: vec![
            RunPlanMilestone {
                id: "m1.0".to_string(),
                marker: "#1.0".to_string(),
                title: "First task".to_string(),
            },
            RunPlanMilestone {
                id: "m1.1".to_string(),
                marker: "#1.1".to_string(),
                title: "Second task".to_string(),
            },
        ],
        skipped: Vec::new(),
    };

    let context = run_plan_prompt_context(&plan, 1);

    assert!(context.contains("Spec: docs/specs/spec.md"));
    assert!(context.contains("Task count: 1"));
    assert!(context.contains("Milestone ID: m1.0"));
    assert!(!context.contains("Milestone ID: m1.1"));
}

#[tokio::test]
async fn selected_spec_archive_prompt_requires_closed_unarchived_spec() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let previous = std::env::current_dir().unwrap();
    std::fs::create_dir_all(dir.path().join(".ferrus/tasks")).unwrap();
    std::fs::create_dir_all(dir.path().join(".ferrus/runs/t-001")).unwrap();
    std::fs::create_dir_all(dir.path().join("docs/specs")).unwrap();
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

    let spec_path = "docs/specs/spec.md";
    tokio::fs::write(
        spec_path,
        "## Milestones\n\
         - [x] #1.0 Done\n\
           - ID: m1.0\n\
           - Depends on: none\n",
    )
    .await
    .unwrap();
    tokio::fs::write(".ferrus/tasks/t-001.md", "task")
        .await
        .unwrap();
    tokio::fs::write(".ferrus/runs/t-001/SUBMISSION.md", "done")
        .await
        .unwrap();
    crate::project::write_project_selection(&ProjectSelection {
        selected_spec: Some(spec_path.to_string()),
    })
    .await
    .unwrap();
    crate::project::record_task_status_with_origin(
        "t-001",
        ".ferrus/tasks/t-001.md",
        crate::project::TaskStatus::Complete,
        Some(spec_path),
        Some("m1.0"),
    )
    .await
    .unwrap();

    assert_eq!(
        selected_spec_archive_prompt().await.unwrap(),
        Some(SpecArchivePrompt {
            spec_path: spec_path.to_string(),
            task_count: 1,
        })
    );

    tokio::fs::remove_file(".ferrus/tasks/t-001.md")
        .await
        .unwrap();
    tokio::fs::remove_dir_all(".ferrus/runs/t-001")
        .await
        .unwrap();
    crate::project::record_task_status_with_origin(
        "t-001",
        &data_dir
            .join("archive/specs/spec/tasks/t-001.md")
            .to_string_lossy(),
        crate::project::TaskStatus::Complete,
        Some(spec_path),
        Some("m1.0"),
    )
    .await
    .unwrap();
    tokio::fs::create_dir_all(".ferrus/runs/t-001")
        .await
        .unwrap();
    tokio::fs::write(".ferrus/runs/t-001/SUBMISSION.md", "stale checkout copy")
        .await
        .unwrap();

    assert_eq!(selected_spec_archive_prompt().await.unwrap(), None);
    std::env::set_current_dir(previous).unwrap();
}

#[test]
fn run_plan_lines_do_not_report_batch_launch_as_unwired() {
    let plan = RunPlan {
        spec_path: "docs/specs/spec.md".to_string(),
        eligible: vec![RunPlanMilestone {
            id: "m1.0".to_string(),
            marker: "#1.0".to_string(),
            title: "First task".to_string(),
        }],
        skipped: Vec::new(),
    };

    let lines = run_plan_lines(&plan, 1).join("\n");

    assert!(!lines.contains("not wired"));
    assert!(lines.contains("selected  : 1"));
}

#[test]
fn executor_spawn_selection_skips_live_tasks_before_slot_limit() {
    let tasks = vec![
        task_record("t-001", crate::project::TaskStatus::Pending),
        task_record("t-002", crate::project::TaskStatus::Pending),
    ];

    let selected = select_executor_spawn_tasks(&tasks, 1, |task| task.id == "t-001");

    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].id, "t-002");
}

#[test]
fn task_claim_blocks_live_expected_or_active_external_agent() {
    let now = chrono::Utc::now();
    let mut task = task_record("t-001", crate::project::TaskStatus::Executing);
    task.claimed_by = Some("executor:codex:t-001".to_string());
    task.lease_until = Some((now + chrono::Duration::minutes(1)).to_rfc3339());
    let mut live_run_task_ids = HashSet::new();

    assert!(!task_claim_blocks_spawn(
        &task,
        "executor:codex:t-001",
        now,
        &live_run_task_ids,
    ));
    assert!(task_claim_blocks_spawn(
        &task,
        "executor:claude-code:t-001",
        now,
        &live_run_task_ids,
    ));

    live_run_task_ids.insert("t-001".to_string());
    assert!(task_claim_blocks_spawn(
        &task,
        "executor:codex:t-001",
        now,
        &live_run_task_ids,
    ));

    task.lease_until = Some((now - chrono::Duration::seconds(1)).to_rfc3339());
    assert!(task_claim_blocks_spawn(
        &task,
        "executor:codex:t-001",
        now,
        &live_run_task_ids,
    ));
    assert!(task_claim_blocks_spawn(
        &task,
        "executor:claude-code:t-001",
        now,
        &live_run_task_ids,
    ));

    live_run_task_ids.clear();
    assert!(!task_claim_blocks_spawn(
        &task,
        "executor:claude-code:t-001",
        now,
        &live_run_task_ids,
    ));
}

#[test]
fn task_claim_blocks_live_unclaimed_run() {
    let now = chrono::Utc::now();
    let task = task_record("t-001", crate::project::TaskStatus::Pending);
    let live_run_task_ids = HashSet::from(["t-001".to_string()]);

    assert!(task_claim_blocks_spawn(
        &task,
        "executor:codex:t-001",
        now,
        &live_run_task_ids,
    ));
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

#[test]
fn answered_human_resume_only_blocks_live_question_owner() {
    let live_run_agents = HashSet::from(["executor:codex:t-001".to_string()]);

    assert!(!answered_human_owner_is_live(
        "supervisor:codex:t-001",
        &live_run_agents,
        false,
    ));
    assert!(answered_human_owner_is_live(
        "executor:codex:t-001",
        &live_run_agents,
        false,
    ));
    assert!(answered_human_owner_is_live(
        "supervisor:codex:t-001",
        &HashSet::new(),
        true,
    ));
}

#[test]
fn human_question_selection_uses_fifo_or_presented_task_id() {
    let question = |task_id: &str| crate::project::HumanQuestion {
        task_id: task_id.to_string(),
        task_path: format!(".ferrus/tasks/{task_id}.md"),
        run_dir: format!(".ferrus/runs/{task_id}"),
        question: format!("Question for {task_id}"),
    };
    let questions = vec![question("t-001"), question("t-002")];

    let selected = select_human_question(questions.clone(), None).unwrap();
    assert_eq!(selected.task_id, "t-001");

    let selected = select_human_question(questions, Some("t-002")).unwrap();
    assert_eq!(selected.task_id, "t-002");
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

#[tokio::test]
async fn answered_consultation_selection_returns_tasks_with_existing_response() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let previous = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();
    store::write_consult_response_for_run_dir(".ferrus/runs/t-002", "existing answer")
        .await
        .unwrap();
    let tasks = vec![
        task_record("t-001", crate::project::TaskStatus::Consultation),
        task_record("t-002", crate::project::TaskStatus::Consultation),
        task_record("t-003", crate::project::TaskStatus::Executing),
    ];

    let selected = answered_consultation_tasks(&tasks).await.unwrap();

    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].id, "t-002");
    std::env::set_current_dir(previous).unwrap();
}

#[cfg(unix)]
#[test]
fn stale_pid_detection() {
    assert!(platform::pid_is_alive(std::process::id()));
    assert!(!platform::pid_is_alive(999999));
}

#[tokio::test]
async fn plain_input_answers_first_scoped_human_question() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let previous = std::env::current_dir().unwrap();
    std::fs::create_dir_all(dir.path().join(".ferrus/tasks")).unwrap();
    std::fs::create_dir_all(dir.path().join(".ferrus/runs/t-007")).unwrap();
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
    crate::project::record_task_status(
        "t-007",
        ".ferrus/tasks/t-007.md",
        crate::project::TaskStatus::Executing,
    )
    .await
    .unwrap();
    crate::project::record_task_human_question_requested(
        "t-007",
        crate::project::TaskStatus::Executing,
        "executor:codex:7",
    )
    .await
    .unwrap();
    store::write_question_for_run_dir(".ferrus/runs/t-007", "Need human input")
        .await
        .unwrap();

    let (_state_tx, state_rx) = watch::channel::<Option<WatchedState>>(None);
    let (msg_tx, _msg_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut ctx = HqContext::new(state_rx, Display(msg_tx), false);

    let error = dispatch("Use option A", &mut ctx)
        .await
        .unwrap_err()
        .to_string();

    crate::test_support::assert_no_state_json();
    assert!(error.contains("Executor agent is not configured"));
    assert_eq!(
        store::read_answer_for_run_dir(".ferrus/runs/t-007")
            .await
            .unwrap(),
        "Use option A"
    );
    assert_eq!(
        store::read_question_for_run_dir(".ferrus/runs/t-007")
            .await
            .unwrap(),
        "Need human input"
    );
    let tasks = crate::project::list_tasks().await.unwrap();
    let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
    assert_eq!(task.status, "awaiting_human");

    std::env::set_current_dir(previous).unwrap();
}

#[tokio::test]
async fn queued_human_answers_follow_fifo_and_preserve_presented_task() {
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

    for task_id in ["t-021", "t-022"] {
        crate::project::record_task_status(
            task_id,
            &format!(".ferrus/tasks/{task_id}.md"),
            crate::project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        crate::project::record_task_human_question_requested(
            task_id,
            crate::project::TaskStatus::Executing,
            &format!("executor:codex:{task_id}"),
        )
        .await
        .unwrap();
        store::write_question_for_run_dir(
            &format!(".ferrus/runs/{task_id}"),
            &format!("Question for {task_id}"),
        )
        .await
        .unwrap();
    }

    let (_state_tx, state_rx) = watch::channel::<Option<WatchedState>>(None);
    let (msg_tx, _msg_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut ctx = HqContext::new(state_rx, Display(msg_tx), false);

    let error = dispatch("First answer", &mut ctx)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("Executor agent is not configured"));
    assert_eq!(
        store::read_answer_for_run_dir(".ferrus/runs/t-021")
            .await
            .unwrap(),
        "First answer"
    );
    assert!(!std::path::Path::new(".ferrus/runs/t-022/ANSWER.md").exists());
    let questions = crate::project::list_human_questions().await.unwrap();
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].task_id, "t-022");

    let error = dispatch_with_human_question_target("Stale answer", Some("t-021"), false, &mut ctx)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("Task t-021 is not waiting"));
    assert!(!std::path::Path::new(".ferrus/runs/t-022/ANSWER.md").exists());

    let error = dispatch_with_human_question_target(
        "Use option B\n\n- preserve formatting",
        Some("t-022"),
        false,
        &mut ctx,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("Executor agent is not configured"));
    assert_eq!(
        store::read_answer_for_run_dir(".ferrus/runs/t-022")
            .await
            .unwrap(),
        "Use option B\n\n- preserve formatting"
    );

    std::env::set_current_dir(previous).unwrap();
}

#[tokio::test]
async fn plain_input_answers_scoped_human_question_without_state_json() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let previous = std::env::current_dir().unwrap();
    std::fs::create_dir_all(dir.path().join(".ferrus/tasks")).unwrap();
    std::fs::create_dir_all(dir.path().join(".ferrus/runs/t-009")).unwrap();
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
    crate::project::record_task_status(
        "t-009",
        ".ferrus/tasks/t-009.md",
        crate::project::TaskStatus::Executing,
    )
    .await
    .unwrap();
    crate::project::record_task_human_question_requested(
        "t-009",
        crate::project::TaskStatus::Executing,
        "executor:codex:9",
    )
    .await
    .unwrap();
    store::write_question_for_run_dir(".ferrus/runs/t-009", "Need scoped input")
        .await
        .unwrap();

    let (_state_tx, state_rx) = watch::channel::<Option<WatchedState>>(None);
    let (msg_tx, _msg_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut ctx = HqContext::new(state_rx, Display(msg_tx), false);

    let error = dispatch("Use scoped answer", &mut ctx)
        .await
        .unwrap_err()
        .to_string();

    assert!(error.contains("Executor agent is not configured"));
    assert_eq!(
        store::read_answer_for_run_dir(".ferrus/runs/t-009")
            .await
            .unwrap(),
        "Use scoped answer"
    );
    assert_eq!(
        store::read_question_for_run_dir(".ferrus/runs/t-009")
            .await
            .unwrap(),
        "Need scoped input"
    );
    let task = crate::project::list_tasks()
        .await
        .unwrap()
        .into_iter()
        .find(|task| task.id == "t-009")
        .unwrap();
    assert_eq!(task.status, "awaiting_human");
    crate::test_support::assert_no_state_json();

    std::env::set_current_dir(previous).unwrap();
}
