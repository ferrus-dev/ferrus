//! Tests for limits.

use super::*;

#[test]
fn effective_budget_clamps_every_caller_value() {
    use std::num::{NonZeroU32, NonZeroU64};

    let requested = MemoryQueryBudget {
        max_results: NonZeroU32::new(1_000).unwrap(),
        max_bytes: NonZeroU64::new(1_000_000).unwrap(),
        max_snippet_bytes: NonZeroU64::new(1_000_000).unwrap(),
        max_depth: NonZeroU32::new(100).unwrap(),
        max_duration_ms: NonZeroU64::new(100_000).unwrap(),
        max_diagnostics: NonZeroU32::new(1_000).unwrap(),
    };
    let limits = QueryLimitsConfig::default();
    let effective = EffectiveBudget::new(&requested, &limits);
    assert_eq!(effective.max_results, limits.max_results);
    assert_eq!(effective.max_bytes, limits.max_bytes);
    assert_eq!(effective.max_snippet_bytes, limits.max_snippet_bytes);
    assert_eq!(effective.max_depth, limits.max_depth);
    assert_eq!(effective.max_duration_ms, limits.max_duration_ms);
    assert_eq!(effective.max_diagnostics, limits.max_diagnostics);
}

#[test]
fn bounded_ranking_keeps_global_order_without_retaining_every_match() {
    let (_root, data, project, revision_id) = indexed_fixture();
    let sidecar = MemorySidecar::open_at(data.path()).unwrap();
    let limits = QueryLimitsConfig::default();
    let query = SqliteMemoryQuery::new(&sidecar, limits.clone());
    let template = query.entities(&revision_id).unwrap().remove(0);
    let request = MemorySearchRequest {
        scope: published_scope(project, &limits),
        text: super::super::super::domain::MemoryQueryText::new("needle").unwrap(),
        entity_kinds: vec![],
        source_categories: vec![],
        page: MemoryPageRequest::default(),
    };
    let entities = (0..10_000).rev().map(|index| {
        let mut entity = template.clone();
        entity.id = MemoryEntityId::new(format!("entity-{index:05}")).unwrap();
        entity.data = MemoryEntityData::Specification {
            title: super::super::super::domain::MemoryTitle::new("Needle").unwrap(),
        };
        Ok(entity)
    });
    let (hits, total, timed_out) =
        rank_memory_search_hits(entities, &request, 4, || false).unwrap();
    assert_eq!(total, 10_000);
    assert!(!timed_out);
    assert_eq!(
        hits.iter()
            .map(|hit| hit.entity.id.as_str())
            .collect::<Vec<_>>(),
        vec![
            "entity-00000",
            "entity-00001",
            "entity-00002",
            "entity-00003"
        ]
    );
}

#[test]
fn context_counts_relationships_against_the_effective_result_limit() {
    let (_root, data, project, revision_id) = indexed_fixture();
    let OpenMemoryQuerySidecarResult::Ready(sidecar) =
        open_for_query_at(&data.path().join(MEMORY_SIDECAR_FILE_NAME)).unwrap()
    else {
        panic!("memory query sidecar should be ready");
    };
    let limits = QueryLimitsConfig::default();
    let query = SqliteMemoryQuery::new(&sidecar, limits.clone());
    let relationship = query
        .relationships(&revision_id)
        .unwrap()
        .into_iter()
        .find(|relationship| {
            matches!(
                relationship.target,
                MemoryRelationshipTarget::MemoryEntity { ref entity_id }
                    if entity_id != &relationship.source
            )
        })
        .expect("fixture should contain a directed entity relationship");
    let relationship_id = relationship.id.clone();
    let request = |cursor| {
        let mut scope = published_scope(project.clone(), &limits);
        scope.budget.max_results = std::num::NonZeroU32::new(1).unwrap();
        MemoryContextRequest {
            scope,
            seeds: vec![MemoryContextSeed::Entity(relationship.source.clone())],
            policy: super::super::super::query::MemoryContextPolicy {
                direction: crate::repository_graph::query::EdgeDirection::Outgoing,
                relationship_kinds: vec![],
                include_unresolved: false,
                include_stale: false,
                include_snippets: false,
            },
            page: MemoryPageRequest { cursor },
        }
    };
    let mut response = query.context(request(None)).unwrap();

    assert!(
        response
            .items
            .len()
            .saturating_add(response.relationships.len())
            <= 1
    );
    assert_eq!(response.relationships.len(), 0);
    assert_eq!(
        response.page.truncation.as_ref().map(|value| value.reason),
        Some(MemoryTruncationReason::Results)
    );
    let mut pages = 1;
    while response.relationships.is_empty() {
        let cursor = response
            .page
            .next_cursor
            .clone()
            .expect("context relationships should remain pageable");
        response = query.context(request(Some(cursor))).unwrap();
        pages += 1;
        assert!(pages < 32, "context cursor should make progress");
    }

    assert_eq!(response.relationships.len(), 1);
    assert_eq!(response.relationships[0].id, relationship_id);
}

#[test]
fn context_candidate_cap_allows_edges_between_selected_entities() {
    assert!(!context_candidate_limit_reached(
        MAX_CONTEXT_CANDIDATES,
        true,
    ));
    assert!(context_candidate_limit_reached(
        MAX_CONTEXT_CANDIDATES,
        false,
    ));
}

#[test]
fn context_counts_relationships_against_the_effective_byte_limit() {
    let (_root, data, project, _) = indexed_fixture();
    let OpenMemoryQuerySidecarResult::Ready(sidecar) =
        open_for_query_at(&data.path().join(MEMORY_SIDECAR_FILE_NAME)).unwrap()
    else {
        panic!("memory query sidecar should be ready");
    };
    let limits = QueryLimitsConfig::default();
    let query = SqliteMemoryQuery::new(&sidecar, limits.clone());
    let request = |scope| MemoryContextRequest {
        scope,
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
    };
    let complete = query
        .context(request(published_scope(project.clone(), &limits)))
        .unwrap();
    assert!(!complete.relationships.is_empty());
    let item_bytes = complete
        .items
        .iter()
        .map(serialized_len)
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .into_iter()
        .sum::<u64>();
    let mut constrained_scope = published_scope(project, &limits);
    constrained_scope.budget.max_bytes = std::num::NonZeroU64::new(item_bytes).unwrap();

    let constrained = query.context(request(constrained_scope)).unwrap();
    let returned_bytes = constrained
        .items
        .iter()
        .map(serialized_len)
        .chain(constrained.relationships.iter().map(serialized_len))
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .into_iter()
        .sum::<u64>();
    assert!(returned_bytes <= item_bytes);
    assert_eq!(
        constrained
            .page
            .truncation
            .as_ref()
            .map(|value| value.returned_bytes),
        Some(returned_bytes)
    );
    assert_eq!(
        constrained.page.truncation.map(|value| value.reason),
        Some(MemoryTruncationReason::Bytes)
    );
}

#[test]
fn context_snippets_use_the_verified_content_boundary_and_effective_cap() {
    let (_root, data, project, _) = indexed_fixture();
    let OpenMemoryQuerySidecarResult::Ready(sidecar) =
        open_for_query_at(&data.path().join(MEMORY_SIDECAR_FILE_NAME)).unwrap()
    else {
        panic!("memory query sidecar should be ready");
    };
    let limits = QueryLimitsConfig {
        max_snippet_bytes: 7,
        ..QueryLimitsConfig::default()
    };
    let content = RecordingContent {
        max_requested: Cell::new(0),
        max_duration: Cell::new(Duration::ZERO),
    };
    let query = SqliteMemoryQuery::new(&sidecar, limits.clone()).with_content(&content);
    let response = query
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
                include_snippets: true,
            },
            page: MemoryPageRequest::default(),
        })
        .unwrap();
    let snippet_bytes = response
        .items
        .iter()
        .filter_map(|item| item.snippet.as_ref())
        .map(|snippet| snippet.text.len())
        .sum::<usize>();
    assert_eq!(snippet_bytes, 7);
    assert_eq!(content.max_requested.get(), 7);
    assert!(content.max_duration.get() <= Duration::from_millis(limits.max_duration_ms));
    assert!(!content.max_duration.get().is_zero());
}
