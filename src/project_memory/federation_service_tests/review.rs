//! Tests for review.

use super::*;

#[test]
fn approved_history_reduces_raw_artifact_reading_without_displacing_source_evidence() {
    let corpus: EvaluationCorpus = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/project_memory_eval/cases.json"
    )))
    .unwrap();
    assert_eq!(corpus.version, 1);
    let fixture = fixture();
    let graph = SqliteGraphQuery::new(
        &fixture.graph_sidecar,
        fixture.config.query_limits.clone(),
        Some(fixture.graph_freshness.clone()),
    );
    let memory =
        SqliteMemoryQuery::new(&fixture.memory_sidecar, fixture.config.query_limits.clone());
    let service = service(&fixture, &graph, &memory);

    for case in corpus.cases {
        assert!(case.baseline_raw_artifact_reads > 0, "{}", case.id);
        let search = memory
            .search(MemorySearchRequest {
                scope: MemoryQueryScope::current(
                    fixture.project.clone(),
                    MemoryRevisionSelector::Published(MemoryViewName::new("project").unwrap()),
                    budget(&fixture.config),
                ),
                text: MemoryQueryText::new(case.search_query).unwrap(),
                entity_kinds: vec![],
                source_categories: vec![],
                page: MemoryPageRequest::default(),
            })
            .unwrap();
        let historical = search
            .hits
            .iter()
            .find(|hit| {
                matches!(
                    &hit.entity.data,
                    MemoryEntityData::Outcome { text }
                        if text.as_str().contains(&case.expected_memory_text)
                )
            })
            .unwrap_or_else(|| panic!("{} did not find approved history", case.id));
        assert_eq!(
            historical.entity.provenance.source_category,
            MemorySourceCategory::ApprovedOutcome
        );

        let response = service
            .context(FederatedContextRequest {
                scope: FederatedScope::current(
                    fixture.project.clone(),
                    target(&fixture, ContextDomain::All),
                    budget(&fixture.config),
                ),
                seeds: vec![FederatedContextSeed::MemoryEntity(
                    historical.entity.id.clone(),
                )],
                repository_policy: ContextPolicy {
                    direction: EdgeDirection::Both,
                    edge_kinds: vec![],
                    include_unresolved: false,
                    include_external: false,
                },
                memory_policy: MemoryContextPolicy {
                    direction: EdgeDirection::Both,
                    relationship_kinds: vec![],
                    include_unresolved: false,
                    include_stale: false,
                    include_snippets: false,
                },
                cursor: None,
            })
            .unwrap();
        assert!(matches!(
            response.items.first(),
            Some(FederatedContextItem::Repository(_))
        ));
        let repository_position = response
            .items
            .iter()
            .position(|item| {
                matches!(
                    item,
                    FederatedContextItem::Repository(item)
                        if item.path.as_str() == case.expected_repository_path
                )
            })
            .unwrap();
        let memory_position = response
            .items
            .iter()
            .position(|item| {
                matches!(
                    item,
                    FederatedContextItem::Memory(item)
                        if item.entity.id == historical.entity.id
                )
            })
            .unwrap();
        assert!(repository_position < memory_position);
        assert!(
            response
                .cross_domain_links
                .iter()
                .all(|link| { link.provenance.resolution == MemoryResolutionState::Resolved })
        );
        assert_eq!(
            response.repository.as_ref().unwrap().freshness.freshness,
            crate::repository_graph::domain::Freshness::Fresh
        );
        assert_eq!(
            response.memory.as_ref().unwrap().freshness.freshness,
            super::super::super::query::MemoryFreshness::Fresh
        );
        let serialized = serde_json::to_string(&response).unwrap();
        for marker in case.forbidden_raw_markers {
            assert!(
                !serialized.contains(&marker),
                "{} exposed {marker}",
                case.id
            );
        }
    }
}
