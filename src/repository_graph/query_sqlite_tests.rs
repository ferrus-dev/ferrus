//! Graph query tests for scoped retrieval, snapshot-bound cursors, and resource limits.

use super::*;
use crate::repository_graph::{
    config::RepositoryGraphConfig,
    domain::{
        Availability, BuildId, BuildState, DiagnosticCode, DiagnosticSeverity, GraphBuild,
        GraphDiagnostic, PublishedViewName, RepositoryId, RepositoryNamespace, SnapshotId,
        SourceRevisionId,
    },
    index::{IndexCoordinator, IndexRequest, active_extractor_identities},
    source::{FilesystemRepositorySource, SourceDiscoveryContext},
    sqlite::{OpenSidecarResult, open_for_build_at},
    store::BuildFailure,
};

fn repository() -> RepositoryRef {
    RepositoryRef {
        namespace: RepositoryNamespace::new("local:test").unwrap(),
        repository_id: RepositoryId::new("root").unwrap(),
    }
}

fn indexed_fixture() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    Sidecar,
    RepositoryGraphConfig,
    FreshnessComparison,
) {
    indexed_fixture_with_extra_files(&[])
}

fn indexed_fixture_with_extra_files(
    extra_files: &[(&str, &str)],
) -> (
    tempfile::TempDir,
    tempfile::TempDir,
    Sidecar,
    RepositoryGraphConfig,
    FreshnessComparison,
) {
    let source_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(source_dir.path().join("src")).unwrap();
    std::fs::write(
        source_dir.path().join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\n",
    )
    .unwrap();
    std::fs::write(
        source_dir.path().join("src/lib.rs"),
        "pub struct RuntimeTaskContext;\npub fn claim_task() {}\n",
    )
    .unwrap();
    for (path, contents) in extra_files {
        let path = source_dir.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    let config = RepositoryGraphConfig::default();
    let identities = active_extractor_identities(&config).unwrap();
    let context = SourceDiscoveryContext::from_config(repository(), &config, &identities).unwrap();
    let source = FilesystemRepositorySource::discover(source_dir.path(), context).unwrap();
    let freshness_comparison = FreshnessComparison::from_manifest(source.manifest());
    let sidecar_dir = tempfile::tempdir().unwrap();
    let OpenSidecarResult::Ready(mut sidecar) =
        open_for_build_at(&sidecar_dir.path().join("repo-graph.db")).unwrap()
    else {
        panic!("new sidecar unexpectedly requires rebuild");
    };
    IndexCoordinator::new(&mut sidecar)
        .index(
            &source,
            &config,
            IndexRequest {
                build_id: BuildId::new("build-query").unwrap(),
                view_name: PublishedViewName::new("canonical").unwrap(),
                force_full: false,
            },
        )
        .unwrap();
    (
        source_dir,
        sidecar_dir,
        sidecar,
        config,
        freshness_comparison,
    )
}

fn scope(config: &RepositoryGraphConfig) -> super::super::query::QueryScope {
    super::super::query::QueryScope::current(
        repository(),
        SnapshotSelector::Published(PublishedViewName::new("canonical").unwrap()),
        default_budget(&config.query_limits).unwrap(),
    )
}

fn context_request(config: &RepositoryGraphConfig, seed: ContextSeed) -> ContextRequest {
    ContextRequest {
        scope: scope(config),
        seeds: vec![seed],
        policy: super::super::query::ContextPolicy {
            direction: EdgeDirection::Both,
            edge_kinds: vec![],
            include_unresolved: false,
            include_external: false,
        },
        page: super::super::query::PageRequest { cursor: None },
    }
}

fn fixture_symbol(query: &SqliteGraphQuery<'_>, config: &RepositoryGraphConfig) -> SemanticKey {
    query
        .search(&SearchRequest {
            scope: scope(config),
            text: "RuntimeTaskContext".to_string(),
            node_kinds: vec!["struct".to_string()],
            paths: vec![],
            page: super::super::query::PageRequest { cursor: None },
        })
        .unwrap()
        .data
        .hits
        .into_iter()
        .find_map(|hit| hit.semantic_key)
        .unwrap()
}

#[test]
fn context_resolves_seeds_ranks_deduplicates_and_preserves_evidence() {
    let (_source, _sidecar_dir, sidecar, config, comparison) = indexed_fixture();
    let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), Some(comparison));
    let symbol = ContextSeed::Symbol(fixture_symbol(&query, &config));
    let request = context_request(&config, symbol.clone());

    let first = query.context(&request).unwrap();
    let second = query.context(&request).unwrap();

    assert!(!first.data.items.is_empty());
    assert_eq!(
        first.data.items[0].selection_reasons[0].kind,
        ContextSelectionKind::ExactSeed
    );
    assert_eq!(
        serde_json::to_value(&first).unwrap(),
        serde_json::to_value(&second).unwrap()
    );
    let mut node_ids = BTreeSet::new();
    let mut selection_kinds = BTreeSet::new();
    for item in &first.data.items {
        assert!(node_ids.insert(item.node_id.clone()));
        let evidence = item.provenance.evidence.as_ref().unwrap();
        assert_eq!(item.path, evidence.path);
        assert_eq!(item.span, evidence.span);
        assert_eq!(item.content_identity, evidence.content_identity);
        let mut reasons = item.selection_reasons.clone();
        sort_context_reasons(&mut reasons);
        reasons.dedup();
        assert_eq!(item.selection_reasons, reasons);
        selection_kinds.extend(
            item.selection_reasons
                .iter()
                .map(|reason| context_selection_rank(reason.kind)),
        );
    }
    assert!(selection_kinds.contains(&context_selection_rank(ContextSelectionKind::Containment)));
    assert!(selection_kinds.contains(&context_selection_rank(ContextSelectionKind::Declaration)));

    let mut ordered = context_request(&config, symbol.clone());
    ordered
        .seeds
        .push(ContextSeed::Path(RepoPath::new("src/lib.rs").unwrap()));
    let mut reversed = ordered.clone();
    reversed.seeds.reverse();
    assert_eq!(
        serde_json::to_value(query.context(&ordered).unwrap()).unwrap(),
        serde_json::to_value(query.context(&reversed).unwrap()).unwrap()
    );
}

#[test]
fn context_pagination_is_snapshot_and_parameter_bound() {
    let (_source, _sidecar_dir, sidecar, mut config, _comparison) = indexed_fixture();
    config.query_limits.max_results = 2;
    let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
    let mut request = context_request(
        &config,
        ContextSeed::Symbol(fixture_symbol(&query, &config)),
    );

    let first = query.context(&request).unwrap();
    assert_eq!(first.data.items.len(), 2);
    assert_eq!(
        first.page.truncation.as_ref().unwrap().reason,
        TruncationReason::Results
    );
    request.page.cursor = first.page.next_cursor.clone();
    let second = query.context(&request).unwrap();
    assert!(first.data.items.iter().all(|left| {
        second
            .data
            .items
            .iter()
            .all(|right| left.node_id != right.node_id)
    }));

    let mut service_capped_depth = request.clone();
    service_capped_depth.scope.budget.max_depth = std::num::NonZeroU32::new(99).unwrap();
    assert!(query.context(&service_capped_depth).is_ok());

    let mut changed_depth = request.clone();
    changed_depth.scope.budget.max_depth = std::num::NonZeroU32::new(1).unwrap();
    let error = query.context(&changed_depth).unwrap_err();
    assert_eq!(error.code, QueryErrorCode::StaleCursor);

    request.policy.direction = EdgeDirection::Outgoing;
    let error = query.context(&request).unwrap_err();
    assert_eq!(error.code, QueryErrorCode::StaleCursor);
}

#[test]
fn context_depth_and_byte_budgets_return_terminal_truncation() {
    let (_source, _sidecar_dir, sidecar, mut config, _comparison) = indexed_fixture();
    config.query_limits.max_depth = 1;
    let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
    let seed = ContextSeed::Symbol(fixture_symbol(&query, &config));

    let depth = query
        .context(&context_request(&config, seed.clone()))
        .unwrap();
    assert_eq!(
        depth.page.truncation.as_ref().unwrap().reason,
        TruncationReason::Depth
    );
    assert!(depth.page.next_cursor.is_none());

    config.query_limits.max_bytes = 1;
    let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
    let bytes = query.context(&context_request(&config, seed)).unwrap();
    assert!(bytes.data.items.is_empty());
    assert_eq!(
        bytes.page.truncation.as_ref().unwrap().reason,
        TruncationReason::Bytes
    );
    assert!(bytes.page.next_cursor.is_none());
}

#[test]
fn context_rejects_empty_and_missing_seeds() {
    let (_source, _sidecar_dir, sidecar, config, _comparison) = indexed_fixture();
    let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
    let mut request = context_request(
        &config,
        ContextSeed::Node(NodeId::new("node:missing").unwrap()),
    );
    assert_eq!(
        query.context(&request).unwrap_err().code,
        QueryErrorCode::InvalidRequest
    );
    request.seeds.clear();
    assert_eq!(
        query.context(&request).unwrap_err().code,
        QueryErrorCode::InvalidRequest
    );
}

#[test]
fn context_policy_controls_unresolved_and_external_candidates() {
    for (resolution, include_unresolved, include_external) in
        [("unresolved", true, false), ("external", false, true)]
    {
        let (_source, _sidecar_dir, sidecar, config, _comparison) = indexed_fixture();
        let (file, classified, edge): (String, String, String) = sidecar
            .connection()
            .query_row(
                "SELECT edge.source_node_id, edge.target_node_id, edge.id \
                 FROM edges AS edge \
                 JOIN nodes AS source ON source.snapshot_id = edge.snapshot_id \
                                     AND source.id = edge.source_node_id \
                 JOIN nodes AS target ON target.snapshot_id = edge.snapshot_id \
                                     AND target.id = edge.target_node_id \
                 WHERE edge.kind = 'classified_as' \
                   AND source.kind = 'file' \
                   AND source.evidence_path = 'Cargo.toml' \
                   AND target.kind = 'configuration' \
                 LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        sidecar
            .connection()
            .execute(
                "UPDATE nodes SET resolution_state = ?1 WHERE id = ?2",
                params![resolution, classified],
            )
            .unwrap();
        sidecar
            .connection()
            .execute(
                "UPDATE edges SET resolution_state = ?1 WHERE id = ?2",
                params![resolution, edge],
            )
            .unwrap();
        let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
        let mut request = context_request(&config, ContextSeed::Node(NodeId::new(file).unwrap()));
        request.policy.direction = EdgeDirection::Outgoing;
        request.policy.edge_kinds = vec!["classified_as".to_string()];

        let excluded = query.context(&request).unwrap();
        assert!(
            excluded
                .data
                .items
                .iter()
                .all(|item| item.node_id.as_str() != classified)
        );

        request.policy.include_unresolved = include_unresolved;
        request.policy.include_external = include_external;
        let included = query.context(&request).unwrap();
        let classified_item = included
            .data
            .items
            .iter()
            .find(|item| item.node_id.as_str() == classified)
            .unwrap();
        assert!(
            classified_item
                .selection_reasons
                .iter()
                .any(|reason| reason.kind == ContextSelectionKind::Configuration)
        );
    }
}

#[test]
fn context_classifies_documentation_facts_ahead_of_generic_relationships() {
    let (_source, _sidecar_dir, sidecar, config, _comparison) =
        indexed_fixture_with_extra_files(&[("README.md", "# Fixture\n")]);
    let (file, document): (String, String) = sidecar
        .connection()
        .query_row(
            "SELECT edge.source_node_id, edge.target_node_id \
             FROM edges AS edge \
             JOIN nodes AS source ON source.snapshot_id = edge.snapshot_id \
                                 AND source.id = edge.source_node_id \
             JOIN nodes AS target ON target.snapshot_id = edge.snapshot_id \
                                 AND target.id = edge.target_node_id \
             WHERE edge.kind = 'classified_as' \
               AND source.evidence_path = 'README.md' \
               AND target.kind = 'document' \
             LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
    let mut request = context_request(&config, ContextSeed::Node(NodeId::new(file).unwrap()));
    request.policy.direction = EdgeDirection::Outgoing;
    request.policy.edge_kinds = vec!["classified_as".to_string()];

    let response = query.context(&request).unwrap();
    let item = response
        .data
        .items
        .iter()
        .find(|item| item.node_id.as_str() == document)
        .unwrap();

    assert!(
        item.selection_reasons
            .iter()
            .any(|reason| reason.kind == ContextSelectionKind::Documentation)
    );
}

#[test]
fn context_labels_resolved_import_targets_as_dependencies() {
    let (_source, _sidecar_dir, sidecar, config, _comparison) =
        indexed_fixture_with_extra_files(&[
            (
                "src/lib.rs",
                "pub mod api;\nuse crate::api::Api;\npub fn make() -> Api { Api }\n",
            ),
            ("src/api.rs", "pub struct Api;\n"),
        ]);
    let (source, target): (String, String) = sidecar
        .connection()
        .query_row(
            "SELECT source_node_id, target_node_id FROM edges \
             WHERE kind = 'imports' AND target_node_id IS NOT NULL \
             ORDER BY id LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
    let mut request = context_request(&config, ContextSeed::Node(NodeId::new(source).unwrap()));
    request.policy.direction = EdgeDirection::Outgoing;
    request.policy.edge_kinds = vec!["imports".to_string()];

    let response = query.context(&request).unwrap();
    let target = response
        .data
        .items
        .iter()
        .find(|item| item.node_id.as_str() == target)
        .unwrap();

    assert!(
        target
            .selection_reasons
            .iter()
            .any(|reason| reason.kind == ContextSelectionKind::ResolvedDependency)
    );
    assert_eq!(target.provenance.resolution, ResolutionState::Resolved);
}

#[test]
fn context_assembly_observes_an_expired_deadline() {
    let (_source, _sidecar_dir, sidecar, config, _comparison) = indexed_fixture();
    let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
    let request = context_request(
        &config,
        ContextSeed::Symbol(fixture_symbol(&query, &config)),
    );
    let resolved = query.resolve_scope(&request.scope).unwrap();
    let started = Instant::now()
        - Duration::from_millis(config.query_limits.max_duration_ms.saturating_add(1));
    let deadline = QueryDeadline::install(
        sidecar.connection(),
        started,
        Duration::from_millis(config.query_limits.max_duration_ms),
    )
    .unwrap();

    let assembly = query
        .assemble_context(&resolved, &request, started)
        .unwrap();
    drop(deadline);

    assert_eq!(assembly.truncation, Some(TruncationReason::Duration));
}

#[test]
fn context_returns_bounded_snapshot_diagnostics() {
    let (_source, _sidecar_dir, mut sidecar, mut config, _comparison) = indexed_fixture();
    let snapshot = sidecar
        .published_snapshot(&repository(), &PublishedViewName::new("canonical").unwrap())
        .unwrap()
        .unwrap();
    for code in ["context.a", "context.b"] {
        sidecar
            .record_diagnostic(&GraphDiagnostic {
                build_id: BuildId::new("build-query").unwrap(),
                snapshot_id: Some(snapshot.id.clone()),
                severity: DiagnosticSeverity::Warning,
                code: DiagnosticCode::new(code).unwrap(),
                location: None,
                metrics: BTreeMap::new(),
            })
            .unwrap();
    }
    config.query_limits.max_diagnostics = 1;
    let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
    let request = context_request(
        &config,
        ContextSeed::Symbol(fixture_symbol(&query, &config)),
    );

    let response = query.context(&request).unwrap();

    assert!(response.diagnostics.summary.warning >= 2);
    assert_eq!(response.diagnostics.items.len(), 1);
    assert!(response.diagnostics.truncated);
}

#[test]
fn search_show_and_neighborhood_return_evidence_and_provenance() {
    let (_source, _sidecar_dir, sidecar, config, freshness_comparison) = indexed_fixture();
    let query = SqliteGraphQuery::new(
        &sidecar,
        config.query_limits.clone(),
        Some(freshness_comparison.clone()),
    );
    let search = query
        .search(&SearchRequest {
            scope: scope(&config),
            text: "RuntimeTaskContext".to_string(),
            node_kinds: vec!["struct".to_string()],
            paths: vec![],
            page: super::super::query::PageRequest { cursor: None },
        })
        .unwrap();
    assert_eq!(search.freshness.freshness, Freshness::Fresh);
    assert_eq!(
        search.source_revision.manifest_digest,
        freshness_comparison.source_manifest_digest
    );
    assert!(!search.data.hits.is_empty());
    let hit = &search.data.hits[0];
    assert_eq!(hit.path.as_ref().unwrap().as_str(), "src/lib.rs");
    assert!(hit.span.is_some());
    assert_eq!(hit.provenance.resolution, ResolutionState::Resolved);

    let shown = query
        .show(&ShowRequest {
            scope: scope(&config),
            lookup: ShowLookup::Node(hit.node_id.clone()),
            page: super::super::query::PageRequest { cursor: None },
        })
        .unwrap();
    assert_eq!(shown.data.nodes.len(), 1);

    let neighborhood = query
        .neighborhood(&NeighborhoodRequest {
            scope: scope(&config),
            roots: vec![hit.node_id.clone()],
            direction: EdgeDirection::Both,
            edge_kinds: vec![],
            page: super::super::query::PageRequest { cursor: None },
        })
        .unwrap();
    assert!(
        neighborhood
            .data
            .nodes
            .iter()
            .any(|node| node.id == hit.node_id)
    );
}

#[test]
fn search_classifies_exact_matches_and_is_snapshot_deterministic() {
    let (_source, _sidecar_dir, sidecar, config, comparison) = indexed_fixture();
    let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), Some(comparison));

    for (text, expected) in [
        ("RuntimeTaskContext", SearchMatchKind::ExactNormalizedName),
        ("src/lib.rs", SearchMatchKind::ExactPath),
    ] {
        let request = SearchRequest {
            scope: scope(&config),
            text: text.to_string(),
            node_kinds: vec![],
            paths: vec![],
            page: super::super::query::PageRequest { cursor: None },
        };
        let first = query.search(&request).unwrap();
        let second = query.search(&request).unwrap();

        assert_eq!(first.data.hits.first().unwrap().match_kind, expected);
        assert_eq!(
            serde_json::to_value(first).unwrap(),
            serde_json::to_value(second).unwrap()
        );
    }

    let symbol = query
        .search(&SearchRequest {
            scope: scope(&config),
            text: "RuntimeTaskContext".to_string(),
            node_kinds: vec!["struct".to_string()],
            paths: vec![],
            page: super::super::query::PageRequest { cursor: None },
        })
        .unwrap()
        .data
        .hits
        .into_iter()
        .find_map(|hit| hit.semantic_key)
        .unwrap();
    let exact_symbol = query
        .search(&SearchRequest {
            scope: scope(&config),
            text: symbol.as_str().to_string(),
            node_kinds: vec![],
            paths: vec![],
            page: super::super::query::PageRequest { cursor: None },
        })
        .unwrap();
    assert_eq!(
        exact_symbol.data.hits.first().unwrap().match_kind,
        SearchMatchKind::ExactSemanticKey
    );
}

#[test]
fn previous_wire_versions_are_rejected_explicitly() {
    let (_source, _sidecar_dir, sidecar, config, _comparison) = indexed_fixture();
    let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
    let mut request_scope = scope(&config);
    request_scope.wire_version = QUERY_WIRE_VERSION - 1;

    let error = query
        .status(&StatusRequest {
            scope: request_scope,
        })
        .unwrap_err();

    assert_eq!(error.code, QueryErrorCode::UnsupportedWireVersion);
    assert_eq!(error.wire_version, QUERY_WIRE_VERSION);
}

#[test]
fn unbuilt_status_and_queries_distinguish_building_and_failed_attempts() {
    let sidecar_dir = tempfile::tempdir().unwrap();
    let OpenSidecarResult::Ready(mut sidecar) =
        open_for_build_at(&sidecar_dir.path().join("repo-graph.db")).unwrap()
    else {
        panic!("new sidecar unexpectedly requires rebuild");
    };
    let config = RepositoryGraphConfig::default();
    let build = GraphBuild {
        id: BuildId::new("build-in-progress").unwrap(),
        repository: repository(),
        source_revision_id: SourceRevisionId::new("revision-next").unwrap(),
        prospective_snapshot_id: SnapshotId::new("snapshot-next").unwrap(),
        state: BuildState::Building,
    };
    sidecar.start_build(&build).unwrap();

    let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
    let status = query
        .status(&StatusRequest {
            scope: scope(&config),
        })
        .unwrap();
    assert_eq!(status.data.availability, Availability::NotBuilt);
    assert_eq!(status.data.build_state, Some(BuildState::Building));
    assert_eq!(
        status.data.recommended_action,
        Some(RetrievalAction::WaitForBuild)
    );
    let error = query
        .search(&SearchRequest {
            scope: scope(&config),
            text: "anything".to_string(),
            node_kinds: vec![],
            paths: vec![],
            page: super::super::query::PageRequest { cursor: None },
        })
        .unwrap_err();
    assert_eq!(error.code, QueryErrorCode::IndexBuilding);

    drop(query);
    sidecar
        .fail_build(&BuildFailure {
            build_id: build.id,
            code: DiagnosticCode::new("index.failed").unwrap(),
        })
        .unwrap();
    let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
    let status = query
        .status(&StatusRequest {
            scope: scope(&config),
        })
        .unwrap();
    assert_eq!(status.data.build_state, Some(BuildState::Failed));
    assert_eq!(
        status.data.recommended_action,
        Some(RetrievalAction::RetryIndex)
    );
    let error = query
        .search(&SearchRequest {
            scope: scope(&config),
            text: "anything".to_string(),
            node_kinds: vec![],
            paths: vec![],
            page: super::super::query::PageRequest { cursor: None },
        })
        .unwrap_err();
    assert_eq!(error.code, QueryErrorCode::IndexFailed);
}

#[test]
fn diagnostics_are_deterministic_and_capped_independently() {
    let (_source, _sidecar_dir, mut sidecar, mut config, _comparison) = indexed_fixture();
    let snapshot = sidecar
        .published_snapshot(&repository(), &PublishedViewName::new("canonical").unwrap())
        .unwrap()
        .unwrap();
    for code in ["query.z", "query.a", "query.m"] {
        sidecar
            .record_diagnostic(&GraphDiagnostic {
                build_id: BuildId::new("build-query").unwrap(),
                snapshot_id: Some(snapshot.id.clone()),
                severity: DiagnosticSeverity::Warning,
                code: DiagnosticCode::new(code).unwrap(),
                location: None,
                metrics: BTreeMap::new(),
            })
            .unwrap();
    }
    config.query_limits.max_diagnostics = 2;
    let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
    let response = query
        .search(&SearchRequest {
            scope: scope(&config),
            text: "RuntimeTaskContext".to_string(),
            node_kinds: vec![],
            paths: vec![],
            page: super::super::query::PageRequest { cursor: None },
        })
        .unwrap();

    assert!(response.diagnostics.truncated);
    assert_eq!(response.diagnostics.items.len(), 2);
    assert!(response.diagnostics.summary.warning >= 3);
    assert!(
        response
            .diagnostics
            .items
            .windows(2)
            .all(|items| { items[0].code.as_str() <= items[1].code.as_str() })
    );
}

#[test]
fn path_filters_treat_like_metacharacters_as_literals() {
    let (_source, _sidecar_dir, sidecar, config, _comparison) =
        indexed_fixture_with_extra_files(&[
            ("src_a/lib.rs", "pub struct PathScopedMarker;\n"),
            ("srcXa/lib.rs", "pub struct PathScopedMarker;\n"),
            ("src%a/lib.rs", "pub struct PathScopedMarker;\n"),
        ]);
    let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);

    for prefix in ["src_a", "src%a"] {
        let response = query
            .search(&SearchRequest {
                scope: scope(&config),
                text: "PathScopedMarker".to_string(),
                node_kinds: vec!["struct".to_string()],
                paths: vec![RepoPath::new(prefix).unwrap()],
                page: super::super::query::PageRequest { cursor: None },
            })
            .unwrap();
        let expected_path = format!("{prefix}/lib.rs");

        assert!(!response.data.hits.is_empty());
        assert!(response.data.hits.iter().all(|hit| {
            hit.path
                .as_ref()
                .is_some_and(|path| path.as_str() == expected_path)
        }));
    }
}

#[test]
fn service_limits_cap_results_and_cursors_are_query_bound() {
    let (_source, _sidecar_dir, sidecar, mut config, _comparison) = indexed_fixture();
    config.query_limits.max_results = 1;
    let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
    let first = query
        .search(&SearchRequest {
            scope: scope(&config),
            text: "rust".to_string(),
            node_kinds: vec![],
            paths: vec![],
            page: super::super::query::PageRequest { cursor: None },
        })
        .unwrap();
    assert_eq!(first.data.hits.len(), 1);
    assert_eq!(
        first.page.truncation.as_ref().unwrap().reason,
        TruncationReason::Results
    );
    assert!(first.page.next_cursor.is_some());
    let cursor = first.page.next_cursor.unwrap();

    query
        .search(&SearchRequest {
            scope: scope(&config),
            text: "rust".to_string(),
            node_kinds: vec![],
            paths: vec![],
            page: super::super::query::PageRequest {
                cursor: Some(cursor.clone()),
            },
        })
        .unwrap();

    for (text, node_kinds, paths) in [
        ("RuntimeTaskContext", vec![], vec![]),
        ("rust", vec!["struct".to_string()], vec![]),
        ("rust", vec![], vec![RepoPath::new("src").unwrap()]),
    ] {
        let error = query
            .search(&SearchRequest {
                scope: scope(&config),
                text: text.to_string(),
                node_kinds,
                paths,
                page: super::super::query::PageRequest {
                    cursor: Some(cursor.clone()),
                },
            })
            .unwrap_err();
        assert_eq!(error.code, QueryErrorCode::StaleCursor);
    }

    let shown = query
        .show(&ShowRequest {
            scope: scope(&config),
            lookup: ShowLookup::Path(RepoPath::new("src/lib.rs").unwrap()),
            page: super::super::query::PageRequest { cursor: None },
        })
        .unwrap();
    let show_cursor = shown.page.next_cursor.unwrap();
    let error = query
        .show(&ShowRequest {
            scope: scope(&config),
            lookup: ShowLookup::Path(RepoPath::new("Cargo.toml").unwrap()),
            page: super::super::query::PageRequest {
                cursor: Some(show_cursor),
            },
        })
        .unwrap_err();
    assert_eq!(error.code, QueryErrorCode::StaleCursor);

    let error = query
        .search(&SearchRequest {
            scope: scope(&config),
            text: "rust".to_string(),
            node_kinds: vec![],
            paths: vec![],
            page: super::super::query::PageRequest {
                cursor: Some(PageCursor::new("cursor:00").unwrap()),
            },
        })
        .unwrap_err();
    assert_eq!(error.code, QueryErrorCode::StaleCursor);
}

#[test]
fn oversized_first_search_hit_returns_terminal_byte_truncation() {
    let (_source, _sidecar_dir, sidecar, mut config, _comparison) = indexed_fixture();
    config.query_limits.max_bytes = 1;
    let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);

    let response = query
        .search(&SearchRequest {
            scope: scope(&config),
            text: "RuntimeTaskContext".to_string(),
            node_kinds: vec![],
            paths: vec![],
            page: super::super::query::PageRequest { cursor: None },
        })
        .unwrap();

    assert!(response.data.hits.is_empty());
    assert_eq!(
        response.page.truncation.as_ref().unwrap().reason,
        TruncationReason::Bytes
    );
    assert!(response.page.next_cursor.is_none());
}

#[test]
fn edge_limit_is_applied_after_seen_edges_are_excluded() {
    let (_source, _sidecar_dir, sidecar, config, _comparison) = indexed_fixture();
    let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
    let (snapshot, node): (String, String) = sidecar
        .connection()
        .query_row(
            "SELECT edges.snapshot_id, nodes.id \
             FROM nodes \
             JOIN edges ON edges.snapshot_id = nodes.snapshot_id \
               AND (edges.source_node_id = nodes.id OR edges.target_node_id = nodes.id) \
             GROUP BY edges.snapshot_id, nodes.id \
             HAVING COUNT(*) >= 2 \
             ORDER BY nodes.id \
             LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let snapshot = SnapshotId::new(snapshot).unwrap();
    let node = NodeId::new(node).unwrap();
    let all = query
        .edges(
            &snapshot,
            &node,
            EdgeDirection::Both,
            &[],
            std::iter::empty::<&EdgeId>(),
            16,
        )
        .unwrap();
    assert!(all.len() >= 2);
    let seen = [all[0].id.clone()];

    let unseen = query
        .edges(&snapshot, &node, EdgeDirection::Both, &[], seen.iter(), 1)
        .unwrap();

    assert_eq!(unseen.first().map(|edge| &edge.id), Some(&all[1].id));
    assert!(unseen.iter().all(|edge| edge.id != seen[0]));
}

#[test]
fn sqlite_search_execution_observes_an_expired_deadline() {
    let (_source, _sidecar_dir, sidecar, config, _comparison) = indexed_fixture();
    let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
    let requested_scope = scope(&config);
    let resolved_scope = query.resolve_scope(&requested_scope).unwrap();
    let request = SearchRequest {
        scope: requested_scope,
        text: "missing-low-selectivity-term".to_string(),
        node_kinds: vec![],
        paths: vec![],
        page: super::super::query::PageRequest { cursor: None },
    };
    let started = Instant::now()
        - Duration::from_millis(config.query_limits.max_duration_ms.saturating_add(1));

    let rows = query
        .search_rows(&resolved_scope, &request, 0, started)
        .unwrap();

    assert!(rows.deadline_exceeded);
    assert!(rows.rows.is_empty());
}

#[test]
fn sqlite_show_execution_observes_an_expired_deadline() {
    let source = (0..300)
        .map(|index| format!("pub struct Type{index};\n"))
        .collect::<String>();
    let extra_files = [("src/many.rs", source.as_str())];
    let (_source, _sidecar_dir, sidecar, config, _comparison) =
        indexed_fixture_with_extra_files(&extra_files);
    let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
    let requested_scope = scope(&config);
    let resolved_scope = query.resolve_scope(&requested_scope).unwrap();
    let request = ShowRequest {
        scope: requested_scope,
        lookup: ShowLookup::Path(RepoPath::new("src/many.rs").unwrap()),
        page: super::super::query::PageRequest { cursor: None },
    };
    let started = Instant::now()
        - Duration::from_millis(config.query_limits.max_duration_ms.saturating_add(1));

    let rows = query
        .show_rows(&resolved_scope, &request, 0, started)
        .unwrap();

    assert!(rows.deadline_exceeded);
    assert!(rows.rows.is_empty());
}

#[test]
fn status_observes_an_expired_deadline() {
    let (_source, _sidecar_dir, sidecar, config, _comparison) = indexed_fixture();
    let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
    let started = Instant::now()
        - Duration::from_millis(config.query_limits.max_duration_ms.saturating_add(1));

    let response = query
        .status_at(
            &StatusRequest {
                scope: scope(&config),
            },
            started,
        )
        .unwrap();

    assert_eq!(
        response.page.truncation.unwrap().reason,
        TruncationReason::Duration
    );
    assert!(response.data.statistics.is_none());
}

#[test]
fn status_reports_counts_and_missing_show_is_actionable() {
    let (_source, _sidecar_dir, sidecar, config, _comparison) = indexed_fixture();
    let query = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), None);
    let status = query
        .status(&StatusRequest {
            scope: scope(&config),
        })
        .unwrap();
    let statistics = status.data.statistics.unwrap();
    assert_eq!(statistics.files, 2);
    assert!(statistics.nodes > 0);
    assert!(statistics.edges > 0);

    let error = query
        .show(&ShowRequest {
            scope: scope(&config),
            lookup: ShowLookup::Node(NodeId::new("node:missing").unwrap()),
            page: super::super::query::PageRequest { cursor: None },
        })
        .unwrap_err();
    assert_eq!(error.code, QueryErrorCode::InvalidRequest);
}

#[test]
fn analysis_and_extractor_changes_mark_the_snapshot_stale() {
    let (_source, _sidecar_dir, sidecar, config, current) = indexed_fixture();
    let snapshot = sidecar
        .published_snapshot(&repository(), &PublishedViewName::new("canonical").unwrap())
        .unwrap()
        .unwrap();
    for (comparison, reason) in [
        (
            FreshnessComparison {
                analysis_config_digest: Digest::new("sha256", "aa").unwrap(),
                ..current.clone()
            },
            "analysis_config_changed",
        ),
        (
            FreshnessComparison {
                extractor_set_digest: Digest::new("sha256", "bb").unwrap(),
                ..current.clone()
            },
            "extractor_set_changed",
        ),
    ] {
        let status = SqliteGraphQuery::new(&sidecar, config.query_limits.clone(), Some(comparison))
            .status(&StatusRequest {
                scope: scope(&config),
            })
            .unwrap();

        assert_eq!(status.freshness.freshness, Freshness::Stale);
        assert_eq!(
            status.freshness.compared_manifest.as_ref(),
            Some(&snapshot.source_manifest_digest)
        );
        assert_eq!(status.freshness.reason_codes, vec![reason]);
    }
}
