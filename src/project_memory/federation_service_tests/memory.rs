//! Tests for memory.

use super::*;

#[test]
fn combined_context_crosses_only_the_exact_resolved_link_set() {
    let fixture = fixture();
    let graph = SqliteGraphQuery::new(
        &fixture.graph_sidecar,
        fixture.config.query_limits.clone(),
        Some(fixture.graph_freshness.clone()),
    );
    let memory =
        SqliteMemoryQuery::new(&fixture.memory_sidecar, fixture.config.query_limits.clone());
    let memory_search = memory
        .search(MemorySearchRequest {
            scope: MemoryQueryScope::current(
                fixture.project.clone(),
                MemoryRevisionSelector::Published(MemoryViewName::new("project").unwrap()),
                budget(&fixture.config),
            ),
            text: MemoryQueryText::new("src/lib.rs").unwrap(),
            entity_kinds: vec![],
            source_categories: vec![],
            page: MemoryPageRequest::default(),
        })
        .unwrap();
    let memory_entity = memory_search.hits[0].entity.id.clone();
    let service = service(&fixture, &graph, &memory);
    let response = service
        .context(FederatedContextRequest {
            scope: super::super::super::federation::FederatedScope::current(
                fixture.project.clone(),
                target(&fixture, ContextDomain::All),
                budget(&fixture.config),
            ),
            seeds: vec![FederatedContextSeed::MemoryEntity(memory_entity)],
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
    assert!(response.repository.is_some());
    assert!(response.memory.is_some());
    assert!(response.items.iter().any(|item| {
        matches!(item, FederatedContextItem::Repository(item) if item.path.as_str() == "src/lib.rs")
    }));
    assert!(
        response
            .items
            .iter()
            .any(|item| matches!(item, FederatedContextItem::Memory(_)))
    );
    assert!(!response.cross_domain_links.is_empty());
    assert!(response.cross_domain_links.iter().all(|relationship| {
        relationship.provenance.resolution == MemoryResolutionState::Resolved
    }));
    assert_eq!(
        response.repository.as_ref().unwrap().freshness.freshness,
        crate::repository_graph::domain::Freshness::Fresh
    );
    assert_eq!(
        response.memory.as_ref().unwrap().freshness.freshness,
        super::super::super::query::MemoryFreshness::Fresh
    );
}

#[test]
fn combined_context_charges_cross_domain_links_against_depth() {
    let fixture = fixture();
    let graph = SqliteGraphQuery::new(
        &fixture.graph_sidecar,
        fixture.config.query_limits.clone(),
        Some(fixture.graph_freshness.clone()),
    );
    let memory =
        SqliteMemoryQuery::new(&fixture.memory_sidecar, fixture.config.query_limits.clone());
    let memory_search = memory
        .search(MemorySearchRequest {
            scope: MemoryQueryScope::current(
                fixture.project.clone(),
                MemoryRevisionSelector::Published(MemoryViewName::new("project").unwrap()),
                budget(&fixture.config),
            ),
            text: MemoryQueryText::new("src/lib.rs").unwrap(),
            entity_kinds: vec![],
            source_categories: vec![],
            page: MemoryPageRequest::default(),
        })
        .unwrap();
    let memory_entity = memory_search.hits[0].entity.id.clone();
    let service = service(&fixture, &graph, &memory);
    let request = |max_results| {
        let mut query_budget = budget(&fixture.config);
        query_budget.max_depth = NonZeroU32::new(1).unwrap();
        query_budget.max_results = NonZeroU32::new(max_results).unwrap();
        FederatedContextRequest {
            scope: FederatedScope::current(
                fixture.project.clone(),
                target(&fixture, ContextDomain::All),
                query_budget,
            ),
            seeds: vec![FederatedContextSeed::MemoryEntity(memory_entity.clone())],
            repository_policy: ContextPolicy {
                direction: EdgeDirection::Both,
                edge_kinds: vec![],
                include_unresolved: false,
                include_external: false,
            },
            memory_policy: MemoryContextPolicy {
                direction: EdgeDirection::Outgoing,
                relationship_kinds: vec![],
                include_unresolved: false,
                include_stale: false,
                include_snippets: false,
            },
            cursor: None,
        }
    };

    let response = service.context(request(100)).unwrap();
    let repository_items = response
        .items
        .iter()
        .filter_map(|item| match item {
            FederatedContextItem::Repository(item) => Some(item),
            FederatedContextItem::Memory(_) => None,
        })
        .collect::<Vec<_>>();
    assert!(!repository_items.is_empty());
    assert!(repository_items.iter().all(|item| {
        item.selection_reasons
            .iter()
            .any(|reason| reason.kind == ContextSelectionKind::ExactSeed)
    }));
    assert_eq!(
        response
            .page
            .truncation
            .as_ref()
            .map(|truncation| truncation.reason),
        Some(MemoryTruncationReason::Depth)
    );

    let paged = service.context(request(1)).unwrap();
    assert_eq!(
        paged
            .page
            .truncation
            .as_ref()
            .map(|truncation| truncation.explored_depth),
        Some(1)
    );
}

#[test]
fn combined_context_applies_direction_and_kind_to_cross_domain_links() {
    let fixture = fixture();
    let graph = SqliteGraphQuery::new(
        &fixture.graph_sidecar,
        fixture.config.query_limits.clone(),
        Some(fixture.graph_freshness.clone()),
    );
    let memory =
        SqliteMemoryQuery::new(&fixture.memory_sidecar, fixture.config.query_limits.clone());
    let memory_search = memory
        .search(MemorySearchRequest {
            scope: MemoryQueryScope::current(
                fixture.project.clone(),
                MemoryRevisionSelector::Published(MemoryViewName::new("project").unwrap()),
                budget(&fixture.config),
            ),
            text: MemoryQueryText::new("src/lib.rs").unwrap(),
            entity_kinds: vec![],
            source_categories: vec![],
            page: MemoryPageRequest::default(),
        })
        .unwrap();
    let memory_entity = memory_search.hits[0].entity.id.clone();
    let service = service(&fixture, &graph, &memory);
    let request = |seeds, direction, relationship_kinds| FederatedContextRequest {
        scope: FederatedScope::current(
            fixture.project.clone(),
            target(&fixture, ContextDomain::All),
            budget(&fixture.config),
        ),
        seeds,
        repository_policy: ContextPolicy {
            direction: EdgeDirection::Both,
            edge_kinds: vec![],
            include_unresolved: false,
            include_external: false,
        },
        memory_policy: MemoryContextPolicy {
            direction,
            relationship_kinds,
            include_unresolved: false,
            include_stale: false,
            include_snippets: false,
        },
        cursor: None,
    };

    let forward = service
        .context(request(
            vec![FederatedContextSeed::MemoryEntity(memory_entity.clone())],
            EdgeDirection::Outgoing,
            vec![],
        ))
        .unwrap();
    let link = forward
        .cross_domain_links
        .first()
        .expect("outgoing memory traversal should cross an exact repository link");
    let snapshot_id = forward
        .repository
        .as_ref()
        .and_then(|state| state.snapshot_id.as_ref());
    let repository_seed = repository_seed_from_target(&link.target, snapshot_id)
        .expect("resolved fixture link should produce a repository seed");
    assert!(
        forward
            .items
            .iter()
            .any(|item| { matches!(item, FederatedContextItem::Repository(_)) })
    );

    let blocked_forward = service
        .context(request(
            vec![FederatedContextSeed::MemoryEntity(memory_entity)],
            EdgeDirection::Incoming,
            vec![],
        ))
        .unwrap();
    assert!(blocked_forward.cross_domain_links.is_empty());
    assert!(
        blocked_forward
            .items
            .iter()
            .all(|item| { !matches!(item, FederatedContextItem::Repository(_)) })
    );

    let reverse = service
        .context(request(
            vec![FederatedContextSeed::Repository(repository_seed.clone())],
            EdgeDirection::Incoming,
            vec![],
        ))
        .unwrap();
    assert!(!reverse.cross_domain_links.is_empty());
    assert!(
        reverse
            .items
            .iter()
            .any(|item| { matches!(item, FederatedContextItem::Memory(_)) })
    );

    let blocked_reverse = service
        .context(request(
            vec![FederatedContextSeed::Repository(repository_seed.clone())],
            EdgeDirection::Outgoing,
            vec![],
        ))
        .unwrap();
    assert!(blocked_reverse.cross_domain_links.is_empty());
    assert!(
        blocked_reverse
            .items
            .iter()
            .all(|item| { !matches!(item, FederatedContextItem::Memory(_)) })
    );

    let kind_filtered = service
        .context(request(
            vec![FederatedContextSeed::Repository(repository_seed)],
            EdgeDirection::Both,
            vec![MemoryRelationshipKind::Supersedes],
        ))
        .unwrap();
    assert!(kind_filtered.cross_domain_links.is_empty());
    assert!(
        kind_filtered
            .items
            .iter()
            .all(|item| { !matches!(item, FederatedContextItem::Memory(_)) })
    );
}
