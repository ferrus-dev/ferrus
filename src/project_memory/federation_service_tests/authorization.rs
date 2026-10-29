//! Tests for authorization.

use super::*;

#[test]
fn explicit_domain_seed_validation_never_broadens_scope() {
    let seed = FederatedContextSeed::MemoryEntity(
        super::super::super::domain::MemoryEntityId::new("memory-entity").unwrap(),
    );
    assert!(repository_only_seeds(&[seed]).is_err());

    let path = RepoPath::new("src/lib.rs").unwrap();
    assert_eq!(
        memory_only_seeds(&[FederatedContextSeed::Repository(ContextSeed::Path(
            path.clone(),
        ))])
        .unwrap(),
        vec![MemoryContextSeed::RepositoryPath(path)]
    );

    let symbol = crate::repository_graph::domain::SemanticKey::new("crate::api::Service").unwrap();
    assert_eq!(
        memory_only_seeds(&[FederatedContextSeed::Repository(ContextSeed::Symbol(
            symbol.clone(),
        ))])
        .unwrap(),
        vec![MemoryContextSeed::RepositorySymbol(symbol)]
    );

    let node = crate::repository_graph::domain::NodeId::new("node-id").unwrap();
    assert!(
        memory_only_seeds(&[FederatedContextSeed::Repository(ContextSeed::Node(node))]).is_err()
    );
}

#[test]
fn repository_and_memory_search_scopes_remain_explicit() {
    let fixture = fixture();
    let graph = SqliteGraphQuery::new(
        &fixture.graph_sidecar,
        fixture.config.query_limits.clone(),
        Some(fixture.graph_freshness.clone()),
    );
    let memory =
        SqliteMemoryQuery::new(&fixture.memory_sidecar, fixture.config.query_limits.clone());
    let service = service(&fixture, &graph, &memory);
    let request = |domain| FederatedSearchRequest {
        scope: super::super::super::federation::FederatedScope::current(
            fixture.project.clone(),
            target(&fixture, domain),
            budget(&fixture.config),
        ),
        text: MemoryQueryText::new("federat").unwrap(),
        repository_kinds: vec![],
        repository_paths: vec![],
        memory_kinds: vec![],
        memory_sources: vec![],
        cursor: None,
    };

    let repository = service.search(request(ContextDomain::Repository)).unwrap();
    assert!(repository.repository.is_some());
    assert!(repository.memory.is_none());
    assert!(
        repository
            .results
            .iter()
            .all(|result| matches!(result, FederatedSearchResult::Repository(_)))
    );

    let memory = service.search(request(ContextDomain::Memory)).unwrap();
    assert!(memory.repository.is_none());
    assert!(memory.memory.is_some());
    assert!(
        memory
            .results
            .iter()
            .all(|result| matches!(result, FederatedSearchResult::Memory(_)))
    );
}
