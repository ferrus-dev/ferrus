//! Validated graph and memory fact preparation.

use super::*;

pub(in super::super) fn prepare_graph(
    request: &PublishGraphRequest,
    batches: &[FactBatch],
    now: DateTime<Utc>,
    max_facts: NonZeroU64,
) -> Result<PreparedGraph, RemoteStoreError> {
    validate_batch_stream(&request.job, batches)?;
    let mut nodes = BTreeMap::<NodeId, GraphNode>::new();
    let mut edges = BTreeMap::<EdgeId, GraphEdge>::new();
    let mut diagnostics = BTreeMap::<String, GraphDiagnostic>::new();
    let mut target = None;
    let mut extractor_set_digest = None;
    for batch in batches {
        let FactTarget::RepositoryGraph {
            snapshot,
            repository_identity,
            build_id,
        } = &batch.header.target
        else {
            return Err(RemoteStoreError::InvalidInput);
        };
        if snapshot.repository != request.repository || snapshot.snapshot_id != request.snapshot_id
        {
            return Err(RemoteStoreError::InvalidInput);
        }

        match &target {
            None => {
                target = Some((
                    snapshot.clone(),
                    repository_identity.clone(),
                    build_id.clone(),
                ))
            }
            Some((existing_snapshot, existing_repository, existing_build))
                if existing_snapshot == snapshot
                    && existing_repository == repository_identity
                    && existing_build == build_id => {}
            Some(_) => return Err(RemoteStoreError::InvalidInput),
        }

        match &extractor_set_digest {
            None => extractor_set_digest = Some(batch.header.extractor_set_digest.clone()),
            Some(existing) if existing == &batch.header.extractor_set_digest => {}
            Some(_) => return Err(RemoteStoreError::InvalidInput),
        }

        let FactBatchPayload::RepositoryGraph {
            nodes: batch_nodes,
            edges: batch_edges,
            diagnostics: batch_diagnostics,
        } = &batch.payload
        else {
            return Err(RemoteStoreError::InvalidInput);
        };
        merge_facts(&mut nodes, batch_nodes, |node| &node.id)?;
        merge_facts(&mut edges, batch_edges, |edge| &edge.id)?;
        for diagnostic in batch_diagnostics {
            let encoded =
                serde_json::to_vec(diagnostic).map_err(|_| RemoteStoreError::Serialization)?;
            let id = sha256_value(b"ferrus.remote.graph-diagnostic.v1\0", &encoded);
            if diagnostics
                .insert(id, diagnostic.clone())
                .is_some_and(|existing| existing != *diagnostic)
            {
                return Err(RemoteStoreError::FactConflict);
            }
        }
    }

    for edge in edges.values() {
        if !nodes.contains_key(&edge.source)
            || matches!(&edge.target, EdgeTarget::Node(target) if !nodes.contains_key(target))
        {
            return Err(RemoteStoreError::FactConflict);
        }
    }

    let count = checked_fact_count(nodes.len(), edges.len(), diagnostics.len())?;
    if count > max_facts.get() {
        return Err(RemoteStoreError::QuotaExceeded);
    }

    let (snapshot, repository_identity, build_id) = target.ok_or(RemoteStoreError::InvalidInput)?;
    let extractor_set_digest = extractor_set_digest.ok_or(RemoteStoreError::InvalidInput)?;
    let fact_set_digest = canonical_digest(&(
        nodes.values().collect::<Vec<_>>(),
        edges.values().collect::<Vec<_>>(),
        diagnostics.values().collect::<Vec<_>>(),
    ))?;
    let mut facts = Vec::with_capacity(count as usize);
    append_facts(
        &mut facts,
        "node",
        nodes.into_iter().map(|(id, value)| (id.to_string(), value)),
    )?;
    append_facts(
        &mut facts,
        "edge",
        edges.into_iter().map(|(id, value)| (id.to_string(), value)),
    )?;
    append_facts(&mut facts, "diagnostic", diagnostics)?;
    Ok(PreparedGraph {
        record: RemoteGraphSnapshotRecord {
            snapshot,
            repository_identity,
            job: request.job.clone(),
            build_id,
            extractor_set_digest,
            fact_set_digest,
            counts: RemoteFactCounts {
                primary: facts.iter().filter(|fact| fact.kind == "node").count() as u64,
                relationships: facts.iter().filter(|fact| fact.kind == "edge").count() as u64,
                diagnostics: facts
                    .iter()
                    .filter(|fact| fact.kind == "diagnostic")
                    .count() as u64,
            },
            completed_at: now,
        },
        facts,
    })
}

pub(in super::super) fn prepare_memory(
    request: &PublishMemoryRequest,
    batches: &[FactBatch],
    now: DateTime<Utc>,
    max_facts: NonZeroU64,
) -> Result<PreparedMemory, RemoteStoreError> {
    validate_batch_stream(&request.job, batches)?;
    let mut entities = BTreeMap::<MemoryEntityId, MemoryEntity>::new();
    let mut relationships = BTreeMap::<MemoryRelationshipId, MemoryRelationship>::new();
    let mut repository_relationships = BTreeMap::<MemoryRelationshipId, MemoryRelationship>::new();
    let mut diagnostics = BTreeMap::<String, MemoryDiagnostic>::new();
    let mut target = None;
    let mut extractor_set_digest = None;
    for batch in batches {
        let FactTarget::ProjectMemory {
            revision,
            project_identity,
            build_id,
            repository_links,
        } = &batch.header.target
        else {
            return Err(RemoteStoreError::InvalidInput);
        };
        if revision.project != request.project || revision.revision_id != request.revision_id {
            return Err(RemoteStoreError::InvalidInput);
        }

        match &target {
            None => {
                target = Some((
                    revision.clone(),
                    project_identity.clone(),
                    build_id.clone(),
                    repository_links.clone(),
                ))
            }
            Some((existing_revision, existing_project, existing_build, existing_links))
                if existing_revision == revision
                    && existing_project == project_identity
                    && existing_build == build_id
                    && existing_links == repository_links => {}
            Some(_) => return Err(RemoteStoreError::InvalidInput),
        }

        match &extractor_set_digest {
            None => extractor_set_digest = Some(batch.header.extractor_set_digest.clone()),
            Some(existing) if existing == &batch.header.extractor_set_digest => {}
            Some(_) => return Err(RemoteStoreError::InvalidInput),
        }

        let FactBatchPayload::ProjectMemory {
            entities: batch_entities,
            relationships: batch_relationships,
            diagnostics: batch_diagnostics,
        } = &batch.payload
        else {
            return Err(RemoteStoreError::InvalidInput);
        };
        merge_facts(&mut entities, batch_entities, |entity| &entity.id)?;
        for relationship in batch_relationships {
            let destination = if is_repository_relationship(relationship) {
                &mut repository_relationships
            } else {
                &mut relationships
            };
            merge_facts(
                destination,
                std::slice::from_ref(relationship),
                |relationship| &relationship.id,
            )?;
        }

        for diagnostic in batch_diagnostics {
            let encoded =
                serde_json::to_vec(diagnostic).map_err(|_| RemoteStoreError::Serialization)?;
            let id = sha256_value(b"ferrus.remote.memory-diagnostic.v1\0", &encoded);
            if diagnostics
                .insert(id, diagnostic.clone())
                .is_some_and(|existing| existing != *diagnostic)
            {
                return Err(RemoteStoreError::FactConflict);
            }
        }
    }

    for relationship in relationships.values() {
        if !entities.contains_key(&relationship.source)
            || matches!(
                &relationship.target,
                MemoryRelationshipTarget::MemoryEntity { entity_id }
                    if !entities.contains_key(entity_id)
            )
        {
            return Err(RemoteStoreError::FactConflict);
        }
    }

    let (revision, project_identity, build_id, repository_link_target) =
        target.ok_or(RemoteStoreError::InvalidInput)?;
    if (repository_link_target.is_none() && !repository_relationships.is_empty())
        || repository_relationships.values().any(|relationship| {
            !entities.contains_key(&relationship.source)
                || repository_link_target
                    .as_ref()
                    .is_none_or(|target| !repository_relationship_matches(relationship, target))
        })
    {
        return Err(RemoteStoreError::FactConflict);
    }

    let repository_relationship_ids = repository_relationships
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    let (repository_diagnostics, diagnostics): (BTreeMap<_, _>, BTreeMap<_, _>) =
        diagnostics.into_iter().partition(|(_, diagnostic)| {
            diagnostic
                .relationship_id
                .as_ref()
                .is_some_and(|id| repository_relationship_ids.contains(id))
        });
    let count = checked_fact_count(
        entities.len(),
        relationships
            .len()
            .saturating_add(repository_relationships.len()),
        diagnostics
            .len()
            .saturating_add(repository_diagnostics.len()),
    )?;
    if count > max_facts.get() {
        return Err(RemoteStoreError::QuotaExceeded);
    }

    let extractor_set_digest = extractor_set_digest.ok_or(RemoteStoreError::InvalidInput)?;
    let fact_set_digest = canonical_digest(&(
        entities.values().collect::<Vec<_>>(),
        relationships.values().collect::<Vec<_>>(),
        diagnostics.values().collect::<Vec<_>>(),
    ))?;
    let mut facts = Vec::with_capacity(count as usize);
    append_facts(
        &mut facts,
        "entity",
        entities
            .into_iter()
            .map(|(id, value)| (id.to_string(), value)),
    )?;
    append_facts(
        &mut facts,
        "relationship",
        relationships
            .into_iter()
            .map(|(id, value)| (id.to_string(), value)),
    )?;
    append_facts(&mut facts, "diagnostic", diagnostics)?;
    let repository_links = repository_link_target
        .map(|target| {
            let fact_set_digest = canonical_digest(&(
                &target,
                repository_relationships.values().collect::<Vec<_>>(),
                repository_diagnostics.values().collect::<Vec<_>>(),
            ))?;
            let counts = RemoteFactCounts {
                primary: 0,
                relationships: repository_relationships.len() as u64,
                diagnostics: repository_diagnostics.len() as u64,
            };
            let mut link_facts = Vec::with_capacity(
                repository_relationships
                    .len()
                    .saturating_add(repository_diagnostics.len()),
            );
            let link_relationships = repository_relationships.values().cloned().collect();
            append_facts(
                &mut link_facts,
                "relationship",
                repository_relationships
                    .into_iter()
                    .map(|(id, value)| (id.to_string(), value)),
            )?;
            append_facts(&mut link_facts, "diagnostic", repository_diagnostics)?;
            Ok::<_, RemoteStoreError>(PreparedMemoryRepositoryLinks {
                target: *target,
                relationships: link_relationships,
                fact_set_digest,
                counts,
                facts: link_facts,
            })
        })
        .transpose()?;
    Ok(PreparedMemory {
        record: RemoteMemoryRevisionRecord {
            revision,
            job: request.job.clone(),
            build_id,
            extractor_set_digest,
            fact_set_digest,
            counts: RemoteFactCounts {
                primary: facts.iter().filter(|fact| fact.kind == "entity").count() as u64,
                relationships: facts
                    .iter()
                    .filter(|fact| fact.kind == "relationship")
                    .count() as u64,
                diagnostics: facts
                    .iter()
                    .filter(|fact| fact.kind == "diagnostic")
                    .count() as u64,
            },
            completed_at: now,
        },
        project_identity,
        facts,
        repository_links,
    })
}

pub(super) fn is_repository_relationship(relationship: &MemoryRelationship) -> bool {
    matches!(
        relationship.target,
        MemoryRelationshipTarget::RepositoryNode { .. }
            | MemoryRelationshipTarget::RepositoryPath { .. }
            | MemoryRelationshipTarget::RepositorySymbol { .. }
    )
}

pub(super) fn repository_relationship_matches(
    relationship: &MemoryRelationship,
    target: &RemoteMemoryLinkSetTarget,
) -> bool {
    if relationship.project != target.link_set.project
        || relationship.memory_revision_id != target.link_set.memory_revision_id
    {
        return false;
    }

    let (repository, snapshot) = match &relationship.target {
        MemoryRelationshipTarget::RepositoryNode {
            repository,
            snapshot_id,
            ..
        } => (repository, Some(snapshot_id)),
        MemoryRelationshipTarget::RepositoryPath {
            repository,
            snapshot_id,
            ..
        }
        | MemoryRelationshipTarget::RepositorySymbol {
            repository,
            snapshot_id,
            ..
        } => (repository, snapshot_id.as_ref()),
        _ => return false,
    };
    if repository != &target.link_set.repository {
        return false;
    }

    match snapshot {
        None => relationship.provenance.resolution == MemoryResolutionState::Unresolved,
        Some(snapshot) if snapshot == &target.graph.snapshot_id => {
            relationship.provenance.resolution == MemoryResolutionState::Resolved
        }
        Some(snapshot) => {
            relationship.provenance.resolution == MemoryResolutionState::Stale
                && target
                    .origin_graphs
                    .iter()
                    .any(|origin| &origin.snapshot_id == snapshot)
        }
    }
}

pub(in super::super) fn validate_batch_stream(
    job: &IndexJobRef,
    batches: &[FactBatch],
) -> Result<(), RemoteStoreError> {
    if batches.is_empty() {
        return Err(RemoteStoreError::InvalidInput);
    }

    let shard = &batches[0].header.shard_id;
    for (index, batch) in batches.iter().enumerate() {
        batch
            .validate()
            .map_err(|_| RemoteStoreError::InvalidInput)?;
        if batch.header.job != *job
            || batch.header.shard_id != *shard
            || usize::try_from(batch.header.sequence).ok() != Some(index)
            || batch.header.final_batch != (index + 1 == batches.len())
        {
            return Err(RemoteStoreError::InvalidInput);
        }
    }

    Ok(())
}

pub(in super::super) fn merge_facts<K, V, F>(
    destination: &mut BTreeMap<K, V>,
    incoming: &[V],
    key: F,
) -> Result<(), RemoteStoreError>
where
    K: Ord + Clone,
    V: Clone + PartialEq,
    F: Fn(&V) -> &K,
{
    for value in incoming {
        let id = key(value).clone();
        if destination
            .insert(id, value.clone())
            .is_some_and(|existing| existing != *value)
        {
            return Err(RemoteStoreError::FactConflict);
        }
    }

    Ok(())
}

pub(in super::super) fn append_facts<T: Serialize>(
    output: &mut Vec<PlainFact>,
    kind: &'static str,
    facts: impl IntoIterator<Item = (String, T)>,
) -> Result<(), RemoteStoreError> {
    for (id, fact) in facts {
        output.push(PlainFact {
            kind,
            id,
            encoded: serde_json::to_vec(&fact).map_err(|_| RemoteStoreError::Serialization)?,
        });
    }

    Ok(())
}

pub(in super::super) fn checked_fact_count(
    primary: usize,
    relationships: usize,
    diagnostics: usize,
) -> Result<u64, RemoteStoreError> {
    [primary, relationships, diagnostics]
        .into_iter()
        .try_fold(0u64, |total, value| {
            total.checked_add(u64::try_from(value).ok()?)
        })
        .ok_or(RemoteStoreError::QuotaExceeded)
}
