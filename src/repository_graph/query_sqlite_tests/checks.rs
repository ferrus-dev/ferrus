//! Tests for checks.

use super::*;

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
            page: super::super::super::query::PageRequest { cursor: None },
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
            page: super::super::super::query::PageRequest { cursor: None },
        })
        .unwrap_err();
    assert_eq!(error.code, QueryErrorCode::IndexFailed);
}
