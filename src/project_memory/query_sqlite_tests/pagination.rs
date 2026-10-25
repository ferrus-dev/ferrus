//! Tests for pagination.

use super::*;

#[test]
fn search_filters_before_decoding_and_pages_ranked_unicode_matches() {
    let (_root, data, project, revision_id) = indexed_fixture();
    let sidecar = MemorySidecar::open_at(data.path()).unwrap();
    let limits = QueryLimitsConfig {
        max_results: 3,
        ..QueryLimitsConfig::default()
    };
    let query = SqliteMemoryQuery::new(&sidecar, limits.clone());
    let template = query.entities(&revision_id).unwrap().remove(0);
    let transaction = sidecar.connection().unchecked_transaction().unwrap();
    for index in 0..20 {
        let mut entity = template.clone();
        entity.id = MemoryEntityId::new(format!("needle-{index:03}")).unwrap();
        entity.data = MemoryEntityData::Specification {
            title: super::super::super::domain::MemoryTitle::new("N\u{00e9}edle").unwrap(),
        };
        entity.provenance.source_category = MemorySourceCategory::SpecificationStructure;
        transaction.execute(
            "INSERT INTO memory_entities(revision_id, id, kind, source_category, entity_json) VALUES (?1, ?2, 'specification', 'specification_structure', ?3)",
            params![revision_id.as_str(), entity.id.as_str(), serde_json::to_string(&entity).unwrap()],
        ).unwrap();
    }

    for (id, kind, category) in [
        ("excluded-kind", "decision", "specification_structure"),
        ("excluded-source", "specification", "runtime_provenance"),
    ] {
        transaction.execute(
            "INSERT INTO memory_entities(revision_id, id, kind, source_category, entity_json) VALUES (?1, ?2, ?3, ?4, 'corrupt JSON')",
            params![revision_id.as_str(), id, kind, category],
        ).unwrap();
    }

    transaction.commit().unwrap();
    let query = SqliteMemoryQuery::new(&sidecar, limits.clone());
    let mut request = MemorySearchRequest {
        scope: published_scope(project, &limits),
        text: super::super::super::domain::MemoryQueryText::new("  N\u{00c9}EDLE  ").unwrap(),
        entity_kinds: vec![MemoryEntityKind::Specification],
        source_categories: vec![MemorySourceCategory::SpecificationStructure],
        page: MemoryPageRequest::default(),
    };
    let mut ids = Vec::new();
    loop {
        let response = query.search(request.clone()).unwrap();
        assert!(response.hits.len() <= 3);
        ids.extend(
            response
                .hits
                .into_iter()
                .map(|hit| hit.entity.id.as_str().to_string()),
        );
        let Some(cursor) = response.page.next_cursor else {
            break;
        };
        assert!(ids.len() < 20);
        request.page.cursor = Some(cursor);
    }

    assert_eq!(
        ids,
        (0..20)
            .map(|index| format!("needle-{index:03}"))
            .collect::<Vec<_>>()
    );
}

#[test]
fn context_pages_relationships_beyond_the_first_page_result_limit() {
    let (_root, data, project, revision_id) = indexed_fixture();
    let OpenMemoryQuerySidecarResult::Ready(sidecar) =
        open_for_query_at(&data.path().join(MEMORY_SIDECAR_FILE_NAME)).unwrap()
    else {
        panic!("memory query sidecar should be ready");
    };
    let limits = QueryLimitsConfig::default();
    let query = SqliteMemoryQuery::new(&sidecar, limits.clone());
    let relationships = query.relationships(&revision_id).unwrap();
    let mut by_source = std::collections::BTreeMap::<_, Vec<_>>::new();
    for relationship in relationships {
        if matches!(
            relationship.target,
            MemoryRelationshipTarget::MemoryEntity { .. }
        ) {
            by_source
                .entry(relationship.source.clone())
                .or_default()
                .push(relationship);
        }
    }

    let (source, expected_relationships) = by_source
        .into_iter()
        .find(|(_, relationships)| relationships.len() > 1)
        .expect("fixture should contain a source with multiple outgoing relationships");
    let expected_relationship_ids = expected_relationships
        .iter()
        .map(|relationship| relationship.id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let expected_neighbor_ids = expected_relationships
        .iter()
        .filter_map(|relationship| match &relationship.target {
            MemoryRelationshipTarget::MemoryEntity { entity_id } => Some(entity_id.clone()),
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    let request = |cursor| {
        let mut scope = published_scope(project.clone(), &limits);
        scope.budget.max_results = std::num::NonZeroU32::new(1).unwrap();
        scope.budget.max_depth = std::num::NonZeroU32::new(1).unwrap();
        MemoryContextRequest {
            scope,
            seeds: vec![MemoryContextSeed::Entity(source.clone())],
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
    let mut cursor = None;
    let mut returned_relationship_ids = std::collections::BTreeSet::new();
    let mut returned_entity_ids = std::collections::BTreeSet::new();
    let mut pages = 0;
    loop {
        let response = query.context(request(cursor)).unwrap();
        returned_entity_ids.extend(response.items.into_iter().map(|item| item.entity.id));
        returned_relationship_ids.extend(
            response
                .relationships
                .into_iter()
                .map(|relationship| relationship.id),
        );
        pages += 1;
        assert!(pages < 32, "context cursor should make progress");
        let Some(next_cursor) = response.page.next_cursor else {
            break;
        };
        cursor = Some(next_cursor);
    }

    assert_eq!(returned_relationship_ids, expected_relationship_ids);
    assert!(expected_neighbor_ids.is_subset(&returned_entity_ids));
}
