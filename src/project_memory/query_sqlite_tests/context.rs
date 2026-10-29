//! Tests for context.

use super::*;

#[test]
fn context_traversal_honors_relationship_direction() {
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
    let MemoryRelationshipTarget::MemoryEntity { entity_id: target } = relationship.target.clone()
    else {
        unreachable!("relationship was selected by target kind");
    };
    let source = relationship.source.clone();
    let relationship_id = relationship.id.clone();

    let context = |seed, direction| {
        query
            .context(MemoryContextRequest {
                scope: published_scope(project.clone(), &limits),
                seeds: vec![MemoryContextSeed::Entity(seed)],
                policy: super::super::super::query::MemoryContextPolicy {
                    direction,
                    relationship_kinds: vec![],
                    include_unresolved: false,
                    include_stale: false,
                    include_snippets: false,
                },
                page: MemoryPageRequest::default(),
            })
            .unwrap()
    };

    let outgoing_from_source = context(source.clone(), EdgeDirection::Outgoing);
    assert!(
        outgoing_from_source
            .items
            .iter()
            .any(|item| item.entity.id == target)
    );
    assert!(
        outgoing_from_source
            .relationships
            .iter()
            .any(|item| item.id == relationship_id)
    );

    let outgoing_from_target = context(target.clone(), EdgeDirection::Outgoing);
    assert!(
        outgoing_from_target
            .items
            .iter()
            .all(|item| item.entity.id != source)
    );
    assert!(
        outgoing_from_target
            .relationships
            .iter()
            .all(|item| item.id != relationship_id)
    );

    let incoming_from_target = context(target.clone(), EdgeDirection::Incoming);
    assert!(
        incoming_from_target
            .items
            .iter()
            .any(|item| item.entity.id == source)
    );
    assert!(
        incoming_from_target
            .relationships
            .iter()
            .any(|item| item.id == relationship_id)
    );

    let incoming_from_source = context(source, EdgeDirection::Incoming);
    assert!(
        incoming_from_source
            .items
            .iter()
            .all(|item| item.entity.id != target)
    );
    assert!(
        incoming_from_source
            .relationships
            .iter()
            .all(|item| item.id != relationship_id)
    );
}

#[test]
fn context_omits_an_optional_snippet_when_only_the_entity_fits() {
    let (_root, data, project, _) = indexed_fixture();
    let OpenMemoryQuerySidecarResult::Ready(sidecar) =
        open_for_query_at(&data.path().join(MEMORY_SIDECAR_FILE_NAME)).unwrap()
    else {
        panic!("memory query sidecar should be ready");
    };
    let limits = QueryLimitsConfig::default();
    let request = |scope, include_snippets| MemoryContextRequest {
        scope,
        seeds: vec![MemoryContextSeed::Milestone(
            MemoryRecordId::new("rg-test").unwrap(),
        )],
        policy: super::super::super::query::MemoryContextPolicy {
            direction: EdgeDirection::Both,
            relationship_kinds: vec![],
            include_unresolved: false,
            include_stale: false,
            include_snippets,
        },
        page: MemoryPageRequest::default(),
    };
    let base = SqliteMemoryQuery::new(&sidecar, limits.clone())
        .context(request(published_scope(project.clone(), &limits), false))
        .unwrap();
    let base_bytes = serialized_len(&base.items[0]).unwrap();
    let response_snippet_allowance = 8;
    let mut constrained_scope = published_scope(project, &limits);
    constrained_scope.budget.max_bytes =
        std::num::NonZeroU64::new(base_bytes + response_snippet_allowance).unwrap();
    let content = RecordingContent {
        max_requested: Cell::new(0),
        max_duration: Cell::new(Duration::ZERO),
    };

    let response = SqliteMemoryQuery::new(&sidecar, limits)
        .with_content(&content)
        .context(request(constrained_scope, true))
        .unwrap();

    assert_eq!(response.items.len(), 1);
    assert!(response.items[0].snippet.is_none());
    assert_eq!(content.max_requested.get(), response_snippet_allowance);
    assert_eq!(
        response.page.truncation.map(|value| value.reason),
        Some(MemoryTruncationReason::Bytes)
    );
}

#[test]
fn query_open_is_absent_or_read_only_and_never_creates_or_migrates() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join(MEMORY_SIDECAR_FILE_NAME);
    assert!(matches!(
        open_for_query_at(&path).unwrap(),
        OpenMemoryQuerySidecarResult::Absent
    ));
    assert!(!path.exists());

    drop(MemorySidecar::open(path.clone()).unwrap());
    let OpenMemoryQuerySidecarResult::Ready(sidecar) = open_for_query_at(&path).unwrap() else {
        panic!("current memory sidecar should be queryable");
    };
    assert!(
        sidecar
            .connection()
            .execute("DELETE FROM memory_builds", [])
            .is_err()
    );
}
