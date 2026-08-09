use super::*;

struct CurrentDirGuard(std::path::PathBuf);

impl CurrentDirGuard {
    fn change_to(path: &std::path::Path) -> Self {
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(path).unwrap();
        Self(previous)
    }
}

impl Drop for CurrentDirGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.0);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn memory_only_loading_ignores_invalid_repository_graph_settings() {
    let _lock = crate::test_support::cwd_lock().lock().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(workspace.path().join(".ferrus")).unwrap();
    std::fs::create_dir_all(workspace.path().join("docs/specs")).unwrap();
    assert!(
        std::process::Command::new("git")
            .arg("init")
            .arg("--quiet")
            .arg(workspace.path())
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(
        workspace.path().join("ferrus.toml"),
        "[repository_graph]\nenabled = false\nunsupported_graph_setting = true\n",
    )
    .unwrap();
    std::fs::write(
        workspace.path().join(".ferrus/project.toml"),
        toml::to_string(&project::LocalProjectRef {
            project_id: "memory-only-test".to_string(),
            name: "memory-only-test".to_string(),
            data_dir: data.path().to_string_lossy().into_owned(),
        })
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        data.path().join("project.toml"),
        toml::to_string(&project::ProjectMetadata {
            id: "memory-only-test".to_string(),
            name: "memory-only-test".to_string(),
            workspace_dir: workspace.path().to_string_lossy().into_owned(),
            ferrus_dir: workspace
                .path()
                .join(".ferrus")
                .to_string_lossy()
                .into_owned(),
            vcs: None,
            origin_repo: None,
            default_branch: None,
            current_head: None,
            created_at: "2026-08-09T00:00:00Z".to_string(),
            last_opened_at: "2026-08-09T00:00:00Z".to_string(),
            version: 1,
        })
        .unwrap(),
    )
    .unwrap();
    let _cwd = CurrentDirGuard::change_to(workspace.path());

    let memory = LocalProjectContext::load_for_cli(false).await.unwrap();
    assert!(memory.graph.is_none());
    assert_eq!(memory.query_limits, QueryLimitsConfig::default());
    assert!(LocalProjectContext::load_for_cli(true).await.is_err());
}
