//! Tests for memory.

use super::*;

#[test]
fn memory_context_traversal_honors_relationship_direction() {
    let fixture = fixture();
    let store =
        SqliteRemotePublicationStore::open(&fixture.control_path, KEY, publication_limits(), true)
            .unwrap();
    let mut memory = store.memory_revision(&fixture.memory).unwrap().unwrap();
    let mut source = memory.entities[0].clone();
    source.id = MemoryEntityId::new("memory-source").unwrap();
    let mut target = memory.entities[0].clone();
    target.id = MemoryEntityId::new("memory-target").unwrap();
    memory.entities = vec![source.clone(), target.clone()];
    memory.relationships = vec![MemoryRelationship {
        project: source.project.clone(),
        memory_revision_id: source.memory_revision_id.clone(),
        id: MemoryRelationshipId::new("memory-direction-edge").unwrap(),
        kind: MemoryRelationshipKind::Concerns,
        source: source.id.clone(),
        target: MemoryRelationshipTarget::MemoryEntity {
            entity_id: target.id.clone(),
        },
        provenance: source.provenance,
    }];
    let loaded = LoadedTarget {
        graph: None,
        memory: Some(memory),
    };
    let traverse = |seed: &MemoryEntityId, direction| {
        let (units, _, _) = context_units(
            &loaded,
            &[RemoteContextSeed::MemoryEntity(seed.clone())],
            direction,
            &[],
            &[],
            false,
            false,
            1,
            Instant::now(),
            Duration::from_secs(1),
        );
        let entities = units
            .iter()
            .filter_map(|unit| match unit {
                ContextUnit::MemoryEntity(entity) => Some(entity.id.clone()),
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        let relationships = units
            .iter()
            .filter(|unit| matches!(unit, ContextUnit::MemoryRelationship(_)))
            .count();
        (entities, relationships)
    };

    assert_eq!(
        traverse(&source.id, EdgeDirection::Outgoing),
        (BTreeSet::from([source.id.clone(), target.id.clone()]), 1)
    );
    assert_eq!(
        traverse(&source.id, EdgeDirection::Incoming),
        (BTreeSet::from([source.id.clone()]), 0)
    );
    assert_eq!(
        traverse(&target.id, EdgeDirection::Outgoing),
        (BTreeSet::from([target.id.clone()]), 0)
    );
    assert_eq!(
        traverse(&target.id, EdgeDirection::Incoming),
        (BTreeSet::from([source.id, target.id]), 1)
    );
}

#[test]
fn federated_context_traverses_resolved_repository_links_in_both_directions() {
    let fixture = fixture();
    let store =
        SqliteRemotePublicationStore::open(&fixture.control_path, KEY, publication_limits(), true)
            .unwrap();
    let graph = store.graph_snapshot(&fixture.graph).unwrap().unwrap();
    let memory = store.memory_revision(&fixture.memory).unwrap().unwrap();
    let source = memory.entities[0].clone();
    let targets = vec![
        MemoryRelationshipTarget::RepositoryNode {
            repository: local_repository(),
            snapshot_id: fixture.graph.snapshot_id.clone(),
            node_id: fixture.graph_node.clone(),
            semantic_key: Some(SemanticKey::new("important-type").unwrap()),
        },
        MemoryRelationshipTarget::RepositoryPath {
            repository: local_repository(),
            path: RepoPath::new("src/lib.rs").unwrap(),
            snapshot_id: Some(fixture.graph.snapshot_id.clone()),
        },
        MemoryRelationshipTarget::RepositorySymbol {
            repository: local_repository(),
            semantic_key: SemanticKey::new("important-type").unwrap(),
            snapshot_id: Some(fixture.graph.snapshot_id.clone()),
        },
    ];

    for (index, target) in targets.into_iter().enumerate() {
        let mut linked_memory = memory.clone();
        linked_memory.relationships = vec![MemoryRelationship {
            project: source.project.clone(),
            memory_revision_id: source.memory_revision_id.clone(),
            id: MemoryRelationshipId::new(format!("memory-repository-link-{index}")).unwrap(),
            kind: MemoryRelationshipKind::Touches,
            source: source.id.clone(),
            target,
            provenance: source.provenance.clone(),
        }];
        let loaded = LoadedTarget {
            graph: Some(graph.clone()),
            memory: Some(linked_memory),
        };
        let traverse = |seed, direction| {
            context_units(
                &loaded,
                &[seed],
                direction,
                &[],
                &[],
                false,
                false,
                1,
                Instant::now(),
                Duration::from_secs(1),
            )
            .0
        };
        let outgoing = traverse(
            RemoteContextSeed::MemoryEntity(source.id.clone()),
            EdgeDirection::Outgoing,
        );
        assert!(outgoing.iter().any(
            |unit| matches!(unit, ContextUnit::GraphNode(node) if node.id == fixture.graph_node)
        ));
        assert!(
            outgoing
                .iter()
                .any(|unit| matches!(unit, ContextUnit::MemoryRelationship(_)))
        );

        let incoming = traverse(
            RemoteContextSeed::GraphNode(fixture.graph_node.clone()),
            EdgeDirection::Incoming,
        );
        assert!(incoming.iter().any(
            |unit| matches!(unit, ContextUnit::MemoryEntity(entity) if entity.id == source.id)
        ));
        assert!(
            incoming
                .iter()
                .any(|unit| matches!(unit, ContextUnit::MemoryRelationship(_)))
        );
    }
}

#[test]
fn remote_context_honors_include_unresolved_for_memory_links() {
    let fixture = fixture();
    let store =
        SqliteRemotePublicationStore::open(&fixture.control_path, KEY, publication_limits(), true)
            .unwrap();
    let graph = store.graph_snapshot(&fixture.graph).unwrap().unwrap();
    let mut memory = store.memory_revision(&fixture.memory).unwrap().unwrap();
    let source = memory.entities[0].clone();
    let mut provenance = source.provenance.clone();
    provenance.resolution = MemoryResolutionState::Unresolved;
    memory.relationships = vec![MemoryRelationship {
        project: source.project.clone(),
        memory_revision_id: source.memory_revision_id.clone(),
        id: MemoryRelationshipId::new("unresolved-repository-link").unwrap(),
        kind: MemoryRelationshipKind::Touches,
        source: source.id.clone(),
        target: MemoryRelationshipTarget::RepositoryPath {
            repository: local_repository(),
            path: RepoPath::new("missing.rs").unwrap(),
            snapshot_id: None,
        },
        provenance,
    }];
    let loaded = LoadedTarget {
        graph: Some(graph),
        memory: Some(memory.clone()),
    };
    let traverse = |include_unresolved| {
        context_units(
            &loaded,
            &[RemoteContextSeed::MemoryEntity(source.id.clone())],
            EdgeDirection::Outgoing,
            &[],
            &[],
            include_unresolved,
            false,
            1,
            Instant::now(),
            Duration::from_secs(1),
        )
        .0
    };

    assert!(!traverse(false).iter().any(
        |unit| matches!(unit, ContextUnit::MemoryRelationship(relationship)
            if relationship.id.as_str() == "unresolved-repository-link")
    ));
    assert!(traverse(true).iter().any(
        |unit| matches!(unit, ContextUnit::MemoryRelationship(relationship)
            if relationship.id.as_str() == "unresolved-repository-link")
    ));

    let memory_only = LoadedTarget {
        graph: None,
        memory: Some(memory),
    };
    let traverse_memory_only = |include_unresolved| {
        context_units(
            &memory_only,
            &[RemoteContextSeed::MemoryEntity(source.id.clone())],
            EdgeDirection::Outgoing,
            &[],
            &[],
            include_unresolved,
            false,
            1,
            Instant::now(),
            Duration::from_secs(1),
        )
        .0
    };
    assert!(!traverse_memory_only(false).iter().any(
        |unit| matches!(unit, ContextUnit::MemoryRelationship(relationship)
            if relationship.id.as_str() == "unresolved-repository-link")
    ));
    assert!(traverse_memory_only(true).iter().any(
        |unit| matches!(unit, ContextUnit::MemoryRelationship(relationship)
            if relationship.id.as_str() == "unresolved-repository-link")
    ));
}
