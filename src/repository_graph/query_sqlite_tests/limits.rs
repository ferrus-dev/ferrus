//! Tests for limits.

use super::*;

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
            page: super::super::super::query::PageRequest { cursor: None },
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
            page: super::super::super::query::PageRequest { cursor: None },
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
