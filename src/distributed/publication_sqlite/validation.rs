//! Immutable graph and memory link validation.

use super::*;

pub(super) fn decode_memory_link_target(
    encoded: &[u8],
    graph: &RemoteGraphSnapshotRef,
) -> Result<RemoteMemoryLinkSetTarget, RemoteStoreError> {
    if let Ok(target) = serde_json::from_slice::<RemoteMemoryLinkSetTarget>(encoded) {
        return (target.graph == *graph)
            .then_some(target)
            .ok_or(RemoteStoreError::IntegrityFailure);
    }

    let link_set = decode(encoded)?;
    Ok(RemoteMemoryLinkSetTarget {
        graph: graph.clone(),
        origin_graphs: Vec::new(),
        link_set,
    })
}

pub(super) fn load_graph_snapshot_from_connection(
    connection: &Connection,
    key: &LessSafeKey,
    snapshot: &RemoteGraphSnapshotRef,
    deadline: Option<(Instant, Duration)>,
    published_only: bool,
) -> Result<Option<StoredRemoteGraphSnapshot>, RemoteStoreError> {
    ensure_read_budget(deadline)?;
    if published_only
        && !target_was_published(
            connection,
            &snapshot.repository.project,
            "repository_graph",
            snapshot.repository.repository_id.as_str(),
            snapshot.snapshot_id.as_str(),
        )?
    {
        return Ok(None);
    }

    let Some(record) = load_graph_record(connection, snapshot)? else {
        return Ok(None);
    };
    let facts = load_facts(
        connection,
        key,
        &record.job,
        "repository_graph",
        record.snapshot.snapshot_id.as_str(),
        record.snapshot.repository.repository_id.as_str(),
        deadline,
    )?;
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut diagnostics = Vec::new();
    for (kind, encoded) in facts {
        ensure_read_budget(deadline)?;
        match kind.as_str() {
            "node" => nodes.push(decode(&encoded)?),
            "edge" => edges.push(decode(&encoded)?),
            "diagnostic" => diagnostics.push(decode(&encoded)?),
            _ => return Err(RemoteStoreError::IntegrityFailure),
        }
    }

    if u64::try_from(nodes.len()).ok() != Some(record.counts.primary)
        || u64::try_from(edges.len()).ok() != Some(record.counts.relationships)
        || u64::try_from(diagnostics.len()).ok() != Some(record.counts.diagnostics)
    {
        return Err(RemoteStoreError::IntegrityFailure);
    }

    Ok(Some(StoredRemoteGraphSnapshot {
        record,
        nodes,
        edges,
        diagnostics,
    }))
}

pub(super) fn validate_repository_links_against_graph(
    links: &PreparedMemoryRepositoryLinks,
    graph: &StoredRemoteGraphSnapshot,
    origins: &[StoredRemoteGraphSnapshot],
) -> Result<(), RemoteStoreError> {
    if links.target.graph != graph.record.snapshot
        || links.target.link_set.repository != graph.record.repository_identity
        || origins.len() != links.target.origin_graphs.len()
        || origins
            .iter()
            .zip(&links.target.origin_graphs)
            .any(|(stored, expected)| {
                stored.record.snapshot != *expected
                    || stored.record.repository_identity != graph.record.repository_identity
            })
    {
        return Err(RemoteStoreError::InvalidInput);
    }

    let valid = links.relationships.iter().all(|relationship| {
        match (&relationship.target, relationship.provenance.resolution) {
            (
                MemoryRelationshipTarget::RepositoryNode {
                    repository,
                    snapshot_id,
                    node_id,
                    semantic_key,
                },
                MemoryResolutionState::Resolved,
            ) => {
                repository == &graph.record.repository_identity
                    && snapshot_id == &graph.record.snapshot.snapshot_id
                    && repository_node_matches(graph, node_id, semantic_key.as_ref())
            }
            (
                MemoryRelationshipTarget::RepositoryNode {
                    repository,
                    snapshot_id,
                    node_id,
                    semantic_key,
                },
                MemoryResolutionState::Stale,
            ) => {
                repository == &graph.record.repository_identity
                    && origins
                        .iter()
                        .find(|origin| origin.record.snapshot.snapshot_id == *snapshot_id)
                        .is_some_and(|origin| {
                            repository_node_matches(origin, node_id, semantic_key.as_ref())
                        })
            }
            (
                MemoryRelationshipTarget::RepositoryPath {
                    repository,
                    path,
                    snapshot_id: Some(snapshot_id),
                },
                MemoryResolutionState::Resolved,
            ) => {
                repository == &graph.record.repository_identity
                    && snapshot_id == &graph.record.snapshot.snapshot_id
                    && repository_path_matches(graph, path)
            }
            (
                MemoryRelationshipTarget::RepositoryPath {
                    repository,
                    path,
                    snapshot_id: Some(snapshot_id),
                },
                MemoryResolutionState::Stale,
            ) => {
                repository == &graph.record.repository_identity
                    && origins
                        .iter()
                        .find(|origin| origin.record.snapshot.snapshot_id == *snapshot_id)
                        .is_some_and(|origin| repository_path_matches(origin, path))
            }
            (
                MemoryRelationshipTarget::RepositoryPath {
                    repository,
                    snapshot_id: None,
                    ..
                }
                | MemoryRelationshipTarget::RepositorySymbol {
                    repository,
                    snapshot_id: None,
                    ..
                },
                MemoryResolutionState::Unresolved,
            ) => repository == &graph.record.repository_identity,
            _ => false,
        }
    });
    valid.then_some(()).ok_or(RemoteStoreError::InvalidInput)
}

pub(super) fn repository_node_matches(
    graph: &StoredRemoteGraphSnapshot,
    node_id: &NodeId,
    semantic_key: Option<&SemanticKey>,
) -> bool {
    graph.nodes.iter().any(|node| {
        &node.id == node_id
            && semantic_key
                .is_none_or(|semantic_key| node.semantic_key.as_ref() == Some(semantic_key))
    })
}

pub(super) fn repository_path_matches(graph: &StoredRemoteGraphSnapshot, path: &RepoPath) -> bool {
    graph.nodes.iter().any(|node| {
        node.provenance
            .evidence
            .as_ref()
            .is_some_and(|evidence| &evidence.path == path)
    })
}
