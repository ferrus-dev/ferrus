//! Tests for review.

use super::*;

#[test]
fn memory_context_rejects_non_span_evidence_for_curated_content() {
    let fixture = fixture_with_memory_evidence(MemoryEvidenceLocator::Record(
        MemoryRecordId::new("outcome").unwrap(),
    ));
    let authorization = auth(
        CredentialClass::QueryAgent,
        AuthorizationScope::Project(fixture.project.clone()),
    );
    let request = query_request(
        &fixture,
        RemoteQueryTarget::Memory(fixture.memory.clone()),
        RemoteQueryOperation::Context {
            seeds: vec![RemoteContextSeed::MemoryEntity(
                MemoryEntityId::new("memory-important").unwrap(),
            )],
            direction: EdgeDirection::Both,
            graph_edge_kinds: Vec::new(),
            memory_relationship_kinds: Vec::new(),
            include_unresolved: false,
            include_external: false,
            include_snippets: true,
        },
        10,
        None,
    );
    let response = fixture.query.query(&authorization, &request).unwrap();
    let RemoteQueryData::Context(data) = response.body.data else {
        panic!("context response expected");
    };
    assert_eq!(data.memory_entities.len(), 1);
    assert!(data.snippets.is_empty());
}

#[test]
fn federated_context_rejects_repository_links_for_another_snapshot() {
    let fixture = fixture();
    let store =
        SqliteRemotePublicationStore::open(&fixture.control_path, KEY, publication_limits(), true)
            .unwrap();
    let graph = store.graph_snapshot(&fixture.graph).unwrap().unwrap();
    let mut memory = store.memory_revision(&fixture.memory).unwrap().unwrap();
    let source = memory.entities[0].clone();
    memory.relationships = vec![MemoryRelationship {
        project: source.project.clone(),
        memory_revision_id: source.memory_revision_id.clone(),
        id: MemoryRelationshipId::new("stale-repository-link").unwrap(),
        kind: MemoryRelationshipKind::Touches,
        source: source.id.clone(),
        target: MemoryRelationshipTarget::RepositoryNode {
            repository: local_repository(),
            snapshot_id: SnapshotId::new("another-snapshot").unwrap(),
            node_id: fixture.graph_node.clone(),
            semantic_key: Some(SemanticKey::new("important-type").unwrap()),
        },
        provenance: source.provenance,
    }];
    let loaded = LoadedTarget {
        graph: Some(graph),
        memory: Some(memory),
    };
    let (units, _, _) = context_units(
        &loaded,
        &[RemoteContextSeed::MemoryEntity(source.id)],
        EdgeDirection::Outgoing,
        &[],
        &[],
        false,
        false,
        1,
        Instant::now(),
        Duration::from_secs(1),
    );
    assert!(
        !units
            .iter()
            .any(|unit| matches!(unit, ContextUnit::GraphNode(_)))
    );
}

#[test]
fn exact_response_budget_trims_an_optional_snippet_before_rejecting_one_result() {
    let fixture = fixture();
    let authorization = auth(
        CredentialClass::QueryAgent,
        AuthorizationScope::Repository(fixture.repository.clone()),
    );
    let request = query_request(
        &fixture,
        RemoteQueryTarget::Repository(fixture.graph.clone()),
        RemoteQueryOperation::Context {
            seeds: vec![RemoteContextSeed::GraphNode(fixture.graph_node.clone())],
            direction: EdgeDirection::Both,
            graph_edge_kinds: Vec::new(),
            memory_relationship_kinds: Vec::new(),
            include_unresolved: false,
            include_external: false,
            include_snippets: true,
        },
        1,
        None,
    );
    let result = fixture.query.query(&authorization, &request).unwrap().body;
    let RemoteQueryData::Context(context) = &result.data else {
        panic!("context response expected");
    };
    assert_eq!(
        context
            .graph_nodes
            .len()
            .saturating_add(context.graph_edges.len())
            .saturating_add(context.memory_entities.len())
            .saturating_add(context.memory_relationships.len()),
        1
    );
    assert_eq!(context.snippets.len(), 1);
    let original_text = match &context.snippets[0] {
        RemoteVerifiedSnippet::Repository { text, .. }
        | RemoteVerifiedSnippet::Memory { text, .. } => text.clone(),
    };

    let mut one_character = result.clone();
    let RemoteQueryData::Context(context) = &mut one_character.data else {
        unreachable!();
    };
    match &mut context.snippets[0] {
        RemoteVerifiedSnippet::Repository {
            text, truncated, ..
        }
        | RemoteVerifiedSnippet::Memory {
            text, truncated, ..
        } => {
            *text = original_text[..1].to_string();
            *truncated = true;
        }
    }

    one_character.page.truncation = Some(RemoteTruncationReason::Bytes);
    one_character.page.returned_bytes =
        encoded_len(&(&one_character.diagnostics, &one_character.data)).unwrap();
    let budget = encoded_len(&one_character).unwrap();

    let fitted = fit_result_to_budget(
        result,
        budget,
        None,
        "snippet-response-budget",
        Instant::now() + Duration::from_secs(1),
        &RequestId::new("snippet-response-budget").unwrap(),
    )
    .unwrap();
    assert!(encoded_len(&fitted).unwrap() <= budget);
    assert_eq!(fitted.page.returned_results, 1);
    assert_eq!(fitted.page.truncation, Some(RemoteTruncationReason::Bytes));
    let RemoteQueryData::Context(context) = fitted.data else {
        panic!("context response expected");
    };
    assert_eq!(context.snippets.len(), 1);
    let (text, truncated) = match &context.snippets[0] {
        RemoteVerifiedSnippet::Repository {
            text, truncated, ..
        }
        | RemoteVerifiedSnippet::Memory {
            text, truncated, ..
        } => (text, truncated),
    };
    assert!(!text.is_empty());
    assert!(text.len() < original_text.len());
    assert!(*truncated);
}

#[test]
fn explicit_queries_reject_immutable_targets_without_publication_visibility() {
    let fixture = fixture();
    rusqlite::Connection::open(&fixture.control_path)
        .unwrap()
        .execute(
            "DELETE FROM remote_published_targets
             WHERE tenant_id = ?1 AND project_id = ?2",
            rusqlite::params![
                fixture.project.tenant_id.as_str(),
                fixture.project.project_id.as_str()
            ],
        )
        .unwrap();
    let authorization = auth(
        CredentialClass::QueryAgent,
        AuthorizationScope::Project(fixture.project.clone()),
    );

    for target in [
        RemoteQueryTarget::Repository(fixture.graph.clone()),
        RemoteQueryTarget::Memory(fixture.memory.clone()),
    ] {
        let request = query_request(&fixture, target, RemoteQueryOperation::Status, 10, None);
        assert_eq!(
            fixture
                .query
                .query(&authorization, &request)
                .unwrap_err()
                .code,
            RemoteErrorCode::NotFound
        );
    }
}
