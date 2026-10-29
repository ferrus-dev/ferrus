//! Tests for authorization.

use super::*;

#[test]
fn context_snippets_are_manifest_scoped_hash_verified_and_bounded() {
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
        10,
        None,
    );
    let response = fixture.query.query(&authorization, &request).unwrap();
    let RemoteQueryData::Context(data) = response.body.data else {
        panic!("context response expected");
    };
    assert_eq!(data.graph_nodes.len(), 2);
    assert_eq!(data.graph_edges.len(), 1);
    assert_eq!(data.snippets.len(), 1);
    let RemoteVerifiedSnippet::Repository {
        text,
        verified_content_identity,
        ..
    } = &data.snippets[0]
    else {
        panic!("repository snippet expected");
    };
    assert_eq!(text, &fixture.source_text);
    assert_eq!(
        verified_content_identity,
        &sha256(fixture.source_text.as_bytes())
    );
}

#[test]
fn memory_context_returns_only_authorized_sanitized_manifest_content() {
    let fixture = fixture();
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
    assert_eq!(data.snippets.len(), 1);
    let RemoteVerifiedSnippet::Memory {
        text,
        verified_fingerprint,
        ..
    } = &data.snippets[0]
    else {
        panic!("memory snippet expected");
    };
    assert_eq!(text, MEMORY_SNIPPET_TEXT);
    assert_eq!(verified_fingerprint, &digest("23"));
}

#[test]
fn authorization_denies_foreign_queries_without_disclosing_target_existence() {
    let fixture = fixture();
    let authorization = auth(
        CredentialClass::QueryAgent,
        AuthorizationScope::Project(RemoteProjectRef {
            tenant_id: TenantId::new("tenant-b").unwrap(),
            project_id: RemoteProjectId::new("project").unwrap(),
        }),
    );
    let request = query_request(
        &fixture,
        RemoteQueryTarget::Repository(fixture.graph.clone()),
        RemoteQueryOperation::Status,
        10,
        None,
    );
    let error = fixture.query.query(&authorization, &request).unwrap_err();
    assert_eq!(error.code, RemoteErrorCode::Unauthorized);
    assert!(!error.retryable);
}
