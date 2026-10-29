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

#[path = "query_sqlite_tests/cancellation.rs"]
mod cancellation_tests;
#[path = "query_sqlite_tests/checks.rs"]
mod checks_tests;
#[path = "query_sqlite_tests/context.rs"]
mod context_tests;
#[path = "query_sqlite_tests/events.rs"]
mod events_tests;
#[path = "query_sqlite_tests/files.rs"]
mod files_tests;
#[path = "query_sqlite_tests/limits.rs"]
mod limits_tests;
#[path = "query_sqlite_tests/pagination.rs"]
mod pagination_tests;
#[path = "query_sqlite_tests/snapshots.rs"]
mod snapshots_tests;

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
