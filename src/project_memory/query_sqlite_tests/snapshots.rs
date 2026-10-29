//! Tests for snapshots.

use super::*;

#[test]
fn search_and_context_are_revision_bound_and_deterministic() {
    let (_root, data, project, revision_id) = indexed_fixture();
    let OpenMemoryQuerySidecarResult::Ready(sidecar) =
        open_for_query_at(&data.path().join(MEMORY_SIDECAR_FILE_NAME)).unwrap()
    else {
        panic!("memory query sidecar should be ready");
    };
    let limits = QueryLimitsConfig::default();
    let query = SqliteMemoryQuery::new(&sidecar, limits.clone());
    let search = query
        .search(MemorySearchRequest {
            scope: published_scope(project.clone(), &limits),
            text: super::super::super::domain::MemoryQueryText::new("sqlite").unwrap(),
            entity_kinds: vec![],
            source_categories: vec![],
            page: MemoryPageRequest::default(),
        })
        .unwrap();
    assert_eq!(search.revision_id, revision_id);
    assert!(!search.hits.is_empty());
    assert!(
        search
            .hits
            .windows(2)
            .all(|pair| { memory_hit_order(&pair[0], &pair[1]) != std::cmp::Ordering::Greater })
    );

    let context = query
        .context(MemoryContextRequest {
            scope: published_scope(project, &limits),
            seeds: vec![MemoryContextSeed::Milestone(
                MemoryRecordId::new("rg-test").unwrap(),
            )],
            policy: super::super::super::query::MemoryContextPolicy {
                direction: crate::repository_graph::query::EdgeDirection::Both,
                relationship_kinds: vec![],
                include_unresolved: false,
                include_stale: false,
                include_snippets: false,
            },
            page: MemoryPageRequest::default(),
        })
        .unwrap();
    assert!(context.items.len() >= 2);
    assert!(!context.relationships.is_empty());
}
