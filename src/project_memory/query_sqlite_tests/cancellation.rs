//! Tests for cancellation.

use super::*;

#[test]
fn search_candidate_ranking_stops_when_the_deadline_expires() {
    let (_root, data, project, revision_id) = indexed_fixture();
    let OpenMemoryQuerySidecarResult::Ready(sidecar) =
        open_for_query_at(&data.path().join(MEMORY_SIDECAR_FILE_NAME)).unwrap()
    else {
        panic!("memory query sidecar should be ready");
    };
    let limits = QueryLimitsConfig::default();
    let query = SqliteMemoryQuery::new(&sidecar, limits.clone());
    let request = MemorySearchRequest {
        scope: published_scope(project, &limits),
        text: super::super::super::domain::MemoryQueryText::new("sqlite").unwrap(),
        entity_kinds: vec![],
        source_categories: vec![],
        page: MemoryPageRequest::default(),
    };
    let entities = query.entities(&revision_id).unwrap();
    let mut deadline_checks = 0;

    let (hits, _, timed_out) =
        rank_memory_search_hits(entities.into_iter().map(Ok), &request, 10, || {
            deadline_checks += 1;
            deadline_checks > 1
        })
        .unwrap();

    assert!(timed_out);
    assert_eq!(deadline_checks, 2);
    assert!(hits.len() <= 1);
}

#[test]
fn memory_scope_resolution_stops_at_the_sqlite_lock_deadline() {
    let (_root, data, project, _revision_id) = indexed_fixture();
    let sidecar = MemorySidecar::open_at(data.path()).unwrap();
    sidecar
        .connection()
        .query_row("PRAGMA journal_mode = DELETE", [], |row| {
            row.get::<_, String>(0)
        })
        .unwrap();
    let blocker = Connection::open(data.path().join(MEMORY_SIDECAR_FILE_NAME)).unwrap();
    blocker
        .execute_batch(
            "BEGIN EXCLUSIVE;
             UPDATE memory_published_views SET generation = generation;",
        )
        .unwrap();
    let limits = QueryLimitsConfig::default();
    let mut scope = published_scope(project, &limits);
    scope.budget.max_duration_ms = NonZeroU64::new(25).unwrap();
    let query = SqliteMemoryQuery::new(&sidecar, limits);
    let started = Instant::now();

    let result = query.search(MemorySearchRequest {
        scope,
        text: super::super::super::domain::MemoryQueryText::new("sqlite").unwrap(),
        entity_kinds: vec![],
        source_categories: vec![],
        page: MemoryPageRequest::default(),
    });

    assert!(matches!(
        result,
        Err(MemoryQueryError::BudgetExceeded(
            MemoryTruncationReason::Duration
        ))
    ));
    assert!(started.elapsed() < Duration::from_secs(1));
    blocker.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn context_stops_before_accepting_a_snippet_that_exceeds_the_deadline() {
    let (_root, data, project, _) = indexed_fixture();
    let OpenMemoryQuerySidecarResult::Ready(sidecar) =
        open_for_query_at(&data.path().join(MEMORY_SIDECAR_FILE_NAME)).unwrap()
    else {
        panic!("memory query sidecar should be ready");
    };
    let limits = QueryLimitsConfig::default();
    let response = SqliteMemoryQuery::new(&sidecar, limits.clone())
        .with_content(&ExpiredContent)
        .context(MemoryContextRequest {
            scope: published_scope(project, &limits),
            seeds: vec![MemoryContextSeed::Milestone(
                MemoryRecordId::new("rg-test").unwrap(),
            )],
            policy: super::super::super::query::MemoryContextPolicy {
                direction: EdgeDirection::Both,
                relationship_kinds: vec![],
                include_unresolved: false,
                include_stale: false,
                include_snippets: true,
            },
            page: MemoryPageRequest::default(),
        })
        .unwrap();

    assert!(response.items.is_empty());
    assert_eq!(
        response.page.truncation.map(|value| value.reason),
        Some(MemoryTruncationReason::Duration)
    );
}
