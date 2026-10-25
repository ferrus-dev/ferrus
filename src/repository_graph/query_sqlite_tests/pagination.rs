//! Tests for pagination.

use super::*;

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
            page: super::super::super::query::PageRequest { cursor: None },
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
            page: super::super::super::query::PageRequest {
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
                page: super::super::super::query::PageRequest {
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
            page: super::super::super::query::PageRequest { cursor: None },
        })
        .unwrap();
    let show_cursor = shown.page.next_cursor.unwrap();
    let error = query
        .show(&ShowRequest {
            scope: scope(&config),
            lookup: ShowLookup::Path(RepoPath::new("Cargo.toml").unwrap()),
            page: super::super::super::query::PageRequest {
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
            page: super::super::super::query::PageRequest {
                cursor: Some(PageCursor::new("cursor:00").unwrap()),
            },
        })
        .unwrap_err();
    assert_eq!(error.code, QueryErrorCode::StaleCursor);
}
