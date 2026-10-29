//! Repository link resolution against immutable snapshots.

use super::*;

/// Resolve the portable subset of repository evidence against one immutable
/// graph snapshot. Remote workers use this pure boundary without opening a
/// repository, sidecar, process, or network connection.
pub(crate) fn resolve_repository_links_for_snapshot(
    revision: &MemoryRevision,
    entities: &[MemoryEntity],
    snapshots: RepositoryLinkSnapshotSet<'_>,
    indexed_at: DateTime<Utc>,
    mut deadline_expired: impl FnMut() -> bool,
) -> std::result::Result<MemoryRepositoryLinkCommit, SnapshotLinkResolutionError> {
    check_snapshot_link_deadline(&mut deadline_expired)?;
    let repository = snapshots.repository;
    let snapshot_id = snapshots.current.snapshot_id;
    let nodes = snapshots.current.nodes;
    let origin_snapshots = snapshots.origins;

    let link_set_id = MemoryRepositoryLinkSetId::new(format!(
        "memory-links:{}",
        canonical_digest(&(
            "repository-link-set",
            &revision.project,
            &revision.id,
            repository,
            &Some(snapshot_id),
            entities
                .iter()
                .flat_map(repository_origin_snapshots)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .map(|origin| {
                    let available = origin == *snapshot_id
                        || origin_snapshots
                            .iter()
                            .any(|snapshot| snapshot.snapshot_id == &origin);
                    (origin, available)
                })
                .collect::<Vec<_>>(),
            resolver_identity(),
        ))
        .map_err(|_| SnapshotLinkResolutionError::InvalidSnapshot)?
        .value()
    ))
    .map_err(|_| SnapshotLinkResolutionError::InvalidSnapshot)?;
    let link_set = MemoryRepositoryLinkSet {
        id: link_set_id,
        project: revision.project.clone(),
        memory_revision_id: revision.id.clone(),
        repository: repository.clone(),
        repository_snapshot_id: Some(snapshot_id.clone()),
        resolver: resolver_identity(),
    };

    let mut origin_files = BTreeMap::new();
    origin_files.insert(
        snapshot_id.clone(),
        snapshot_files_from_nodes(snapshot_id, nodes, &mut deadline_expired)?,
    );
    for snapshot in origin_snapshots {
        check_snapshot_link_deadline(&mut deadline_expired)?;
        let files =
            snapshot_files_from_nodes(snapshot.snapshot_id, snapshot.nodes, &mut deadline_expired)?;
        if origin_files
            .insert(snapshot.snapshot_id.clone(), files)
            .is_some()
        {
            return Err(SnapshotLinkResolutionError::InvalidSnapshot);
        }
    }

    let milestones = entities
        .iter()
        .filter_map(|entity| match &entity.data {
            MemoryEntityData::Milestone { milestone_id, .. } => {
                Some((milestone_id.clone(), entity.id.clone()))
            }
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    let mut diagnostics = Vec::new();
    let mut candidates = BTreeMap::new();
    for entity in entities {
        check_snapshot_link_deadline(&mut deadline_expired)?;
        if let MemorySourceLocator::TrackedFile { path } = &entity.provenance.source_locator {
            insert_candidate(
                &mut candidates,
                revision,
                entity.id.clone(),
                MemoryRelationshipKind::Concerns,
                ExplicitRepositoryReference::Path(path.clone()),
                None,
                entity.provenance.clone(),
            )?;
        }

        if let MemoryEntityData::ArchiveReference { spec_path, .. } = &entity.data {
            insert_candidate(
                &mut candidates,
                revision,
                entity.id.clone(),
                MemoryRelationshipKind::Concerns,
                ExplicitRepositoryReference::Path(spec_path.clone()),
                None,
                entity.provenance.clone(),
            )?;
        }

        if let Some(text) = curated_text(&entity.data) {
            for reference in explicit_references_until(text, &mut deadline_expired)? {
                check_snapshot_link_deadline(&mut deadline_expired)?;
                insert_candidate(
                    &mut candidates,
                    revision,
                    entity.id.clone(),
                    MemoryRelationshipKind::Touches,
                    reference,
                    None,
                    entity.provenance.clone(),
                )?;
            }
        }

        match &entity.data {
            MemoryEntityData::TaskReference {
                task_id,
                milestone_id,
                baseline_snapshot_id,
                repository_snapshot_id,
                ..
            } => {
                let changed = portable_origin_changed_paths(
                    baseline_snapshot_id.as_ref(),
                    repository_snapshot_id.as_ref(),
                    &origin_files,
                );
                if baseline_snapshot_id.is_some()
                    && repository_snapshot_id.is_some()
                    && changed.is_none()
                {
                    push_diagnostic(
                        &mut diagnostics,
                        &revision.completed_by,
                        &revision.id,
                        diagnostic_code(
                            const {
                                crate::project_memory::diagnostics::MemoryDiagnosticCode::from_static("link.taskorigin.unavailable")
                            },
                        ),
                        Some(entity.id.clone()),
                        None,
                    );
                }

                for changed in changed.unwrap_or_default() {
                    insert_candidate(
                        &mut candidates,
                        revision,
                        entity.id.clone(),
                        MemoryRelationshipKind::Touches,
                        ExplicitRepositoryReference::Path(changed.path.clone()),
                        Some(changed.origin_snapshot_id.clone()),
                        entity.provenance.clone(),
                    )?;
                    if let Some(milestone_entity) =
                        milestone_id.as_ref().and_then(|id| milestones.get(id))
                    {
                        let mut provenance = entity.provenance.clone();
                        provenance.evidence = MemoryEvidenceLocator::Record(task_id.clone());
                        insert_candidate(
                            &mut candidates,
                            revision,
                            milestone_entity.clone(),
                            MemoryRelationshipKind::Touches,
                            ExplicitRepositoryReference::Path(changed.path),
                            Some(changed.origin_snapshot_id),
                            provenance,
                        )?;
                    }
                }
            }
            MemoryEntityData::RunReference {
                baseline_snapshot_id,
                repository_snapshot_id,
                ..
            } => {
                for changed in portable_origin_changed_paths(
                    baseline_snapshot_id.as_ref(),
                    repository_snapshot_id.as_ref(),
                    &origin_files,
                )
                .unwrap_or_default()
                {
                    insert_candidate(
                        &mut candidates,
                        revision,
                        entity.id.clone(),
                        MemoryRelationshipKind::Touches,
                        ExplicitRepositoryReference::Path(changed.path),
                        Some(changed.origin_snapshot_id),
                        entity.provenance.clone(),
                    )?;
                }
            }
            _ => {}
        }
    }

    let mut paths = BTreeSet::new();
    let mut symbols = BTreeMap::<SemanticKey, Vec<NodeId>>::new();
    for node in nodes {
        check_snapshot_link_deadline(&mut deadline_expired)?;
        if node.snapshot_id != *snapshot_id {
            return Err(SnapshotLinkResolutionError::InvalidSnapshot);
        }

        if let Some(evidence) = &node.provenance.evidence {
            paths.insert(evidence.path.clone());
        }

        if let Some(key) = &node.semantic_key {
            symbols
                .entry(key.clone())
                .or_default()
                .push(node.id.clone());
        }
    }

    if candidates.len() > MAX_REPOSITORY_LINKS {
        push_diagnostic(
            &mut diagnostics,
            &revision.completed_by,
            &revision.id,
            diagnostic_code(
                const {
                    crate::project_memory::diagnostics::MemoryDiagnosticCode::from_static(
                        "link.limit",
                    )
                },
            ),
            None,
            None,
        );
    }

    let mut relationships = Vec::with_capacity(candidates.len().min(MAX_REPOSITORY_LINKS));
    for candidate in candidates.into_values().take(MAX_REPOSITORY_LINKS) {
        check_snapshot_link_deadline(&mut deadline_expired)?;
        let (target, resolution) = match &candidate.reference {
            ExplicitRepositoryReference::Path(path) if paths.contains(path) => (
                MemoryRelationshipTarget::RepositoryPath {
                    repository: repository.clone(),
                    path: path.clone(),
                    snapshot_id: Some(snapshot_id.clone()),
                },
                MemoryResolutionState::Resolved,
            ),
            ExplicitRepositoryReference::Path(path)
                if candidate.origin_snapshot_id.as_ref().is_some_and(|origin| {
                    origin_files
                        .get(origin)
                        .is_some_and(|files| files.contains_key(path))
                }) =>
            {
                let origin = candidate
                    .origin_snapshot_id
                    .as_ref()
                    .ok_or(SnapshotLinkResolutionError::InvalidSnapshot)?;
                (
                    MemoryRelationshipTarget::RepositoryPath {
                        repository: repository.clone(),
                        path: path.clone(),
                        snapshot_id: Some(origin.clone()),
                    },
                    MemoryResolutionState::Stale,
                )
            }
            ExplicitRepositoryReference::Path(path) => (
                MemoryRelationshipTarget::RepositoryPath {
                    repository: repository.clone(),
                    path: path.clone(),
                    snapshot_id: None,
                },
                MemoryResolutionState::Unresolved,
            ),
            ExplicitRepositoryReference::Symbol(key)
                if symbols.get(key).is_some_and(|matches| matches.len() == 1) =>
            {
                let node_id = symbols
                    .get(key)
                    .and_then(|matches| matches.first())
                    .ok_or(SnapshotLinkResolutionError::InvalidSnapshot)?
                    .clone();
                (
                    MemoryRelationshipTarget::RepositoryNode {
                        repository: repository.clone(),
                        snapshot_id: snapshot_id.clone(),
                        node_id,
                        semantic_key: Some(key.clone()),
                    },
                    MemoryResolutionState::Resolved,
                )
            }
            ExplicitRepositoryReference::Symbol(key) => (
                MemoryRelationshipTarget::RepositorySymbol {
                    repository: repository.clone(),
                    semantic_key: key.clone(),
                    snapshot_id: None,
                },
                MemoryResolutionState::Unresolved,
            ),
        };
        let mut provenance = candidate.provenance;
        provenance.extractor = resolver_identity();
        provenance.resolution = resolution;
        provenance.confidence = MemoryConfidence::Exact;
        provenance.timestamps = MemoryIndexTimestamps {
            source_observed_at: provenance.timestamps.source_observed_at,
            indexed_at,
        };
        let relationship = MemoryRelationship {
            project: revision.project.clone(),
            memory_revision_id: revision.id.clone(),
            id: candidate.id,
            kind: candidate.kind,
            source: candidate.source,
            target,
            provenance,
        };
        if relationship.provenance.resolution == MemoryResolutionState::Unresolved {
            push_diagnostic(
                &mut diagnostics,
                &revision.completed_by,
                &revision.id,
                diagnostic_code(
                    const {
                        crate::project_memory::diagnostics::MemoryDiagnosticCode::from_static(
                            "link.unresolved",
                        )
                    },
                ),
                Some(relationship.source.clone()),
                Some(relationship.id.clone()),
            );
        }

        relationships.push(relationship);
    }

    Ok(MemoryRepositoryLinkCommit {
        link_set,
        relationships,
        diagnostics,
    })
}

pub(super) fn check_snapshot_link_deadline(
    deadline_expired: &mut impl FnMut() -> bool,
) -> std::result::Result<(), SnapshotLinkResolutionError> {
    if deadline_expired() {
        Err(SnapshotLinkResolutionError::DeadlineExceeded)
    } else {
        Ok(())
    }
}

pub(super) fn snapshot_files_from_nodes(
    snapshot_id: &SnapshotId,
    nodes: &[GraphNode],
    deadline_expired: &mut impl FnMut() -> bool,
) -> std::result::Result<BTreeMap<RepoPath, Digest>, SnapshotLinkResolutionError> {
    let mut files = BTreeMap::new();
    for node in nodes {
        check_snapshot_link_deadline(deadline_expired)?;
        if node.snapshot_id != *snapshot_id {
            return Err(SnapshotLinkResolutionError::InvalidSnapshot);
        }

        let Some(evidence) = &node.provenance.evidence else {
            continue;
        };
        if files
            .insert(evidence.path.clone(), evidence.content_identity.clone())
            .is_some_and(|existing| existing != evidence.content_identity)
        {
            return Err(SnapshotLinkResolutionError::InvalidSnapshot);
        }

        if files.len() > MAX_ORIGIN_FILES {
            return Err(SnapshotLinkResolutionError::InvalidSnapshot);
        }
    }

    Ok(files)
}

pub(super) fn portable_origin_changed_paths(
    baseline: Option<&SnapshotId>,
    view: Option<&SnapshotId>,
    snapshots: &BTreeMap<SnapshotId, BTreeMap<RepoPath, Digest>>,
) -> Option<Vec<ChangedPath>> {
    let (Some(baseline), Some(view)) = (baseline, view) else {
        return None;
    };
    let (Some(baseline_files), Some(view_files)) = (snapshots.get(baseline), snapshots.get(view))
    else {
        return None;
    };
    let paths = baseline_files
        .keys()
        .chain(view_files.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    Some(
        paths
            .into_iter()
            .filter_map(|path| {
                let baseline_identity = baseline_files.get(&path);
                let view_identity = view_files.get(&path);
                (baseline_identity != view_identity).then(|| ChangedPath {
                    origin_snapshot_id: if view_identity.is_some() {
                        view.clone()
                    } else {
                        baseline.clone()
                    },
                    path,
                })
            })
            .collect(),
    )
}
