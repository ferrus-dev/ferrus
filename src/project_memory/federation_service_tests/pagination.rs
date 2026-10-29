//! Tests for pagination.

use super::*;

#[test]
fn cursor_is_bound_to_both_domain_revisions() {
    let cursor = encode_cursor("search", "request", "revision-one", 4).unwrap();
    let decoded = decode_cursor(Some(&cursor), "search", "request")
        .unwrap()
        .unwrap();
    assert_eq!(
        validate_cursor_revision(Some(&decoded), "revision-two"),
        Err(MemoryQueryError::StaleCursor)
    );
}

#[test]
fn supplemental_byte_truncation_does_not_advertise_an_unusable_cursor() {
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
    let request = |query_budget, cursor| FederatedContextRequest {
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
            direction: EdgeDirection::Both,
            relationship_kinds: vec![],
            include_unresolved: false,
            include_stale: false,
            include_snippets: false,
        },
        cursor,
    };
    let complete = service
        .context(request(budget(&fixture.config), None))
        .unwrap();
    assert!(!complete.cross_domain_links.is_empty());
    let primary_bytes = complete
        .items
        .iter()
        .map(serialized_len)
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .into_iter()
        .sum::<u64>();
    let mut constrained_budget = budget(&fixture.config);
    constrained_budget.max_bytes = std::num::NonZeroU64::new(primary_bytes).unwrap();

    let truncated = service.context(request(constrained_budget, None)).unwrap();
    assert!(!truncated.items.is_empty());
    assert_eq!(
        truncated.page.truncation.as_ref().map(|value| value.reason),
        Some(MemoryTruncationReason::Bytes)
    );
    let encoded_bytes = serde_json::to_vec(&truncated).unwrap().len() as u64;
    assert!(encoded_bytes <= primary_bytes);
    assert_eq!(
        truncated.page.truncation.as_ref().unwrap().returned_bytes,
        encoded_bytes
    );
    assert_eq!(
        truncated.page.next_cursor.is_some(),
        truncated.items.len() < complete.items.len()
    );

    assert!(complete.items.len() > 2);
    let page_size = complete.items.len() - 1;
    let page_primary_bytes = complete
        .items
        .iter()
        .take(page_size)
        .map(serialized_len)
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .into_iter()
        .sum::<u64>();
    let mut page_budget = budget(&fixture.config);
    page_budget.max_results = NonZeroU32::new(u32::try_from(page_size).unwrap()).unwrap();
    page_budget.max_bytes = NonZeroU64::new(page_primary_bytes).unwrap();

    let first = service.context(request(page_budget, None)).unwrap();
    assert_eq!(
        first.page.truncation.as_ref().map(|value| value.reason),
        Some(MemoryTruncationReason::Bytes)
    );
    let cursor = first
        .page
        .next_cursor
        .clone()
        .expect("remaining primary items must stay pageable");
    let second = service.context(request(page_budget, Some(cursor))).unwrap();
    assert!(!second.items.is_empty());
    assert!(second.items.iter().all(|item| !first.items.contains(item)));
}

#[test]
fn combined_context_counts_the_complete_response_against_the_byte_budget() {
    let fixture = fixture();
    let graph = SqliteGraphQuery::new(
        &fixture.graph_sidecar,
        fixture.config.query_limits.clone(),
        Some(fixture.graph_freshness.clone()),
    );
    let memory =
        SqliteMemoryQuery::new(&fixture.memory_sidecar, fixture.config.query_limits.clone());
    let service = service(&fixture, &graph, &memory);
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
    let seed = memory_search.hits[0].entity.id.clone();
    let request = |query_budget| FederatedContextRequest {
        scope: FederatedScope::current(
            fixture.project.clone(),
            target(&fixture, ContextDomain::All),
            query_budget,
        ),
        seeds: vec![FederatedContextSeed::MemoryEntity(seed.clone())],
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
    };

    let complete = service.context(request(budget(&fixture.config))).unwrap();
    assert!(complete.items.len() > 1);
    let mut one_item_budget = budget(&fixture.config);
    one_item_budget.max_results = NonZeroU32::new(1).unwrap();
    let one_item = service.context(request(one_item_budget)).unwrap();
    let one_response_bytes = one_item
        .page
        .truncation
        .as_ref()
        .expect("one-item context must be truncated")
        .returned_bytes;
    assert_eq!(
        one_response_bytes,
        serde_json::to_vec(&one_item).unwrap().len() as u64
    );

    let mut constrained_budget = budget(&fixture.config);
    constrained_budget.max_bytes = NonZeroU64::new(one_response_bytes).unwrap();
    let constrained = service.context(request(constrained_budget)).unwrap();
    assert_eq!(constrained.items.len(), 1);
    let encoded_bytes = serde_json::to_vec(&constrained).unwrap().len() as u64;
    assert!(encoded_bytes <= one_response_bytes);
    assert_eq!(
        constrained
            .page
            .truncation
            .as_ref()
            .expect("context must report byte truncation")
            .returned_bytes,
        encoded_bytes
    );

    let bare_item_bytes = serialized_len(&complete.items[0]).unwrap();
    let mut insufficient_budget = budget(&fixture.config);
    insufficient_budget.max_bytes = NonZeroU64::new(bare_item_bytes + 1).unwrap();
    assert_eq!(
        service.context(request(insufficient_budget)).unwrap_err(),
        MemoryQueryError::BudgetExceeded(MemoryTruncationReason::Bytes)
    );
}

#[test]
fn combined_search_cursor_is_deterministic_and_does_not_repeat_results() {
    let fixture = fixture();
    let graph = SqliteGraphQuery::new(
        &fixture.graph_sidecar,
        fixture.config.query_limits.clone(),
        Some(fixture.graph_freshness.clone()),
    );
    let memory =
        SqliteMemoryQuery::new(&fixture.memory_sidecar, fixture.config.query_limits.clone());
    let service = service(&fixture, &graph, &memory);
    let mut page_budget = budget(&fixture.config);
    page_budget.max_results = NonZeroU32::new(1).unwrap();
    let mut request = FederatedSearchRequest {
        scope: super::super::super::federation::FederatedScope::current(
            fixture.project.clone(),
            target(&fixture, ContextDomain::All),
            page_budget,
        ),
        text: MemoryQueryText::new("federat").unwrap(),
        repository_kinds: vec![],
        repository_paths: vec![],
        memory_kinds: vec![],
        memory_sources: vec![],
        cursor: None,
    };
    let first = service.search(request.clone()).unwrap();
    assert_eq!(first.results.len(), 1);
    request.cursor = first.page.next_cursor.clone();
    assert!(request.cursor.is_some());
    let second = service.search(request).unwrap();
    assert_eq!(second.results.len(), 1);
    assert_ne!(
        search_result_key(&first.results[0]),
        search_result_key(&second.results[0])
    );
}

#[test]
fn combined_search_counts_the_complete_response_against_the_byte_budget() {
    let fixture = fixture();
    let graph = SqliteGraphQuery::new(
        &fixture.graph_sidecar,
        fixture.config.query_limits.clone(),
        Some(fixture.graph_freshness.clone()),
    );
    let memory =
        SqliteMemoryQuery::new(&fixture.memory_sidecar, fixture.config.query_limits.clone());
    let service = service(&fixture, &graph, &memory);
    let request = |query_budget| FederatedSearchRequest {
        scope: FederatedScope::current(
            fixture.project.clone(),
            target(&fixture, ContextDomain::All),
            query_budget,
        ),
        text: MemoryQueryText::new("federat").unwrap(),
        repository_kinds: vec![],
        repository_paths: vec![],
        memory_kinds: vec![],
        memory_sources: vec![],
        cursor: None,
    };

    let complete = service.search(request(budget(&fixture.config))).unwrap();
    assert!(complete.results.len() > 1);

    let mut one_result_budget = budget(&fixture.config);
    one_result_budget.max_results = NonZeroU32::new(1).unwrap();
    let one_result = service.search(request(one_result_budget)).unwrap();
    let one_response_bytes = one_result
        .page
        .truncation
        .as_ref()
        .expect("one-result response must be truncated")
        .returned_bytes;
    assert_eq!(
        one_response_bytes,
        serde_json::to_vec(&one_result).unwrap().len() as u64
    );

    let mut constrained_budget = budget(&fixture.config);
    constrained_budget.max_bytes = NonZeroU64::new(one_response_bytes).unwrap();
    let constrained = service.search(request(constrained_budget)).unwrap();
    assert_eq!(constrained.results.len(), 1);
    let encoded_bytes = serde_json::to_vec(&constrained).unwrap().len() as u64;
    assert!(encoded_bytes <= one_response_bytes);
    assert_eq!(
        constrained
            .page
            .truncation
            .as_ref()
            .expect("response must report byte truncation")
            .returned_bytes,
        encoded_bytes
    );

    let bare_result_bytes = serialized_len(&complete.results[0]).unwrap();
    let mut insufficient_budget = budget(&fixture.config);
    insufficient_budget.max_bytes = NonZeroU64::new(bare_result_bytes + 1).unwrap();
    assert_eq!(
        service.search(request(insufficient_budget)).unwrap_err(),
        MemoryQueryError::BudgetExceeded(MemoryTruncationReason::Bytes)
    );
}
