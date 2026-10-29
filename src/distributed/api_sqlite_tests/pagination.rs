//! Tests for pagination.

use super::*;

#[test]
fn latest_search_is_resolved_once_and_pagination_is_snapshot_bound() {
    let fixture = fixture();
    let authorization = auth(
        CredentialClass::QueryAgent,
        AuthorizationScope::Project(fixture.project.clone()),
    );
    let operation = RemoteQueryOperation::Search {
        text: "important".to_string(),
        graph_kinds: Vec::new(),
        graph_paths: Vec::new(),
        memory_kinds: Vec::new(),
    };
    let request = query_request(
        &fixture,
        RemoteQueryTarget::FederatedView {
            repository: fixture.repository.clone(),
            graph_view: PublishedViewName::new("canonical").unwrap(),
            memory_view: crate::project_memory::domain::MemoryViewName::new("project").unwrap(),
        },
        operation.clone(),
        1,
        None,
    );
    let first = fixture.query.query(&authorization, &request).unwrap();
    assert_eq!(
        first.resolved_target,
        RemoteQueryTarget::Federated(
            FederatedViewRef::new(fixture.graph.clone(), fixture.memory.clone()).unwrap()
        )
    );
    let RemoteQueryData::Search(data) = first.body.data else {
        panic!("search response expected");
    };
    assert_eq!(data.items.len(), 1);
    let cursor = first.body.page.next_cursor.unwrap();
    let changed_query = query_request(
        &fixture,
        first.resolved_target.clone(),
        RemoteQueryOperation::Search {
            text: "different".to_string(),
            graph_kinds: Vec::new(),
            graph_paths: Vec::new(),
            memory_kinds: Vec::new(),
        },
        1,
        Some(cursor.clone()),
    );
    assert_eq!(
        fixture
            .query
            .query(&authorization, &changed_query)
            .unwrap_err()
            .code,
        RemoteErrorCode::StaleCursor
    );
    let pinned = query_request(&fixture, first.resolved_target, operation, 1, Some(cursor));
    let second = fixture.query.query(&authorization, &pinned).unwrap();
    let RemoteQueryData::Search(data) = second.body.data else {
        panic!("search response expected");
    };
    assert!(!data.items.is_empty());
}

#[test]
fn exact_response_budget_returns_a_smaller_page_instead_of_a_terminal_error() {
    let fixture = fixture();
    let store =
        SqliteRemotePublicationStore::open(&fixture.control_path, KEY, publication_limits(), true)
            .unwrap();
    let loaded = LoadedTarget {
        graph: Some(store.graph_snapshot(&fixture.graph).unwrap().unwrap()),
        memory: Some(store.memory_revision(&fixture.memory).unwrap().unwrap()),
    };
    let fingerprint = "response-budget";
    let candidates = search_candidates(
        &loaded,
        "important",
        &[],
        &[],
        &[],
        Instant::now(),
        Duration::from_secs(1),
    )
    .unwrap();
    assert!(candidates.len() > 1);
    let (items, mut page) = paginate(
        candidates,
        None,
        fingerprint,
        query_budget(10),
        Instant::now(),
        0,
        &RequestId::new("response-budget").unwrap(),
    )
    .unwrap();
    let data = RemoteQueryData::Search(RemoteSearchData { items });
    page.returned_bytes = encoded_len(&(&Vec::<RemoteQueryDiagnostic>::new(), &data)).unwrap();
    let result = RemoteQueryResult {
        page,
        diagnostics: Vec::new(),
        data,
    };
    let mut one = result.clone();
    let RemoteQueryData::Search(data) = &mut one.data else {
        unreachable!();
    };
    data.items.truncate(1);
    one.page.returned_results = 1;
    one.page.next_cursor = Some(RemotePageCursor::new(format!("{fingerprint}.1")).unwrap());
    one.page.truncation = Some(RemoteTruncationReason::Bytes);
    one.page.returned_bytes =
        encoded_len(&(&one.diagnostics, &one.data)).expect("one result serializes");
    let one_result_budget = encoded_len(&one).unwrap();

    let fitted = fit_result_to_budget(
        result,
        one_result_budget,
        None,
        fingerprint,
        Instant::now() + Duration::from_secs(1),
        &RequestId::new("response-budget").unwrap(),
    )
    .unwrap();
    assert_eq!(fitted, one);
}

#[test]
fn stale_cursor_and_tiny_response_budget_fail_without_reissuing_pages() {
    let fixture = fixture();
    let authorization = auth(
        CredentialClass::QueryAgent,
        AuthorizationScope::Project(fixture.project.clone()),
    );
    let mut request = query_request(
        &fixture,
        RemoteQueryTarget::Repository(fixture.graph.clone()),
        RemoteQueryOperation::Search {
            text: "important".to_string(),
            graph_kinds: Vec::new(),
            graph_paths: Vec::new(),
            memory_kinds: Vec::new(),
        },
        1,
        Some(RemotePageCursor::new("deadbeef.0").unwrap()),
    );
    let error = fixture.query.query(&authorization, &request).unwrap_err();
    assert_eq!(error.code, RemoteErrorCode::StaleCursor);

    request.body.page.cursor = None;
    request.body.budget.max_bytes = NonZeroU64::new(1).unwrap();
    let error = fixture.query.query(&authorization, &request).unwrap_err();
    assert_eq!(error.code, RemoteErrorCode::BudgetExceeded);
}
