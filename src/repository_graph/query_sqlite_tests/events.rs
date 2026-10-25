//! Tests for events.

use super::*;

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
            page: super::super::super::query::PageRequest { cursor: None },
        })
        .unwrap_err();
    assert_eq!(error.code, QueryErrorCode::InvalidRequest);
}
