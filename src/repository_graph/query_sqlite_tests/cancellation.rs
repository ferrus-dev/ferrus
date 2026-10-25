//! Tests for cancellation.

use super::*;

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
        page: super::super::super::query::PageRequest { cursor: None },
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
        page: super::super::super::query::PageRequest { cursor: None },
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
