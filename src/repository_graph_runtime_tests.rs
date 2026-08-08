use super::*;

struct CurrentDirGuard {
    previous: std::path::PathBuf,
}

impl CurrentDirGuard {
    fn change_to(path: &Path) -> Self {
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(path).unwrap();
        Self { previous }
    }
}

impl Drop for CurrentDirGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.previous);
    }
}

fn remove_sqlite_file_set(path: &Path) {
    for suffix in ["-wal", "-shm", ""] {
        let mut candidate = path.as_os_str().to_os_string();
        candidate.push(suffix);
        match std::fs::remove_file(std::path::PathBuf::from(candidate)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("failed to remove SQLite sidecar file: {error}"),
        }
    }
}

fn indexed_context(root: &Path, sidecar_path: &Path) -> (LocalGraphContext, ContextRequest) {
    let mut context = context(root);
    context.config.enabled = true;
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), b"pub struct RuntimeTaskContext;\n").unwrap();
    let source = context.discover().unwrap();
    let mut sidecar = match repository_graph::sqlite::open_for_build_at(sidecar_path).unwrap() {
        repository_graph::sqlite::OpenSidecarResult::Ready(sidecar) => sidecar,
        repository_graph::sqlite::OpenSidecarResult::RequiresRebuild(_) => {
            panic!("new sidecar unexpectedly requires rebuild")
        }
    };
    repository_graph::index::IndexCoordinator::new(&mut sidecar)
        .index(
            &source,
            &context.config,
            repository_graph::index::IndexRequest {
                build_id: repository_graph::domain::BuildId::new("build-1").unwrap(),
                view_name: PublishedViewName::new(CANONICAL_VIEW).unwrap(),
                force_full: false,
            },
        )
        .unwrap();
    drop(sidecar);
    let request = ContextRequest {
        scope: context
            .scope(default_budget(&context.config.query_limits).unwrap())
            .unwrap(),
        seeds: vec![repository_graph::query::ContextSeed::Path(
            repository_graph::domain::RepoPath::new("src/lib.rs").unwrap(),
        )],
        policy: repository_graph::query::ContextPolicy {
            direction: repository_graph::query::EdgeDirection::Both,
            edge_kinds: vec![],
            include_unresolved: false,
            include_external: false,
        },
        page: repository_graph::query::PageRequest { cursor: None },
    };
    (context, request)
}

fn context(root: &Path) -> LocalGraphContext {
    LocalGraphContext {
        project_root: root.to_path_buf(),
        root: root.to_path_buf(),
        repository: RepositoryRef {
            namespace: RepositoryNamespace::new("local:test").unwrap(),
            repository_id: RepositoryId::new("root").unwrap(),
        },
        config: RepositoryGraphConfig::default(),
        repository_view: None,
        task_view_id: None,
        run_id: None,
    }
}

#[test]
fn canonical_discovery_ignores_a_task_worktree_root() {
    let canonical = tempfile::tempdir().unwrap();
    let worktree = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(canonical.path().join("src")).unwrap();
    std::fs::create_dir_all(worktree.path().join("src")).unwrap();
    std::fs::write(
        canonical.path().join("src/lib.rs"),
        b"pub struct CanonicalSymbol;\n",
    )
    .unwrap();
    std::fs::write(
        worktree.path().join("src/lib.rs"),
        b"pub struct UnapprovedTaskSymbol;\n",
    )
    .unwrap();
    let mut graph_context = context(canonical.path());
    graph_context.root = worktree.path().to_path_buf();

    let task_source = graph_context.discover().unwrap();
    let canonical_source = graph_context.discover_canonical().unwrap();

    assert_ne!(
        task_source.manifest().revision.manifest_digest,
        canonical_source.manifest().revision.manifest_digest
    );
    assert_eq!(
        canonical_source.manifest().revision.manifest_digest,
        context(canonical.path())
            .discover()
            .unwrap()
            .manifest()
            .revision
            .manifest_digest
    );
}

#[test]
fn submitted_tree_freeze_rejects_content_newer_than_the_indexed_snapshot() {
    let repository = tempfile::tempdir().unwrap();
    let sidecar_directory = tempfile::tempdir().unwrap();
    let root = repository.path();
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
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub struct Indexed;\n").unwrap();

    let mut graph_context = context(root);
    graph_context.config.enabled = true;
    let source = graph_context.discover().unwrap();
    let sidecar_path = sidecar_directory.path().join(SIDECAR_FILE_NAME);
    let mut sidecar = match open_for_build_at(&sidecar_path).unwrap() {
        OpenSidecarResult::Ready(sidecar) => sidecar,
        OpenSidecarResult::RequiresRebuild(_) => {
            panic!("new sidecar unexpectedly requires rebuild")
        }
    };
    let indexed = IndexCoordinator::new(&mut sidecar)
        .index(
            &source,
            &graph_context.config,
            IndexRequest {
                build_id: BuildId::new("submitted-freeze-build").unwrap(),
                view_name: PublishedViewName::new("task:test").unwrap(),
                force_full: false,
            },
        )
        .unwrap();
    drop(sidecar);

    let matching_tree = capture_matching_submitted_tree(
        &sidecar_path,
        root,
        "task-match",
        graph_context.repository.clone(),
        &graph_context.config,
        &indexed.snapshot.id,
    )
    .unwrap();
    assert!(matching_tree.value().len() >= 40);
    release_submitted_tree_pin(root, "task-match").unwrap();

    std::fs::write(root.join("src/lib.rs"), "pub struct NewerEdit;\n").unwrap();
    let error = capture_matching_submitted_tree(
        &sidecar_path,
        root,
        "task-race",
        graph_context.repository.clone(),
        &graph_context.config,
        &indexed.snapshot.id,
    )
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("does not match the indexed task view")
    );
    let references = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("show-ref")
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&references.stdout).contains("refs/ferrus/reviews/"));
}

#[test]
fn absent_status_search_and_context_are_read_only_and_actionable() {
    let directory = tempfile::tempdir().unwrap();
    let sidecar = directory.path().join(SIDECAR_FILE_NAME);
    let context = context(directory.path());

    let status = status_response_at(&context, &sidecar, None).unwrap();
    let search = search_response_at(
        &context,
        &sidecar,
        None,
        &SearchRequest {
            scope: context
                .scope(default_budget(&context.config.query_limits).unwrap())
                .unwrap(),
            text: "RuntimeTaskContext".to_string(),
            node_kinds: vec![],
            paths: vec![],
            page: repository_graph::query::PageRequest { cursor: None },
        },
    )
    .unwrap_err();
    let context_response = context_response_at(
        &context,
        &sidecar,
        None,
        &ContextRequest {
            scope: context
                .scope(default_budget(&context.config.query_limits).unwrap())
                .unwrap(),
            seeds: vec![repository_graph::query::ContextSeed::Path(
                repository_graph::domain::RepoPath::new("src/lib.rs").unwrap(),
            )],
            policy: repository_graph::query::ContextPolicy {
                direction: repository_graph::query::EdgeDirection::Both,
                edge_kinds: vec![],
                include_unresolved: false,
                include_external: false,
            },
            page: repository_graph::query::PageRequest { cursor: None },
        },
    )
    .unwrap_err();

    assert_eq!(status.data.availability, Availability::NotBuilt);
    assert_eq!(status.data.recommended_action, Some(RetrievalAction::Index));
    assert_eq!(search.code, QueryErrorCode::NotBuilt);
    assert_eq!(search.recommended_action, Some(RetrievalAction::Index));
    assert_eq!(context_response.code, QueryErrorCode::NotBuilt);
    assert_eq!(
        context_response.recommended_action,
        Some(RetrievalAction::Index)
    );
    assert!(!sidecar.exists());
}

#[test]
fn unavailable_task_status_exposes_binding_and_direct_source_fallback() {
    let directory = tempfile::tempdir().unwrap();
    let sidecar = directory.path().join(SIDECAR_FILE_NAME);
    let mut context = context(directory.path());
    context.repository_view = Some(
        project::RepositoryViewReference::new(
            None,
            None,
            project::RepositoryViewStatus::Unavailable,
        )
        .unwrap(),
    );
    context.task_view_id = Some(TaskViewId::new("t-001").unwrap());
    let response = status_response_at(&context, &sidecar, None).unwrap();

    assert_eq!(response.task_view, None);
    assert_eq!(response.data.availability, Availability::NotBuilt);
    assert_eq!(response.data.published_view, None);
    assert_eq!(
        response.data.task_view_status,
        Some(TaskViewStatus::Unavailable)
    );
    assert_eq!(
        response.data.fallback,
        Some(RetrievalFallback::DirectSourceInspection)
    );
    assert!(
        context
            .scope(default_budget(&context.config.query_limits).unwrap())
            .unwrap_err()
            .to_string()
            .contains("current task view")
    );
    assert!(!sidecar.exists());
}

#[test]
fn unreadable_sidecar_is_distinct_from_a_missing_index() {
    let directory = tempfile::tempdir().unwrap();
    let sidecar = directory.path().join(SIDECAR_FILE_NAME);
    std::fs::write(&sidecar, b"not a sqlite database").unwrap();
    let context = context(directory.path());

    let status = status_response_at(&context, &sidecar, None).unwrap();

    assert_eq!(status.data.availability, Availability::Incompatible);
    assert_eq!(status.freshness.reason_codes, ["sidecar_unreadable"]);
    assert_eq!(
        status.data.recommended_action,
        Some(RetrievalAction::Rebuild)
    );
}

#[test]
fn mcp_runtime_does_not_label_changed_source_as_fresh_without_revalidation() {
    let directory = tempfile::tempdir().unwrap();
    let sidecar = directory.path().join(SIDECAR_FILE_NAME);
    let (context, request) = indexed_context(directory.path(), &sidecar);
    std::fs::write(
        directory.path().join("src/lib.rs"),
        b"pub struct ChangedAfterIndex;\n",
    )
    .unwrap();

    let response = context_response_at(&context, &sidecar, None, &request).unwrap();

    assert_eq!(response.freshness.freshness, Freshness::Unknown);
    assert_eq!(response.freshness.reason_codes, ["source_not_compared"]);
}

#[tokio::test]
async fn approval_refresh_waits_for_the_active_canonical_lease_and_retries() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let data_dir = root.join(".ferrus/projects/test-project");
    std::fs::create_dir_all(root.join(".ferrus")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::write(
        root.join("ferrus.toml"),
        "[repository_graph]\nenabled = true\n",
    )
    .unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub struct CanonicalSymbol;\n").unwrap();
    std::fs::write(
        root.join(".ferrus/project.toml"),
        toml::to_string(&project::LocalProjectRef {
            project_id: "test-project".to_string(),
            name: "test".to_string(),
            data_dir: data_dir.to_string_lossy().into_owned(),
        })
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        data_dir.join("project.toml"),
        toml::to_string(&project::ProjectMetadata {
            id: "test-project".to_string(),
            name: "test".to_string(),
            workspace_dir: root.to_string_lossy().into_owned(),
            ferrus_dir: root.join(".ferrus").to_string_lossy().into_owned(),
            vcs: None,
            origin_repo: None,
            default_branch: None,
            current_head: None,
            created_at: "2026-07-28T00:00:00Z".to_string(),
            last_opened_at: "2026-07-28T00:00:00Z".to_string(),
            version: 1,
        })
        .unwrap(),
    )
    .unwrap();

    let _cwd = CurrentDirGuard::change_to(root);
    project::record_task_status(
        "t-approval",
        ".ferrus/tasks/t-approval.md",
        project::TaskStatus::Complete,
    )
    .await
    .unwrap();
    project::record_canonical_graph_invalidation(
        "t-approval",
        None,
        None,
        project::CanonicalInvalidationReason::ApprovedIntegration,
    )
    .await
    .unwrap();

    let context = LocalGraphContext::load(false).await.unwrap();
    let view_name = PublishedViewName::new(CANONICAL_VIEW).unwrap();
    let sidecar_path = data_dir.join(SIDECAR_FILE_NAME);
    let mut sidecar = match open_for_build_at(&sidecar_path).unwrap() {
        OpenSidecarResult::Ready(sidecar) => sidecar,
        OpenSidecarResult::RequiresRebuild(_) => {
            panic!("new sidecar unexpectedly requires rebuild")
        }
    };
    assert_eq!(
        sidecar
            .acquire_refresh_lease(
                &context.repository,
                &view_name,
                "blocking-refresh",
                REFRESH_LEASE_TTL,
            )
            .unwrap(),
        RefreshLeaseOutcome::Acquired
    );
    drop(sidecar);

    let refresh = tokio::spawn(refresh_canonical_graph_after_approval(
        root.to_path_buf(),
        "t-approval".to_string(),
        None,
    ));
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(!refresh.is_finished());

    let mut sidecar = match open_for_build_at(&sidecar_path).unwrap() {
        OpenSidecarResult::Ready(sidecar) => sidecar,
        OpenSidecarResult::RequiresRebuild(_) => {
            panic!("new sidecar unexpectedly requires rebuild")
        }
    };
    assert!(
        sidecar
            .release_refresh_lease(&context.repository, &view_name, "blocking-refresh",)
            .unwrap()
    );
    drop(sidecar);

    tokio::time::timeout(Duration::from_secs(10), refresh)
        .await
        .unwrap()
        .unwrap();
    let reference = project::canonical_graph_reference().await.unwrap();
    assert_eq!(reference.status, project::CanonicalGraphStatus::Fresh);
    assert!(reference.snapshot_id.is_some());
}

#[test]
fn task_scope_remains_pinned_when_canonical_publication_advances() {
    let directory = tempfile::tempdir().unwrap();
    let sidecar_path = directory.path().join(SIDECAR_FILE_NAME);
    let source_root = directory.path().join("repository");
    std::fs::create_dir(&source_root).unwrap();
    let (mut context, _) = indexed_context(&source_root, &sidecar_path);
    let sidecar = match open_for_query_at(&sidecar_path).unwrap() {
        OpenQuerySidecarResult::Ready(sidecar) => sidecar,
        _ => panic!("indexed sidecar must be queryable"),
    };
    let baseline = sidecar
        .published_view(
            &context.repository,
            &PublishedViewName::new(CANONICAL_VIEW).unwrap(),
        )
        .unwrap()
        .unwrap()
        .snapshot_id;
    drop(sidecar);
    context.repository_view = Some(
        project::RepositoryViewReference::new(
            Some(baseline.clone()),
            None,
            project::RepositoryViewStatus::Available,
        )
        .unwrap(),
    );

    std::fs::write(
        source_root.join("src/lib.rs"),
        b"pub struct CanonicalAdvanced;\n",
    )
    .unwrap();
    let source = context.discover().unwrap();
    let mut sidecar = match open_for_build_at(&sidecar_path).unwrap() {
        OpenSidecarResult::Ready(sidecar) => sidecar,
        OpenSidecarResult::RequiresRebuild(_) => panic!("sidecar unexpectedly incompatible"),
    };
    let advanced = IndexCoordinator::new(&mut sidecar)
        .index(
            &source,
            &context.config,
            IndexRequest {
                build_id: BuildId::new("build-advanced").unwrap(),
                view_name: PublishedViewName::new(CANONICAL_VIEW).unwrap(),
                force_full: false,
            },
        )
        .unwrap();
    assert_ne!(advanced.snapshot.id, baseline);
    drop(sidecar);

    let request = SearchRequest {
        scope: context
            .scope(default_budget(&context.config.query_limits).unwrap())
            .unwrap(),
        text: "RuntimeTaskContext".to_string(),
        node_kinds: vec![],
        paths: vec![],
        page: repository_graph::query::PageRequest { cursor: None },
    };
    let response = search_response_at(&context, &sidecar_path, None, &request).unwrap();

    assert_eq!(response.snapshot_id, baseline);
    assert!(response.data.hits.iter().any(|hit| {
        hit.semantic_key
            .as_ref()
            .is_some_and(|key| key.as_str().contains("RuntimeTaskContext"))
    }));
}

#[tokio::test]
async fn dispatch_pins_git_baseline_without_changing_task_lifecycle() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    std::fs::create_dir_all(root.join(".ferrus/projects/test-project")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("ferrus.toml"),
        "[repository_graph]\nenabled = true\n",
    )
    .unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub struct BaselineSymbol;\n").unwrap();
    std::fs::write(root.join("src/deleted.rs"), "pub struct DeletedSymbol;\n").unwrap();
    let data_dir = root.join(".ferrus/projects/test-project");
    std::fs::write(
        root.join(".ferrus/project.toml"),
        toml::to_string(&project::LocalProjectRef {
            project_id: "test-project".to_string(),
            name: "test".to_string(),
            data_dir: data_dir.to_string_lossy().into_owned(),
        })
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        data_dir.join("project.toml"),
        toml::to_string(&project::ProjectMetadata {
            id: "test-project".to_string(),
            name: "test".to_string(),
            workspace_dir: root.to_string_lossy().into_owned(),
            ferrus_dir: root.join(".ferrus").to_string_lossy().into_owned(),
            vcs: Some("git".to_string()),
            origin_repo: None,
            default_branch: Some("main".to_string()),
            current_head: None,
            created_at: "2026-07-22T00:00:00Z".to_string(),
            last_opened_at: "2026-07-22T00:00:00Z".to_string(),
            version: 1,
        })
        .unwrap(),
    )
    .unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    git(&["init"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Ferrus Test"]);
    git(&["config", "commit.gpgsign", "false"]);
    git(&["add", "ferrus.toml", "src/lib.rs", "src/deleted.rs"]);
    git(&["commit", "-m", "baseline"]);
    let baseline_tree = git(&["rev-parse", "HEAD^{tree}"]);

    let _cwd = CurrentDirGuard::change_to(root);
    project::record_task_status(
        "t-001",
        ".ferrus/tasks/t-001.md",
        project::TaskStatus::Executing,
    )
    .await
    .unwrap();
    schedule_task_baseline_pin("t-001", root, Some(&baseline_tree))
        .await
        .unwrap()
        .await
        .unwrap();

    let repository_view = project::task_repository_view("t-001")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        repository_view.status,
        project::RepositoryViewStatus::Available
    );
    assert!(repository_view.baseline_snapshot_id.is_some());
    assert_eq!(
        project::list_tasks().await.unwrap()[0].status,
        project::TaskStatus::Executing.as_str()
    );

    remove_sqlite_file_set(&data_dir.join(SIDECAR_FILE_NAME));
    let rebuilt_view = resolve_task_baseline(
        LocalGraphContext::load(false).await.unwrap(),
        data_dir.join(SIDECAR_FILE_NAME),
        "t-001",
        root,
        Some(&baseline_tree),
        Some(&repository_view),
    )
    .await
    .unwrap();
    assert_eq!(
        rebuilt_view.status,
        project::RepositoryViewStatus::Available
    );
    let rebuilt_sidecar = match open_for_query_at(&data_dir.join(SIDECAR_FILE_NAME)).unwrap() {
        OpenQuerySidecarResult::Ready(sidecar) => sidecar,
        _ => panic!("rebuilt baseline sidecar must be queryable"),
    };
    assert!(
        rebuilt_sidecar
            .snapshot(rebuilt_view.baseline_snapshot_id.as_ref().unwrap())
            .unwrap()
            .is_some()
    );
    drop(rebuilt_sidecar);

    std::fs::write(
        root.join("src/lib.rs"),
        "pub mod added;\npub struct OverlaySymbol;\n",
    )
    .unwrap();
    std::fs::write(root.join("src/added.rs"), "pub struct AddedSymbol;\n").unwrap();
    std::fs::remove_file(root.join("src/deleted.rs")).unwrap();
    let repository_view = refresh_task_overlay("t-001", root, &baseline_tree)
        .await
        .unwrap();
    assert!(repository_view.overlay_revision_id.is_some());

    let overlay_sidecar = match open_for_query_at(&data_dir.join(SIDECAR_FILE_NAME)).unwrap() {
        OpenQuerySidecarResult::Ready(sidecar) => sidecar,
        _ => panic!("refreshed task overlay must be queryable"),
    };
    let overlay_publication = overlay_sidecar
        .published_view(
            &LocalGraphContext::load(false).await.unwrap().repository,
            &task_overlay_view_name(&TaskViewId::new("t-001").unwrap()).unwrap(),
        )
        .unwrap()
        .unwrap();
    let overlay_metrics = overlay_sidecar
        .index_build_metrics(&overlay_publication.build_id)
        .unwrap()
        .unwrap();
    assert_eq!(overlay_metrics.parsed_files, 2);
    assert_eq!(overlay_metrics.reused_files, 1);
    drop(overlay_sidecar);

    let mut task_context = LocalGraphContext::load(false).await.unwrap();
    task_context.repository_view = Some(repository_view.clone());
    task_context.task_view_id = Some(TaskViewId::new("t-001").unwrap());
    task_context.root = root.to_path_buf();
    let task_freshness = task_context.freshness_comparison().unwrap();
    assert!(task_freshness.is_none());
    let task_status = status_response_at(
        &task_context,
        &data_dir.join(SIDECAR_FILE_NAME),
        task_freshness,
    )
    .unwrap();
    assert_eq!(task_status.freshness.freshness, Freshness::Unknown);
    assert_eq!(task_status.freshness.reason_codes, ["source_not_compared"]);
    let search = |text: &str| SearchRequest {
        scope: task_context
            .scope(default_budget(&task_context.config.query_limits).unwrap())
            .unwrap(),
        text: text.to_string(),
        node_kinds: vec![],
        paths: vec![],
        page: repository_graph::query::PageRequest { cursor: None },
    };
    let overlay_response = task_context
        .search(&search("OverlaySymbol"))
        .await
        .unwrap()
        .unwrap();
    assert!(!overlay_response.data.hits.is_empty());
    assert_eq!(
        overlay_response.task_view,
        Some(TaskViewEnvelope {
            task_view_id: TaskViewId::new("t-001").unwrap(),
            baseline_snapshot_id: repository_view.baseline_snapshot_id.clone().unwrap(),
            overlay_revision_id: repository_view.overlay_revision_id.clone(),
            lifecycle: TaskViewLifecycle::Mutable,
        })
    );
    assert!(
        task_context
            .search(&search("BaselineSymbol"))
            .await
            .unwrap()
            .unwrap()
            .data
            .hits
            .is_empty()
    );
    assert!(
        task_context
            .search(&search("DeletedSymbol"))
            .await
            .unwrap()
            .unwrap()
            .data
            .hits
            .is_empty()
    );
    assert!(
        !task_context
            .search(&search("AddedSymbol"))
            .await
            .unwrap()
            .unwrap()
            .data
            .hits
            .is_empty()
    );
    let context_request = ContextRequest {
        scope: task_context
            .scope(default_budget(&task_context.config.query_limits).unwrap())
            .unwrap(),
        seeds: vec![repository_graph::query::ContextSeed::Path(
            repository_graph::domain::RepoPath::new("src/lib.rs").unwrap(),
        )],
        policy: repository_graph::query::ContextPolicy {
            direction: repository_graph::query::EdgeDirection::Both,
            edge_kinds: vec![],
            include_unresolved: false,
            include_external: false,
        },
        page: repository_graph::query::PageRequest { cursor: None },
    };
    let context_response = task_context
        .context_with_snippets(&context_request, NonZeroU64::new(1024).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert!(
        context_response
            .data
            .snippets
            .iter()
            .any(|snippet| snippet.text.contains("OverlaySymbol"))
    );
    assert!(
        context_response
            .data
            .items
            .iter()
            .any(|item| item.path.as_str() == "src/added.rs")
    );
    assert_eq!(context_response.task_view, overlay_response.task_view);

    let frozen_view = repository_view
        .clone()
        .frozen(capture_worktree_tree(root).unwrap())
        .unwrap();
    task_context.repository_view = Some(frozen_view);
    std::fs::write(
        root.join("src/lib.rs"),
        "pub struct ChangedAfterSubmission;\n",
    )
    .unwrap();
    let frozen_response = task_context
        .context_with_snippets(&context_request, NonZeroU64::new(1024).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert!(
        frozen_response
            .data
            .snippets
            .iter()
            .any(|snippet| snippet.text.contains("OverlaySymbol"))
    );
    assert_eq!(
        frozen_response.task_view.unwrap().lifecycle,
        TaskViewLifecycle::FrozenSubmitted
    );

    project::record_task_status(
        "t-002",
        ".ferrus/tasks/t-002.md",
        project::TaskStatus::Executing,
    )
    .await
    .unwrap();
    schedule_task_baseline_pin("t-002", root, Some("invalid-tree"))
        .await
        .unwrap()
        .await
        .unwrap();
    assert_eq!(
        project::task_repository_view("t-002")
            .await
            .unwrap()
            .unwrap()
            .status,
        project::RepositoryViewStatus::Failed
    );
    assert_eq!(
        project::list_tasks()
            .await
            .unwrap()
            .into_iter()
            .find(|task| task.id == "t-002")
            .unwrap()
            .status,
        project::TaskStatus::Executing.as_str()
    );

    std::fs::write(root.join("src/lib.rs"), "pub struct BaselineSymbol;\n").unwrap();
    std::fs::write(root.join("src/deleted.rs"), "pub struct DeletedSymbol;\n").unwrap();
    std::fs::remove_file(root.join("src/added.rs")).unwrap();
    schedule_task_baseline_pin("t-002", root, Some(&baseline_tree))
        .await
        .unwrap()
        .await
        .unwrap();
    let retried_view = project::task_repository_view("t-002")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        retried_view.status,
        project::RepositoryViewStatus::Available
    );
    assert!(retried_view.baseline_snapshot_id.is_some());

    std::fs::write(root.join("src/lib.rs"), "pub struct AgentEdit;\n").unwrap();
    project::record_task_status(
        "t-003",
        ".ferrus/tasks/t-003.md",
        project::TaskStatus::Executing,
    )
    .await
    .unwrap();
    schedule_task_baseline_pin("t-003", root, Some(&baseline_tree))
        .await
        .unwrap()
        .await
        .unwrap();
    let changed_worktree_view = project::task_repository_view("t-003")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        changed_worktree_view.status,
        project::RepositoryViewStatus::Available
    );
    assert!(changed_worktree_view.baseline_snapshot_id.is_some());
    assert_eq!(
        project::list_tasks()
            .await
            .unwrap()
            .into_iter()
            .find(|task| task.id == "t-003")
            .unwrap()
            .status,
        project::TaskStatus::Executing.as_str()
    );
    let expected_pin = changed_worktree_view.clone();
    let frozen = changed_worktree_view
        .frozen(capture_worktree_tree(root).unwrap())
        .unwrap();
    project::record_task_repository_view("t-003", &frozen)
        .await
        .unwrap();
    assert!(
        !compare_and_record_task_baseline_at(
            &data_dir.join("ferrus.db"),
            "t-003",
            Some(&expected_pin),
            &expected_pin,
        )
        .await
        .unwrap()
    );
    assert_eq!(
        project::task_repository_view("t-003").await.unwrap(),
        Some(frozen)
    );
    let previous_agent = std::env::var_os(ENV_AGENT_ID);
    let previous_task = std::env::var_os(ENV_TASK_ID);
    // SAFETY: cwd_lock serializes Ferrus tests that mutate process-global runtime context.
    unsafe {
        std::env::set_var(ENV_AGENT_ID, "executor:codex:missing");
        std::env::set_var(ENV_TASK_ID, "t-missing");
    }
    let invalid_binding = match LocalGraphContext::load(false).await {
        Ok(_) => panic!("invalid task binding unexpectedly selected canonical context"),
        Err(error) => error,
    };
    assert!(invalid_binding.to_string().contains("not attached"));
    // SAFETY: the same lock remains held while the prior environment is restored.
    unsafe {
        match previous_agent {
            Some(value) => std::env::set_var(ENV_AGENT_ID, value),
            None => std::env::remove_var(ENV_AGENT_ID),
        }
        match previous_task {
            Some(value) => std::env::set_var(ENV_TASK_ID, value),
            None => std::env::remove_var(ENV_TASK_ID),
        }
    }
}

#[test]
fn context_snippets_are_deduplicated_hash_verified_and_stale_safe() {
    let directory = tempfile::tempdir().unwrap();
    let sidecar = directory.path().join(SIDECAR_FILE_NAME);
    let (mut context, request) = indexed_context(directory.path(), &sidecar);
    let response = context_response_at(&context, &sidecar, None, &request).unwrap();

    let enriched = attach_snippets_at(
        &context,
        &sidecar,
        &request,
        response.clone(),
        NonZeroU64::new(1024).unwrap(),
    )
    .unwrap();
    assert!(!enriched.data.snippets.is_empty());
    assert_eq!(enriched.data.items, response.data.items);
    assert_eq!(enriched.page, response.page);
    assert!(
        enriched
            .data
            .snippets
            .iter()
            .all(|snippet| snippet.text.contains("RuntimeTaskContext"))
    );
    let unique = enriched
        .data
        .snippets
        .iter()
        .map(|snippet| serde_json::to_string(&(snippet.path.clone(), snippet.span.clone())))
        .collect::<Result<BTreeSet<_>, _>>()
        .unwrap();
    assert_eq!(unique.len(), enriched.data.snippets.len());

    std::fs::write(
        directory.path().join("src/lib.rs"),
        b"pub struct Changed;\n",
    )
    .unwrap();
    context.config.query_limits.max_diagnostics = 1;
    let stale = attach_snippets_at(
        &context,
        &sidecar,
        &request,
        response,
        NonZeroU64::new(1024).unwrap(),
    )
    .unwrap();
    assert!(stale.data.snippets.is_empty());
    assert_eq!(stale.diagnostics.items.len(), 1);
    assert!(stale.diagnostics.summary.warning > 1);
    assert!(stale.diagnostics.truncated);
    assert!(
        stale
            .diagnostics
            .items
            .iter()
            .any(|diagnostic| diagnostic.code.as_str() == "content.changed")
    );
}
