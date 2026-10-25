//! Tests for cancellation.

use super::*;

#[test]
fn project_operator_can_submit_inspect_and_cancel_a_build() {
    let fixture = fixture();
    let mut objects = object_store(&fixture.object_root);
    let manifest = repository_manifest(&mut objects, "pub struct LaterType;\n");
    let spec = IndexJobSpec::new(
        IndexJobKind::RepositoryGraph,
        IndexInputRef::Repository(manifest.reference),
        IndexSemantics {
            semantic_config_digest: digest("91"),
            model_version: NonZeroU32::new(1).unwrap(),
            extractor_set_digest: digest("44"),
        },
    )
    .unwrap();
    let submit = SubmitIndexJobRequest {
        protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
        request_id: RequestId::new("control-submit").unwrap(),
        project: fixture.project.clone(),
        job: spec,
    };
    let operator = auth(
        CredentialClass::ProjectOperator,
        AuthorizationScope::Project(fixture.project.clone()),
    );
    let query_agent = auth(
        CredentialClass::QueryAgent,
        AuthorizationScope::Project(fixture.project.clone()),
    );
    let mut control = SqliteRemoteControlApi::new(coordinator(&fixture.control_path));
    assert_eq!(
        control
            .submit_build(&query_agent, &submit, Utc::now())
            .unwrap_err()
            .code,
        RemoteErrorCode::Unauthorized
    );
    let submitted = control
        .submit_build(&operator, &submit, Utc::now())
        .unwrap()
        .body;
    assert_eq!(
        submitted.state,
        super::super::super::protocol::IndexJobState::Queued
    );
    let inspected = control
        .inspect_build(
            &operator,
            &InspectIndexJobRequest {
                protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                request_id: RequestId::new("control-inspect").unwrap(),
                job: submitted.job.clone(),
            },
        )
        .unwrap()
        .body;
    assert_eq!(inspected.job, submitted.job);
    let cancelled = control
        .cancel_build(
            &operator,
            &CancelIndexJobRequest {
                protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                request_id: RequestId::new("control-cancel").unwrap(),
                job: submitted.job,
                expected_state: Some(super::super::super::protocol::IndexJobState::Queued),
            },
            Utc::now(),
        )
        .unwrap()
        .body;
    assert_eq!(
        cancelled.state,
        super::super::super::protocol::IndexJobState::Cancelled
    );
}

#[test]
fn mutable_view_resolution_honors_the_query_deadline() {
    let fixture = fixture();
    let request_id = RequestId::new("expired-view-resolution").unwrap();
    let started = Instant::now()
        .checked_sub(Duration::from_millis(2))
        .unwrap();
    let duration = Duration::from_millis(1);
    let targets = [
        RemoteQueryTarget::RepositoryView {
            repository: fixture.repository.clone(),
            view_name: PublishedViewName::new("canonical").unwrap(),
        },
        RemoteQueryTarget::MemoryView {
            project: fixture.project.clone(),
            view_name: crate::project_memory::domain::MemoryViewName::new("project").unwrap(),
        },
        RemoteQueryTarget::FederatedView {
            repository: fixture.repository.clone(),
            graph_view: PublishedViewName::new("canonical").unwrap(),
            memory_view: crate::project_memory::domain::MemoryViewName::new("project").unwrap(),
        },
    ];

    for target in targets {
        let error = fixture
            .query
            .resolve_target(&request_id, &target, started, duration)
            .unwrap_err();
        assert_eq!(error.code, RemoteErrorCode::BudgetExceeded);
    }
}

#[test]
fn snippet_manifest_loading_observes_an_expired_deadline_for_each_domain() {
    let fixture = fixture();
    let store =
        SqliteRemotePublicationStore::open(&fixture.control_path, KEY, publication_limits(), true)
            .unwrap();
    let context = RemoteContextData {
        graph_nodes: Vec::new(),
        graph_edges: Vec::new(),
        memory_entities: Vec::new(),
        memory_relationships: Vec::new(),
        snippets: Vec::new(),
    };
    let targets = [
        LoadedTarget {
            graph: Some(store.graph_snapshot(&fixture.graph).unwrap().unwrap()),
            memory: None,
        },
        LoadedTarget {
            graph: None,
            memory: Some(store.memory_revision(&fixture.memory).unwrap().unwrap()),
        },
    ];

    for loaded in targets {
        let error = fixture
            .query
            .snippets(
                &RequestId::new("expired-snippet").unwrap(),
                &loaded,
                &context,
                1024,
                Instant::now(),
                Duration::ZERO,
            )
            .unwrap_err();
        assert_eq!(error.code, RemoteErrorCode::BudgetExceeded);
    }
}

#[test]
fn snippet_manifest_job_lookup_observes_the_query_deadline() {
    let fixture = fixture();
    let store =
        SqliteRemotePublicationStore::open(&fixture.control_path, KEY, publication_limits(), true)
            .unwrap();
    let graph = store.graph_snapshot(&fixture.graph).unwrap().unwrap();
    let memory = store.memory_revision(&fixture.memory).unwrap().unwrap();
    let deadline = Instant::now()
        .checked_sub(Duration::from_millis(1))
        .unwrap();

    for job in [&graph.record.job, &memory.record.job] {
        let error = fixture
            .query
            .job(
                &RequestId::new("expired-manifest-job").unwrap(),
                job,
                deadline,
            )
            .unwrap_err();
        assert_eq!(error.code, RemoteErrorCode::BudgetExceeded);
    }
}

#[test]
fn search_candidate_generation_observes_an_expired_deadline() {
    let fixture = fixture();
    let store =
        SqliteRemotePublicationStore::open(&fixture.control_path, KEY, publication_limits(), true)
            .unwrap();
    let loaded = LoadedTarget {
        graph: Some(store.graph_snapshot(&fixture.graph).unwrap().unwrap()),
        memory: Some(store.memory_revision(&fixture.memory).unwrap().unwrap()),
    };
    assert!(
        search_candidates(
            &loaded,
            "important",
            &[],
            &[],
            &[],
            Instant::now(),
            Duration::ZERO,
        )
        .is_err()
    );
}

#[test]
fn graph_context_seed_resolution_observes_an_expired_deadline() {
    let fixture = fixture();
    let store =
        SqliteRemotePublicationStore::open(&fixture.control_path, KEY, publication_limits(), true)
            .unwrap();
    let graph = store.graph_snapshot(&fixture.graph).unwrap().unwrap();

    let roots = graph_seed_ids(
        &graph,
        &[RemoteContextSeed::GraphSymbol(
            SemanticKey::new("rust:function:src/lib.rs:important").unwrap(),
        )],
        Instant::now(),
        Duration::ZERO,
    );

    assert!(roots.is_empty());
}

#[test]
fn adjacency_and_materialization_observe_an_expired_deadline() {
    let fixture = fixture();
    let store =
        SqliteRemotePublicationStore::open(&fixture.control_path, KEY, publication_limits(), true)
            .unwrap();
    let graph = store.graph_snapshot(&fixture.graph).unwrap().unwrap();
    let memory = store.memory_revision(&fixture.memory).unwrap().unwrap();

    let (graph_units, graph_depth, graph_depth_limited) = graph_neighborhood(
        &graph,
        &[fixture.graph_node],
        EdgeDirection::Both,
        &[],
        2,
        Instant::now(),
        Duration::ZERO,
    );
    assert!(graph_units.is_empty());
    assert_eq!(graph_depth, 0);
    assert!(!graph_depth_limited);

    let (memory_units, memory_depth, memory_depth_limited) = memory_context_units(
        &memory,
        &[RemoteContextSeed::MemoryEntity(
            memory.entities[0].id.clone(),
        )],
        EdgeDirection::Both,
        &[],
        false,
        2,
        Instant::now(),
        Duration::ZERO,
    );
    assert!(memory_units.is_empty());
    assert_eq!(memory_depth, 0);
    assert!(!memory_depth_limited);
}

#[test]
fn response_fitting_observes_an_expired_query_deadline() {
    let request_id = RequestId::new("expired-response-fitting").unwrap();
    let result = RemoteQueryResult {
        page: empty_page(),
        diagnostics: Vec::new(),
        data: RemoteQueryData::Search(RemoteSearchData { items: Vec::new() }),
    };

    let error = fit_result_to_budget(
        result,
        u64::MAX,
        None,
        "expired-response-fitting",
        Instant::now(),
        &request_id,
    )
    .unwrap_err();
    assert_eq!(error.code, RemoteErrorCode::BudgetExceeded);
}
