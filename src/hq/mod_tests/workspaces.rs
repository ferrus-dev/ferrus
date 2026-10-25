//! Tests for workspaces.

use super::*;

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
    tokio::fs::create_dir_all(".codex").await.unwrap();
    tokio::fs::write(".codex/config.toml", "model = \"baseline\"\n")
        .await
        .unwrap();
    std::process::Command::new("git")
        .args(["add", ".gitignore", "tracked.txt", ".codex/config.toml"])
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

    tokio::fs::write(".codex/config.toml", "model = \"canonical-update\"\n")
        .await
        .unwrap();
    let resumed_workspace = prepare_executor_workspace("t-001").await.unwrap();
    assert_eq!(
        resumed_workspace.baseline_tree.as_deref(),
        Some(baseline_tree)
    );
    assert_eq!(
        tokio::fs::read_to_string(resumed_workspace.workspace_dir.join(".codex/config.toml"))
            .await
            .unwrap(),
        "model = \"baseline\"\n"
    );
    let config_patch = std::process::Command::new("git")
        .arg("-C")
        .arg(&resumed_workspace.workspace_dir)
        .args([
            "diff",
            "--binary",
            baseline_tree,
            "--",
            ".codex/config.toml",
        ])
        .output()
        .unwrap();
    assert!(config_patch.status.success());
    assert!(config_patch.stdout.is_empty());

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
    assert!(
        std::process::Command::new("git")
            .args(["add", "seed.txt"])
            .status()
            .unwrap()
            .success()
    );

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

    let worktrees_dir = data_dir.join("worktrees");
    tokio::fs::create_dir_all(&worktrees_dir).await.unwrap();
    let baseline_metadata_dir = worktrees_dir.join(".baseline-trees");
    tokio::fs::write(&baseline_metadata_dir, "blocks baseline metadata directory")
        .await
        .unwrap();

    prepare_executor_workspace("t-unborn").await.unwrap_err();
    assert!(!worktrees_dir.join("t-unborn").exists());
    assert!(
        !std::process::Command::new("git")
            .args([
                "show-ref",
                "--verify",
                "--quiet",
                "refs/ferrus/baselines/t-unborn"
            ])
            .status()
            .unwrap()
            .success()
    );

    tokio::fs::remove_file(baseline_metadata_dir).await.unwrap();
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

    tokio::fs::write(
        ".codex/config.toml",
        "[mcp_servers.ferrus-executor]\ncommand = \"ferrus-updated\"\n",
    )
    .await
    .unwrap();
    let resumed_workspace = prepare_executor_workspace("t-unborn").await.unwrap();
    assert_eq!(resumed_workspace.baseline_tree, baseline_tree);
    assert_eq!(
        tokio::fs::read_to_string(resumed_workspace.workspace_dir.join(".codex/config.toml"))
            .await
            .unwrap(),
        "[mcp_servers.ferrus-executor]\ncommand = \"ferrus-updated\"\n"
    );
    let restored_ref = std::process::Command::new("git")
        .args(["rev-parse", "--verify", "refs/ferrus/baselines/t-unborn"])
        .output()
        .unwrap();
    assert!(restored_ref.status.success());
    assert_eq!(
        String::from_utf8_lossy(&restored_ref.stdout).trim(),
        baseline_tree.as_deref().unwrap()
    );

    tokio::fs::remove_file(executor_workspace_baseline_path(&data_dir, "t-unborn"))
        .await
        .unwrap();
    tokio::fs::remove_file(resumed_workspace.workspace_dir.join("seed.txt"))
        .await
        .unwrap();

    let rebuilt_workspace = prepare_executor_workspace("t-unborn").await.unwrap();
    assert_eq!(
        tokio::fs::read_to_string(rebuilt_workspace.workspace_dir.join("seed.txt"))
            .await
            .unwrap(),
        "uncommitted project file\n"
    );
    assert!(executor_workspace_baseline_path(&data_dir, "t-unborn").exists());

    std::env::set_current_dir(previous).unwrap();
}
