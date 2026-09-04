//! Local memory runtime tests for read-only retrieval, indexing, and independent freshness.

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
        "[repository_graph]\nenabled = false\nunsupported_graph_setting = true\n\
         \n[repository_graph.query_limits]\nmax_results = 7\nmax_bytes = 8192\n\
         max_snippet_bytes = 1024\nmax_depth = 2\nmax_duration_ms = 250\n\
         max_diagnostics = 3\n",
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
    assert_eq!(
        memory.query_limits,
        QueryLimitsConfig {
            max_results: 7,
            max_bytes: 8192,
            max_snippet_bytes: 1024,
            max_depth: 2,
            max_duration_ms: 250,
            max_diagnostics: 3,
        }
    );
    assert!(LocalProjectContext::load_for_cli(true).await.is_err());
}

#[test]
fn repository_only_queries_ignore_an_incompatible_memory_sidecar() {
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(workspace.path().join("src")).unwrap();
    std::fs::write(
        workspace.path().join("src/lib.rs"),
        b"pub struct RuntimeTaskContext;\n",
    )
    .unwrap();

    let repository = crate::repository_graph::domain::RepositoryRef {
        namespace: crate::repository_graph::domain::RepositoryNamespace::new("local:test").unwrap(),
        repository_id: crate::repository_graph::domain::RepositoryId::new("root").unwrap(),
    };
    let mut config = crate::repository_graph::config::RepositoryGraphConfig::default();
    config.enabled = true;
    let graph = LocalGraphContext {
        project_root: workspace.path().to_path_buf(),
        root: workspace.path().to_path_buf(),
        repository: repository.clone(),
        config: config.clone(),
        repository_view: None,
        task_view_id: None,
        run_id: None,
    };
    let source = graph.discover().unwrap();
    let graph_path = data.path().join(SIDECAR_FILE_NAME);
    let crate::repository_graph::sqlite::OpenSidecarResult::Ready(mut sidecar) =
        crate::repository_graph::sqlite::open_for_build_at(&graph_path).unwrap()
    else {
        panic!("new graph sidecar unexpectedly requires rebuild");
    };
    crate::repository_graph::index::IndexCoordinator::new(&mut sidecar)
        .index(
            &source,
            &config,
            crate::repository_graph::index::IndexRequest {
                build_id: crate::repository_graph::domain::BuildId::new("build-runtime").unwrap(),
                view_name: crate::repository_graph::domain::PublishedViewName::new(
                    crate::repository_graph_runtime::CANONICAL_VIEW,
                )
                .unwrap(),
                force_full: false,
            },
        )
        .unwrap();
    drop(sidecar);

    std::fs::write(
        data.path().join(MEMORY_SIDECAR_FILE_NAME),
        b"not a SQLite database",
    )
    .unwrap();
    let context = LocalProjectContext {
        graph: Some(graph),
        query_limits: config.query_limits.clone(),
        project: ProjectRef {
            namespace: ProjectNamespace::new("local:ferrus").unwrap(),
            project_id: ProjectId::new("repository-only-test").unwrap(),
        },
        data_dir: data.path().to_path_buf(),
        exact_memory_source: None,
        compare_local_freshness: false,
    };
    let budget = context.default_budget().unwrap();
    let search = context
        .search(FederatedSearchRequest {
            scope: context
                .scope(ContextDomain::Repository, budget.clone())
                .unwrap(),
            text: crate::project_memory::domain::MemoryQueryText::new("RuntimeTaskContext")
                .unwrap(),
            repository_kinds: vec![],
            repository_paths: vec![],
            memory_kinds: vec![],
            memory_sources: vec![],
            cursor: None,
        })
        .unwrap();
    assert!(search.repository.is_some());
    assert!(search.memory.is_none());
    assert!(!search.results.is_empty());

    let response = context
        .context(FederatedContextRequest {
            scope: context.scope(ContextDomain::Repository, budget).unwrap(),
            seeds: vec![
                crate::project_memory::federation::FederatedContextSeed::Repository(
                    crate::repository_graph::query::ContextSeed::Path(
                        crate::repository_graph::domain::RepoPath::new("src/lib.rs").unwrap(),
                    ),
                ),
            ],
            repository_policy: crate::repository_graph::query::ContextPolicy {
                direction: crate::repository_graph::query::EdgeDirection::Both,
                edge_kinds: vec![],
                include_unresolved: false,
                include_external: false,
            },
            memory_policy: crate::project_memory::query::MemoryContextPolicy {
                direction: crate::repository_graph::query::EdgeDirection::Both,
                relationship_kinds: vec![],
                include_unresolved: false,
                include_stale: false,
                include_snippets: false,
            },
            cursor: None,
        })
        .unwrap();
    assert!(response.repository.is_some());
    assert!(response.memory.is_none());
    assert!(!response.items.is_empty());
}
