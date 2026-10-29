//! Graph, memory, and federated context assembly.

use super::*;

#[allow(clippy::too_many_arguments)]
pub(in super::super) fn context_units(
    loaded: &LoadedTarget,
    seeds: &[RemoteContextSeed],
    direction: EdgeDirection,
    graph_edge_kinds: &[String],
    memory_relationship_kinds: &[crate::project_memory::domain::MemoryRelationshipKind],
    include_unresolved: bool,
    include_external: bool,
    max_depth: u32,
    started: Instant,
    duration: Duration,
) -> (Vec<ContextUnit>, u32, bool) {
    if let (Some(graph), Some(memory)) = (&loaded.graph, &loaded.memory) {
        return federated_context_units(
            graph,
            memory,
            seeds,
            direction,
            graph_edge_kinds,
            memory_relationship_kinds,
            include_unresolved,
            include_external,
            max_depth,
            started,
            duration,
        );
    }

    let mut units = Vec::new();
    let mut explored = 0;
    let mut depth_limited = false;
    if let Some(graph) = &loaded.graph {
        let roots = graph_seed_ids(graph, seeds, started, duration);
        if started.elapsed() >= duration {
            return (Vec::new(), 0, false);
        }

        let mut nodes = Vec::with_capacity(graph.nodes.len());
        for node in &graph.nodes {
            if started.elapsed() >= duration {
                return (Vec::new(), 0, false);
            }

            nodes.push(node.clone());
        }

        let mut edges = Vec::new();
        for edge in &graph.edges {
            if started.elapsed() >= duration {
                return (Vec::new(), 0, false);
            }

            let included = match &edge.target {
                EdgeTarget::Node(_) => true,
                EdgeTarget::External(_) => include_external,
                EdgeTarget::Unresolved(_) => include_unresolved,
            };
            if included {
                edges.push(edge.clone());
            }
        }

        let filtered = StoredRemoteGraphSnapshot {
            record: graph.record.clone(),
            nodes,
            edges,
            diagnostics: Vec::new(),
        };
        let (graph_units, depth, graph_depth_limited) = graph_neighborhood(
            &filtered,
            &roots,
            direction,
            graph_edge_kinds,
            max_depth,
            started,
            duration,
        );
        explored = explored.max(depth);
        depth_limited |= graph_depth_limited;
        units.extend(graph_units.into_iter().map(|unit| match unit {
            GraphUnit::Node(node) => ContextUnit::GraphNode(node),
            GraphUnit::Edge(edge) => ContextUnit::GraphEdge(edge),
        }));
    }

    if let Some(memory) = &loaded.memory {
        let (memory_units, depth, memory_depth_limited) = memory_context_units(
            memory,
            seeds,
            direction,
            memory_relationship_kinds,
            include_unresolved,
            max_depth,
            started,
            duration,
        );
        explored = explored.max(depth);
        depth_limited |= memory_depth_limited;
        units.extend(memory_units);
    }

    (units, explored, depth_limited)
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum FederatedNode {
    Graph(NodeId),
    Memory(crate::project_memory::domain::MemoryEntityId),
}

#[allow(clippy::too_many_arguments)]
pub(super) fn federated_context_units(
    graph: &StoredRemoteGraphSnapshot,
    memory: &StoredRemoteMemoryRevision,
    seeds: &[RemoteContextSeed],
    direction: EdgeDirection,
    graph_edge_kinds: &[String],
    memory_relationship_kinds: &[crate::project_memory::domain::MemoryRelationshipKind],
    include_unresolved: bool,
    include_external: bool,
    max_depth: u32,
    started: Instant,
    duration: Duration,
) -> (Vec<ContextUnit>, u32, bool) {
    let graph_roots = graph_seed_ids(graph, seeds, started, duration);
    if started.elapsed() >= duration {
        return (Vec::new(), 0, false);
    }

    let mut graph_nodes = BTreeMap::new();
    for node in &graph.nodes {
        if started.elapsed() >= duration {
            return (Vec::new(), 0, false);
        }

        graph_nodes.insert(node.id.clone(), node);
    }

    let mut memory_entities = BTreeMap::new();
    for entity in &memory.entities {
        if started.elapsed() >= duration {
            return (Vec::new(), 0, false);
        }

        memory_entities.insert(entity.id.clone(), entity);
    }

    let mut selected = graph_roots
        .into_iter()
        .filter(|id| graph_nodes.contains_key(id))
        .map(FederatedNode::Graph)
        .chain(seeds.iter().filter_map(|seed| match seed {
            RemoteContextSeed::MemoryEntity(id) if memory_entities.contains_key(id) => {
                Some(FederatedNode::Memory(id.clone()))
            }
            _ => None,
        }))
        .collect::<BTreeSet<_>>();
    let mut frontier = selected.clone();
    let mut selected_graph_edges = BTreeSet::new();
    let mut selected_memory_relationships = BTreeSet::new();
    let mut explored = 0;

    while !frontier.is_empty() && explored < max_depth && started.elapsed() < duration {
        let mut next = BTreeSet::new();
        for edge in &graph.edges {
            if started.elapsed() >= duration {
                return (Vec::new(), explored, false);
            }

            if !graph_edge_kinds.is_empty()
                && !graph_edge_kinds.iter().any(|kind| kind == &edge.kind)
            {
                continue;
            }

            let target = match &edge.target {
                EdgeTarget::Node(target) => Some(target),
                EdgeTarget::External(_) if include_external => None,
                EdgeTarget::Unresolved(_) if include_unresolved => None,
                EdgeTarget::External(_) | EdgeTarget::Unresolved(_) => continue,
            };
            let outgoing = frontier.contains(&FederatedNode::Graph(edge.source.clone()))
                && matches!(direction, EdgeDirection::Outgoing | EdgeDirection::Both);
            let incoming = target
                .is_some_and(|target| frontier.contains(&FederatedNode::Graph(target.clone())))
                && matches!(direction, EdgeDirection::Incoming | EdgeDirection::Both);
            if outgoing || incoming {
                selected_graph_edges.insert(edge.id.clone());
                if outgoing && let Some(target) = target {
                    next.insert(FederatedNode::Graph(target.clone()));
                }

                if incoming {
                    next.insert(FederatedNode::Graph(edge.source.clone()));
                }
            }
        }

        for relationship in &memory.relationships {
            if started.elapsed() >= duration {
                return (Vec::new(), explored, false);
            }

            if relationship.provenance.resolution != MemoryResolutionState::Resolved
                && !include_unresolved
            {
                continue;
            }

            if !memory_relationship_kinds.is_empty()
                && !memory_relationship_kinds.contains(&relationship.kind)
            {
                continue;
            }

            let targets = federated_relationship_targets(graph, relationship, started, duration);
            if started.elapsed() >= duration {
                return (Vec::new(), explored, false);
            }

            let outgoing = frontier.contains(&FederatedNode::Memory(relationship.source.clone()))
                && matches!(direction, EdgeDirection::Outgoing | EdgeDirection::Both);
            let incoming = targets.iter().any(|target| frontier.contains(target))
                && matches!(direction, EdgeDirection::Incoming | EdgeDirection::Both);
            if outgoing || incoming {
                selected_memory_relationships.insert(relationship.id.clone());
                if outgoing {
                    next.extend(targets);
                }

                if incoming {
                    next.insert(FederatedNode::Memory(relationship.source.clone()));
                }
            }
        }

        let mut filtered = BTreeSet::new();
        for node in next {
            if started.elapsed() >= duration {
                return (Vec::new(), explored, false);
            }

            let exists = match &node {
                FederatedNode::Graph(id) => graph_nodes.contains_key(id),
                FederatedNode::Memory(id) => memory_entities.contains_key(id),
            };
            if !selected.contains(&node) && exists {
                selected.insert(node.clone());
                filtered.insert(node);
            }
        }

        frontier = filtered;
        explored += 1;
    }

    let depth_limited = !frontier.is_empty() && explored >= max_depth;

    let mut units = Vec::new();
    for node in &selected {
        if started.elapsed() >= duration {
            return (Vec::new(), explored, false);
        }

        if let FederatedNode::Graph(id) = node
            && let Some(node) = graph_nodes.get(id)
        {
            units.push(ContextUnit::GraphNode((*node).clone()));
        }
    }

    for edge in &graph.edges {
        if started.elapsed() >= duration {
            return (Vec::new(), explored, false);
        }

        if selected_graph_edges.contains(&edge.id) {
            units.push(ContextUnit::GraphEdge(edge.clone()));
        }
    }

    for node in &selected {
        if started.elapsed() >= duration {
            return (Vec::new(), explored, false);
        }

        if let FederatedNode::Memory(id) = node
            && let Some(entity) = memory_entities.get(id)
        {
            units.push(ContextUnit::MemoryEntity((*entity).clone()));
        }
    }

    for relationship in &memory.relationships {
        if started.elapsed() >= duration {
            return (Vec::new(), explored, false);
        }

        if selected_memory_relationships.contains(&relationship.id) {
            units.push(ContextUnit::MemoryRelationship(relationship.clone()));
        }
    }

    (units, explored, depth_limited)
}

pub(super) fn federated_relationship_targets(
    graph: &StoredRemoteGraphSnapshot,
    relationship: &MemoryRelationship,
    started: Instant,
    duration: Duration,
) -> BTreeSet<FederatedNode> {
    match &relationship.target {
        MemoryRelationshipTarget::MemoryEntity { entity_id } => {
            BTreeSet::from([FederatedNode::Memory(entity_id.clone())])
        }
        MemoryRelationshipTarget::RepositoryNode {
            snapshot_id,
            node_id,
            ..
        } if relationship.provenance.resolution
            == crate::project_memory::domain::MemoryResolutionState::Resolved
            && snapshot_id == &graph.record.snapshot.snapshot_id =>
        {
            BTreeSet::from([FederatedNode::Graph(node_id.clone())])
        }
        MemoryRelationshipTarget::RepositoryPath {
            path,
            snapshot_id: Some(snapshot_id),
            ..
        } if relationship.provenance.resolution
            == crate::project_memory::domain::MemoryResolutionState::Resolved
            && snapshot_id == &graph.record.snapshot.snapshot_id =>
        {
            let mut targets = BTreeSet::new();
            for node in &graph.nodes {
                if started.elapsed() >= duration {
                    break;
                }

                if node
                    .provenance
                    .evidence
                    .as_ref()
                    .is_some_and(|evidence| &evidence.path == path)
                {
                    targets.insert(FederatedNode::Graph(node.id.clone()));
                }
            }

            targets
        }
        MemoryRelationshipTarget::RepositorySymbol {
            semantic_key,
            snapshot_id: Some(snapshot_id),
            ..
        } if relationship.provenance.resolution
            == crate::project_memory::domain::MemoryResolutionState::Resolved
            && snapshot_id == &graph.record.snapshot.snapshot_id =>
        {
            let mut targets = BTreeSet::new();
            for node in &graph.nodes {
                if started.elapsed() >= duration {
                    break;
                }

                if node.semantic_key.as_ref() == Some(semantic_key) {
                    targets.insert(FederatedNode::Graph(node.id.clone()));
                }
            }

            targets
        }
        MemoryRelationshipTarget::RepositoryNode { .. }
        | MemoryRelationshipTarget::RepositoryPath { .. }
        | MemoryRelationshipTarget::RepositorySymbol { .. }
        | MemoryRelationshipTarget::Task { .. }
        | MemoryRelationshipTarget::Run { .. }
        | MemoryRelationshipTarget::Milestone { .. } => BTreeSet::new(),
    }
}

pub(in super::super) fn graph_seed_ids(
    graph: &StoredRemoteGraphSnapshot,
    seeds: &[RemoteContextSeed],
    started: Instant,
    duration: Duration,
) -> Vec<NodeId> {
    let mut roots = BTreeSet::new();
    for seed in seeds {
        if started.elapsed() >= duration {
            break;
        }

        match seed {
            RemoteContextSeed::GraphNode(id) => {
                roots.insert(id.clone());
            }
            RemoteContextSeed::GraphSymbol(key) => {
                for node in &graph.nodes {
                    if started.elapsed() >= duration {
                        break;
                    }

                    if node.semantic_key.as_ref() == Some(key) {
                        roots.insert(node.id.clone());
                    }
                }
            }
            RemoteContextSeed::GraphPath(path) => {
                for node in &graph.nodes {
                    if started.elapsed() >= duration {
                        break;
                    }

                    if node
                        .provenance
                        .evidence
                        .as_ref()
                        .is_some_and(|evidence| &evidence.path == path)
                    {
                        roots.insert(node.id.clone());
                    }
                }
            }
            RemoteContextSeed::MemoryEntity(_) => {}
        }
    }

    roots.into_iter().collect()
}

#[allow(clippy::too_many_arguments)]
pub(in super::super) fn memory_context_units(
    memory: &StoredRemoteMemoryRevision,
    seeds: &[RemoteContextSeed],
    direction: EdgeDirection,
    relationship_kinds: &[crate::project_memory::domain::MemoryRelationshipKind],
    include_unresolved: bool,
    max_depth: u32,
    started: Instant,
    duration: Duration,
) -> (Vec<ContextUnit>, u32, bool) {
    let mut entities = BTreeMap::new();
    for entity in &memory.entities {
        if started.elapsed() >= duration {
            return (Vec::new(), 0, false);
        }

        entities.insert(entity.id.clone(), entity);
    }

    let mut selected = BTreeSet::new();
    for seed in seeds {
        if started.elapsed() >= duration {
            return (Vec::new(), 0, false);
        }

        if let RemoteContextSeed::MemoryEntity(id) = seed
            && entities.contains_key(id)
        {
            selected.insert(id.clone());
        }
    }

    let mut frontier = selected.clone();
    let mut relationships = BTreeSet::new();
    let mut explored = 0;
    while !frontier.is_empty() && explored < max_depth && started.elapsed() < duration {
        let mut next = BTreeSet::new();
        for relationship in &memory.relationships {
            if started.elapsed() >= duration {
                return (Vec::new(), explored, false);
            }

            if relationship.provenance.resolution != MemoryResolutionState::Resolved
                && !include_unresolved
            {
                continue;
            }

            if !relationship_kinds.is_empty() && !relationship_kinds.contains(&relationship.kind) {
                continue;
            }

            let target = match &relationship.target {
                MemoryRelationshipTarget::MemoryEntity { entity_id } => Some(entity_id),
                _ => None,
            };
            let outgoing = frontier.contains(&relationship.source)
                && matches!(direction, EdgeDirection::Outgoing | EdgeDirection::Both);
            let incoming = target.is_some_and(|target| frontier.contains(target))
                && matches!(direction, EdgeDirection::Incoming | EdgeDirection::Both);
            if outgoing || incoming {
                relationships.insert(relationship.id.clone());
                if outgoing && let Some(target) = target {
                    next.insert(target.clone());
                }

                if incoming {
                    next.insert(relationship.source.clone());
                }
            }
        }

        let mut filtered = BTreeSet::new();
        for id in next {
            if started.elapsed() >= duration {
                return (Vec::new(), explored, false);
            }

            if !selected.contains(&id) && entities.contains_key(&id) {
                selected.insert(id.clone());
                filtered.insert(id);
            }
        }

        frontier = filtered;
        explored += 1;
    }

    let depth_limited = !frontier.is_empty() && explored >= max_depth;
    let mut units = Vec::with_capacity(selected.len() + relationships.len());
    for id in selected {
        if started.elapsed() >= duration {
            return (Vec::new(), explored, false);
        }

        if let Some(entity) = entities.get(&id) {
            units.push(ContextUnit::MemoryEntity((*entity).clone()));
        }
    }

    for relationship in &memory.relationships {
        if started.elapsed() >= duration {
            return (Vec::new(), explored, false);
        }

        if relationships.contains(&relationship.id) {
            units.push(ContextUnit::MemoryRelationship(relationship.clone()));
        }
    }

    (units, explored, depth_limited)
}
