//! Tests for memory.

use super::*;

#[test]
fn repository_symbol_seed_matches_resolved_and_stale_node_links() {
    use crate::{
        project_memory::domain::{MemoryRelationshipId, MemoryRelationshipKind},
        repository_graph::domain::{
            NodeId, RepositoryId, RepositoryNamespace, RepositoryRef, SemanticKey, SnapshotId,
        },
    };

    let (_root, data, _project, revision_id) = indexed_fixture();
    let OpenMemoryQuerySidecarResult::Ready(sidecar) =
        open_for_query_at(&data.path().join(MEMORY_SIDECAR_FILE_NAME)).unwrap()
    else {
        panic!("memory query sidecar should be ready");
    };
    let query = SqliteMemoryQuery::new(&sidecar, QueryLimitsConfig::default());
    let entity = query.entities(&revision_id).unwrap().remove(0);
    let symbol = SemanticKey::new("rust:function:src/lib.rs:run").unwrap();
    let mut provenance = entity.provenance.clone();
    provenance.resolution = MemoryResolutionState::Resolved;
    let relationship = MemoryRelationship {
        project: entity.project.clone(),
        memory_revision_id: revision_id,
        id: MemoryRelationshipId::new("memory-relationship:resolved-symbol").unwrap(),
        kind: MemoryRelationshipKind::Touches,
        source: entity.id.clone(),
        target: MemoryRelationshipTarget::RepositoryNode {
            repository: RepositoryRef {
                namespace: RepositoryNamespace::new("local:test").unwrap(),
                repository_id: RepositoryId::new("repository").unwrap(),
            },
            snapshot_id: SnapshotId::new("snapshot-current").unwrap(),
            node_id: NodeId::new("node-run").unwrap(),
            semantic_key: Some(symbol.clone()),
        },
        provenance,
    };
    let entities = BTreeMap::from([(entity.id.clone(), entity.clone())]);
    let mut policy = super::super::super::query::MemoryContextPolicy {
        direction: EdgeDirection::Incoming,
        relationship_kinds: vec![],
        include_unresolved: false,
        include_stale: false,
        include_snippets: false,
    };

    assert_eq!(
        matching_seed_entities(
            &MemoryContextSeed::RepositorySymbol(symbol.clone()),
            &entities,
            std::slice::from_ref(&relationship),
            &policy,
        ),
        vec![entity.id.clone()]
    );

    let mut stale = relationship;
    stale.provenance.resolution = MemoryResolutionState::Stale;
    assert!(
        matching_seed_entities(
            &MemoryContextSeed::RepositorySymbol(symbol.clone()),
            &entities,
            std::slice::from_ref(&stale),
            &policy,
        )
        .is_empty()
    );
    policy.include_stale = true;
    assert_eq!(
        matching_seed_entities(
            &MemoryContextSeed::RepositorySymbol(symbol),
            &entities,
            std::slice::from_ref(&stale),
            &policy,
        ),
        vec![entity.id]
    );
}
