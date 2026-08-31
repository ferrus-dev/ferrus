//! Evidence-backed links from immutable project memory to repository snapshots.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::repository_graph::{
    domain::{
        Digest, GraphNode, NodeId, PublishedViewName, RepoPath, RepositoryId, RepositoryNamespace,
        RepositoryRef, SemanticKey, SnapshotId,
    },
    sqlite::{OpenQuerySidecarResult, SIDECAR_FILE_NAME, Sidecar, open_for_query_at},
};

use super::{
    diagnostics::{MemoryDiagnostic, MemoryDiagnosticCode, MemoryDiagnosticSeverity},
    domain::{
        MemoryBuildId, MemoryConfidence, MemoryEntity, MemoryEntityData, MemoryEntityId,
        MemoryEvidenceLocator, MemoryExtractorIdentity, MemoryIndexTimestamps, MemoryProvenance,
        MemoryRelationship, MemoryRelationshipId, MemoryRelationshipKind, MemoryRelationshipTarget,
        MemoryRepositoryLinkCommit, MemoryRepositoryLinkSet, MemoryRepositoryLinkSetId,
        MemoryResolutionState, MemoryRevision, MemorySourceLocator, ProjectRef,
    },
    extractors::{canonical_digest, extractor_identity, relationship_id},
};

const CANONICAL_VIEW: &str = "canonical";
const MAX_LINK_DIAGNOSTICS: usize = 1_000;
const MAX_REPOSITORY_LINKS: usize = 20_000;
const MAX_ORIGIN_FILES: usize = 100_000;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
enum ExplicitRepositoryReference {
    Path(RepoPath),
    Symbol(SemanticKey),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SnapshotLinkResolutionError {
    InvalidSnapshot,
    DeadlineExceeded,
}

#[derive(Clone, Copy)]
pub(crate) struct RepositoryLinkSnapshot<'a> {
    pub snapshot_id: &'a SnapshotId,
    pub nodes: &'a [GraphNode],
}

pub(crate) struct RepositoryLinkSnapshotSet<'a> {
    pub repository: &'a RepositoryRef,
    pub current: RepositoryLinkSnapshot<'a>,
    pub origins: &'a [RepositoryLinkSnapshot<'a>],
}

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
        .value()
    ))
    .expect("sha256 memory link-set identity is bounded");
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
            );
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
            );
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
                );
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
                        diagnostic_code("link.taskorigin.unavailable"),
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
                    );
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
                        );
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
                    );
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
            diagnostic_code("link.limit"),
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
                    .expect("matched origin path has a snapshot");
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
                    .expect("unique semantic key has one node")
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
                diagnostic_code("link.unresolved"),
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

fn check_snapshot_link_deadline(
    deadline_expired: &mut impl FnMut() -> bool,
) -> std::result::Result<(), SnapshotLinkResolutionError> {
    if deadline_expired() {
        Err(SnapshotLinkResolutionError::DeadlineExceeded)
    } else {
        Ok(())
    }
}

fn snapshot_files_from_nodes(
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

fn portable_origin_changed_paths(
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

#[derive(Debug, Clone)]
struct LinkCandidate {
    id: MemoryRelationshipId,
    source: MemoryEntityId,
    kind: MemoryRelationshipKind,
    reference: ExplicitRepositoryReference,
    origin_snapshot_id: Option<SnapshotId>,
    provenance: MemoryProvenance,
}

#[derive(Debug, Clone)]
struct ChangedPath {
    path: RepoPath,
    origin_snapshot_id: SnapshotId,
}

pub(crate) struct LocalRepositoryLinkResolver {
    repository: RepositoryRef,
    sidecar: Option<Sidecar>,
    repository_snapshot_id: Option<SnapshotId>,
    availability_diagnostic: Option<MemoryDiagnosticCode>,
}

impl LocalRepositoryLinkResolver {
    pub(crate) fn open(data_dir: &std::path::Path, project: &ProjectRef) -> Result<Self> {
        let repository = RepositoryRef {
            namespace: RepositoryNamespace::new(format!("local:{}", project.project_id.as_str()))?,
            repository_id: RepositoryId::new("root")?,
        };
        let path = data_dir.join(SIDECAR_FILE_NAME);
        let (sidecar, repository_snapshot_id, availability_diagnostic) =
            match open_for_query_at(&path) {
                Ok(OpenQuerySidecarResult::Ready(sidecar)) => {
                    let view = sidecar.published_view(
                        &repository,
                        &PublishedViewName::new(CANONICAL_VIEW)
                            .expect("canonical view name is valid"),
                    );
                    match view {
                        Ok(Some(view)) => (Some(sidecar), Some(view.snapshot_id), None),
                        Ok(None) => (
                            None,
                            None,
                            Some(diagnostic_code("link.repository.notbuilt")),
                        ),
                        Err(_) => (
                            None,
                            None,
                            Some(diagnostic_code("link.repository.unavailable")),
                        ),
                    }
                }
                Ok(OpenQuerySidecarResult::Absent) => (
                    None,
                    None,
                    Some(diagnostic_code("link.repository.notbuilt")),
                ),
                Ok(OpenQuerySidecarResult::NeedsMigration { .. })
                | Ok(OpenQuerySidecarResult::RequiresRebuild(_))
                | Err(_) => (
                    None,
                    None,
                    Some(diagnostic_code("link.repository.unavailable")),
                ),
            };
        Ok(Self {
            repository,
            sidecar,
            repository_snapshot_id,
            availability_diagnostic,
        })
    }

    pub(crate) fn repository(&self) -> &RepositoryRef {
        &self.repository
    }

    pub(crate) fn link_set_id(
        &self,
        revision: &MemoryRevision,
        entities: &[MemoryEntity],
    ) -> Result<MemoryRepositoryLinkSetId> {
        let mut origin_snapshots = entities
            .iter()
            .flat_map(repository_origin_snapshots)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|snapshot| {
                let available = self
                    .sidecar
                    .as_ref()
                    .map(|sidecar| snapshot_belongs_to(sidecar, &snapshot, &self.repository))
                    .transpose()?
                    .unwrap_or(false);
                Ok((snapshot, available))
            })
            .collect::<Result<Vec<_>>>()?;
        origin_snapshots.sort();
        let digest = canonical_digest(&(
            "repository-link-set",
            &revision.project,
            &revision.id,
            &self.repository,
            &self.repository_snapshot_id,
            origin_snapshots,
            resolver_identity(),
        ));
        Ok(
            MemoryRepositoryLinkSetId::new(format!("memory-links:{}", digest.value()))
                .expect("sha256 memory link-set identity is bounded"),
        )
    }

    pub(crate) fn resolve(
        &self,
        revision: &MemoryRevision,
        entities: &[MemoryEntity],
        previous: Option<(&MemoryRepositoryLinkSet, &[MemoryRelationship])>,
    ) -> Result<MemoryRepositoryLinkCommit> {
        let link_set_id = self.link_set_id(revision, entities)?;
        if let Some((previous_set, relationships)) = previous
            && previous_set.id == link_set_id
        {
            let diagnostics = link_diagnostics(
                revision,
                relationships,
                self.availability_diagnostic.clone(),
            );
            return Ok(MemoryRepositoryLinkCommit {
                link_set: previous_set.clone(),
                relationships: relationships.to_vec(),
                diagnostics,
            });
        }

        let previous_relationships = previous
            .filter(|_| self.repository_snapshot_id.is_some())
            .map(|(_, relationships)| {
                relationships
                    .iter()
                    .map(|relationship| (relationship.id.clone(), relationship))
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default();
        let link_set = MemoryRepositoryLinkSet {
            id: link_set_id,
            project: revision.project.clone(),
            memory_revision_id: revision.id.clone(),
            repository: self.repository.clone(),
            repository_snapshot_id: self.repository_snapshot_id.clone(),
            resolver: resolver_identity(),
        };
        let mut diagnostics = Vec::new();
        if let Some(code) = self.availability_diagnostic.clone() {
            push_diagnostic(
                &mut diagnostics,
                &revision.completed_by,
                &revision.id,
                code,
                None,
                None,
            );
        }
        let candidates = self.candidates(revision, entities, &mut diagnostics)?;
        if candidates.len() > MAX_REPOSITORY_LINKS {
            push_diagnostic(
                &mut diagnostics,
                &revision.completed_by,
                &revision.id,
                diagnostic_code("link.limit"),
                None,
                None,
            );
        }
        let mut relationships = Vec::with_capacity(candidates.len().min(MAX_REPOSITORY_LINKS));
        for candidate in candidates.into_values().take(MAX_REPOSITORY_LINKS) {
            let previous = previous_relationships.get(&candidate.id).copied();
            let relationship = self.resolve_candidate(revision, candidate, previous)?;
            if relationship.provenance.resolution != MemoryResolutionState::Resolved {
                let code = match relationship.provenance.resolution {
                    MemoryResolutionState::Stale => "link.stale",
                    MemoryResolutionState::Unresolved => "link.unresolved",
                    MemoryResolutionState::Resolved => unreachable!(),
                };
                push_diagnostic(
                    &mut diagnostics,
                    &revision.completed_by,
                    &revision.id,
                    diagnostic_code(code),
                    Some(relationship.source.clone()),
                    Some(relationship.id.clone()),
                );
            }
            relationships.push(relationship);
        }
        relationships.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(MemoryRepositoryLinkCommit {
            link_set,
            relationships,
            diagnostics,
        })
    }

    fn candidates(
        &self,
        revision: &MemoryRevision,
        entities: &[MemoryEntity],
        diagnostics: &mut Vec<MemoryDiagnostic>,
    ) -> Result<BTreeMap<MemoryRelationshipId, LinkCandidate>> {
        let mut candidates = BTreeMap::new();
        let milestones = entities
            .iter()
            .filter_map(|entity| match &entity.data {
                MemoryEntityData::Milestone { milestone_id, .. } => {
                    Some((milestone_id.clone(), entity.id.clone()))
                }
                _ => None,
            })
            .collect::<BTreeMap<_, _>>();

        for entity in entities {
            if let MemorySourceLocator::TrackedFile { path } = &entity.provenance.source_locator {
                insert_candidate(
                    &mut candidates,
                    revision,
                    entity.id.clone(),
                    MemoryRelationshipKind::Concerns,
                    ExplicitRepositoryReference::Path(path.clone()),
                    None,
                    entity.provenance.clone(),
                );
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
                );
            }
            if let Some(text) = curated_text(&entity.data) {
                for reference in explicit_references(text) {
                    insert_candidate(
                        &mut candidates,
                        revision,
                        entity.id.clone(),
                        MemoryRelationshipKind::Touches,
                        reference,
                        None,
                        entity.provenance.clone(),
                    );
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
                    let changed = self.origin_changed_paths(
                        baseline_snapshot_id.as_ref(),
                        repository_snapshot_id.as_ref(),
                    )?;
                    if baseline_snapshot_id.is_some()
                        && repository_snapshot_id.is_some()
                        && changed.is_none()
                    {
                        push_diagnostic(
                            diagnostics,
                            &revision.completed_by,
                            &revision.id,
                            diagnostic_code("link.taskorigin.unavailable"),
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
                        );
                        if let Some(milestone_entity) =
                            milestone_id.as_ref().and_then(|id| milestones.get(id))
                        {
                            let mut provenance = entity.provenance.clone();
                            provenance.evidence =
                                super::domain::MemoryEvidenceLocator::Record(task_id.clone());
                            insert_candidate(
                                &mut candidates,
                                revision,
                                milestone_entity.clone(),
                                MemoryRelationshipKind::Touches,
                                ExplicitRepositoryReference::Path(changed.path),
                                Some(changed.origin_snapshot_id),
                                provenance,
                            );
                        }
                    }
                }
                MemoryEntityData::RunReference {
                    baseline_snapshot_id,
                    repository_snapshot_id,
                    ..
                } => {
                    for changed in self
                        .origin_changed_paths(
                            baseline_snapshot_id.as_ref(),
                            repository_snapshot_id.as_ref(),
                        )?
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
                        );
                    }
                }
                _ => {}
            }
        }
        Ok(candidates)
    }

    fn resolve_candidate(
        &self,
        revision: &MemoryRevision,
        candidate: LinkCandidate,
        previous: Option<&MemoryRelationship>,
    ) -> Result<MemoryRelationship> {
        let current = self.repository_snapshot_id.as_ref();
        let (target, resolution) = match &candidate.reference {
            ExplicitRepositoryReference::Path(path) => {
                let current_match = current
                    .map(|snapshot| self.path_exists(snapshot, path))
                    .transpose()?
                    .unwrap_or(false);
                let origin_match = candidate
                    .origin_snapshot_id
                    .as_ref()
                    .map(|snapshot| self.path_exists(snapshot, path))
                    .transpose()?
                    .unwrap_or(false);
                if current_match {
                    let snapshot = current.expect("current path match requires a snapshot");
                    (
                        MemoryRelationshipTarget::RepositoryPath {
                            repository: self.repository.clone(),
                            path: path.clone(),
                            snapshot_id: Some(snapshot.clone()),
                        },
                        MemoryResolutionState::Resolved,
                    )
                } else if origin_match {
                    let origin = candidate
                        .origin_snapshot_id
                        .as_ref()
                        .expect("origin path match requires a snapshot");
                    (
                        MemoryRelationshipTarget::RepositoryPath {
                            repository: self.repository.clone(),
                            path: path.clone(),
                            snapshot_id: Some(origin.clone()),
                        },
                        MemoryResolutionState::Stale,
                    )
                } else if let Some(previous) = previous.filter(|relationship| {
                    relationship.provenance.resolution != MemoryResolutionState::Unresolved
                }) {
                    (previous.target.clone(), MemoryResolutionState::Stale)
                } else {
                    (
                        MemoryRelationshipTarget::RepositoryPath {
                            repository: self.repository.clone(),
                            path: path.clone(),
                            snapshot_id: None,
                        },
                        MemoryResolutionState::Unresolved,
                    )
                }
            }
            ExplicitRepositoryReference::Symbol(semantic_key) => {
                let current_matches = current
                    .map(|snapshot| self.symbol_nodes(snapshot, semantic_key))
                    .transpose()?
                    .unwrap_or_default();
                if current_matches.len() == 1 {
                    (
                        MemoryRelationshipTarget::RepositoryNode {
                            repository: self.repository.clone(),
                            snapshot_id: current.expect("current snapshot was queried").clone(),
                            node_id: current_matches[0].clone(),
                            semantic_key: Some(semantic_key.clone()),
                        },
                        MemoryResolutionState::Resolved,
                    )
                } else if !current_matches.is_empty() {
                    (
                        MemoryRelationshipTarget::RepositorySymbol {
                            repository: self.repository.clone(),
                            semantic_key: semantic_key.clone(),
                            snapshot_id: current.cloned(),
                        },
                        MemoryResolutionState::Unresolved,
                    )
                } else if let Some(origin) = candidate.origin_snapshot_id.as_ref() {
                    let origin_matches = self.symbol_nodes(origin, semantic_key)?;
                    if origin_matches.len() == 1 {
                        (
                            MemoryRelationshipTarget::RepositoryNode {
                                repository: self.repository.clone(),
                                snapshot_id: origin.clone(),
                                node_id: origin_matches[0].clone(),
                                semantic_key: Some(semantic_key.clone()),
                            },
                            MemoryResolutionState::Stale,
                        )
                    } else if let Some(previous) = previous.filter(|relationship| {
                        relationship.provenance.resolution != MemoryResolutionState::Unresolved
                    }) {
                        (previous.target.clone(), MemoryResolutionState::Stale)
                    } else {
                        (
                            MemoryRelationshipTarget::RepositorySymbol {
                                repository: self.repository.clone(),
                                semantic_key: semantic_key.clone(),
                                snapshot_id: None,
                            },
                            MemoryResolutionState::Unresolved,
                        )
                    }
                } else if let Some(previous) = previous.filter(|relationship| {
                    relationship.provenance.resolution != MemoryResolutionState::Unresolved
                }) {
                    (previous.target.clone(), MemoryResolutionState::Stale)
                } else {
                    (
                        MemoryRelationshipTarget::RepositorySymbol {
                            repository: self.repository.clone(),
                            semantic_key: semantic_key.clone(),
                            snapshot_id: None,
                        },
                        MemoryResolutionState::Unresolved,
                    )
                }
            }
        };
        let mut provenance = candidate.provenance;
        provenance.extractor = resolver_identity();
        provenance.resolution = resolution;
        provenance.confidence = MemoryConfidence::Exact;
        provenance.timestamps = MemoryIndexTimestamps {
            source_observed_at: provenance.timestamps.source_observed_at,
            indexed_at: Utc::now(),
        };
        Ok(MemoryRelationship {
            project: revision.project.clone(),
            memory_revision_id: revision.id.clone(),
            id: candidate.id,
            kind: candidate.kind,
            source: candidate.source,
            target,
            provenance,
        })
    }

    fn origin_changed_paths(
        &self,
        baseline: Option<&SnapshotId>,
        view: Option<&SnapshotId>,
    ) -> Result<Option<Vec<ChangedPath>>> {
        let (Some(sidecar), Some(baseline), Some(view)) = (&self.sidecar, baseline, view) else {
            return Ok(None);
        };
        if !snapshot_belongs_to(sidecar, baseline, &self.repository)?
            || !snapshot_belongs_to(sidecar, view, &self.repository)?
        {
            return Ok(None);
        }
        let baseline_files = snapshot_files(sidecar, baseline)?;
        let view_files = snapshot_files(sidecar, view)?;
        let paths = baseline_files
            .keys()
            .chain(view_files.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        Ok(Some(
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
        ))
    }

    fn path_exists(&self, snapshot: &SnapshotId, path: &RepoPath) -> Result<bool> {
        let Some(sidecar) = &self.sidecar else {
            return Ok(false);
        };
        sidecar
            .connection()
            .query_row(
                "SELECT 1 FROM files WHERE snapshot_id = ?1 AND path = ?2",
                params![snapshot.as_str(), path.as_str()],
                |_| Ok(()),
            )
            .optional()
            .map(|row| row.is_some())
            .context("failed to resolve repository path evidence")
    }

    fn symbol_nodes(
        &self,
        snapshot: &SnapshotId,
        semantic_key: &SemanticKey,
    ) -> Result<Vec<NodeId>> {
        let Some(sidecar) = &self.sidecar else {
            return Ok(Vec::new());
        };
        let mut statement = sidecar.connection().prepare(
            "SELECT id FROM nodes WHERE snapshot_id = ?1 AND semantic_key = ?2 \
             ORDER BY id LIMIT 2",
        )?;
        let rows = statement
            .query_map(params![snapshot.as_str(), semantic_key.as_str()], |row| {
                row.get::<_, String>(0)
            })?;
        rows.map(|row| Ok(NodeId::new(row?)?)).collect()
    }
}

fn insert_candidate(
    candidates: &mut BTreeMap<MemoryRelationshipId, LinkCandidate>,
    revision: &MemoryRevision,
    source: MemoryEntityId,
    kind: MemoryRelationshipKind,
    reference: ExplicitRepositoryReference,
    origin_snapshot_id: Option<SnapshotId>,
    provenance: MemoryProvenance,
) {
    if candidates.len() > MAX_REPOSITORY_LINKS {
        return;
    }
    let id = relationship_id(
        &revision.project,
        &("repository-link", &source, kind, &reference),
    );
    candidates.entry(id.clone()).or_insert(LinkCandidate {
        id,
        source,
        kind,
        reference,
        origin_snapshot_id,
        provenance,
    });
}

fn curated_text(data: &MemoryEntityData) -> Option<&str> {
    match data {
        MemoryEntityData::Outcome { text }
        | MemoryEntityData::Decision { text }
        | MemoryEntityData::Deviation { text }
        | MemoryEntityData::FollowUpWork { text, .. } => Some(text.as_str()),
        MemoryEntityData::ValidationEvidence {
            text: Some(text), ..
        } => Some(text.as_str()),
        _ => None,
    }
}

fn repository_origin_snapshots(entity: &MemoryEntity) -> Vec<SnapshotId> {
    match &entity.data {
        MemoryEntityData::TaskReference {
            baseline_snapshot_id,
            repository_snapshot_id,
            ..
        }
        | MemoryEntityData::RunReference {
            baseline_snapshot_id,
            repository_snapshot_id,
            ..
        } => baseline_snapshot_id
            .iter()
            .chain(repository_snapshot_id.iter())
            .cloned()
            .collect(),
        _ => Vec::new(),
    }
}

fn explicit_references(text: &str) -> Vec<ExplicitRepositoryReference> {
    explicit_references_until(text, &mut || false)
        .expect("an unbounded reference scan cannot reach its deadline")
}

fn explicit_references_until(
    text: &str,
    deadline_expired: &mut impl FnMut() -> bool,
) -> std::result::Result<Vec<ExplicitRepositoryReference>, SnapshotLinkResolutionError> {
    let mut references = BTreeSet::new();
    for (index, code) in text.split('`').enumerate() {
        check_snapshot_link_deadline(deadline_expired)?;
        if index % 2 == 0 || code.contains(['\n', '\r']) {
            continue;
        }
        let code = code.trim();
        if let Some(path) = code.strip_prefix("path:").map(str::trim)
            && let Ok(path) = RepoPath::new(path)
        {
            references.insert(ExplicitRepositoryReference::Path(path));
        } else if let Some(symbol) = code.strip_prefix("symbol:").map(str::trim)
            && let Ok(symbol) = SemanticKey::new(symbol)
        {
            references.insert(ExplicitRepositoryReference::Symbol(symbol));
        }
    }
    Ok(references.into_iter().collect())
}

fn snapshot_files(
    sidecar: &Sidecar,
    snapshot: &SnapshotId,
) -> Result<BTreeMap<RepoPath, (String, String)>> {
    let mut statement = sidecar.connection().prepare(
        "SELECT path, content_algorithm, content_digest FROM files \
         WHERE snapshot_id = ?1 ORDER BY path LIMIT ?2",
    )?;
    let rows = statement.query_map(
        params![snapshot.as_str(), MAX_ORIGIN_FILES as i64 + 1],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        },
    )?;
    let files = rows
        .map(|row| {
            let (path, algorithm, digest) = row?;
            Ok((RepoPath::new(path)?, (algorithm, digest)))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    if files.len() > MAX_ORIGIN_FILES {
        anyhow::bail!("repository origin snapshot exceeds the memory link file budget");
    }
    Ok(files)
}

fn snapshot_belongs_to(
    sidecar: &Sidecar,
    snapshot: &SnapshotId,
    repository: &RepositoryRef,
) -> Result<bool> {
    sidecar
        .connection()
        .query_row(
            "SELECT repository_namespace, repository_id FROM snapshots WHERE id = ?1",
            [snapshot.as_str()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map(|row| {
            row.is_some_and(|(namespace, repository_id)| {
                namespace == repository.namespace.as_str()
                    && repository_id == repository.repository_id.as_str()
            })
        })
        .context("failed to validate repository snapshot origin")
}

fn resolver_identity() -> MemoryExtractorIdentity {
    extractor_identity("ferrus.repository-link-resolver")
}

fn diagnostic_code(value: &str) -> MemoryDiagnosticCode {
    MemoryDiagnosticCode::new(value).expect("static memory link diagnostic code is valid")
}

fn push_diagnostic(
    diagnostics: &mut Vec<MemoryDiagnostic>,
    build_id: &MemoryBuildId,
    revision_id: &super::domain::MemoryRevisionId,
    code: MemoryDiagnosticCode,
    entity_id: Option<MemoryEntityId>,
    relationship_id: Option<MemoryRelationshipId>,
) {
    if diagnostics.len() >= MAX_LINK_DIAGNOSTICS {
        return;
    }
    diagnostics.push(MemoryDiagnostic {
        build_id: build_id.clone(),
        revision_id: revision_id.clone(),
        severity: MemoryDiagnosticSeverity::Warning,
        code,
        source_category: None,
        entity_id,
        relationship_id,
        metrics: BTreeMap::new(),
    });
}

fn link_diagnostics(
    revision: &MemoryRevision,
    relationships: &[MemoryRelationship],
    availability: Option<MemoryDiagnosticCode>,
) -> Vec<MemoryDiagnostic> {
    let mut diagnostics = Vec::new();
    if let Some(code) = availability {
        push_diagnostic(
            &mut diagnostics,
            &revision.completed_by,
            &revision.id,
            code,
            None,
            None,
        );
    }
    for relationship in relationships {
        let code = match relationship.provenance.resolution {
            MemoryResolutionState::Resolved => continue,
            MemoryResolutionState::Stale => "link.stale",
            MemoryResolutionState::Unresolved => "link.unresolved",
        };
        push_diagnostic(
            &mut diagnostics,
            &revision.completed_by,
            &revision.id,
            diagnostic_code(code),
            Some(relationship.source.clone()),
            Some(relationship.id.clone()),
        );
    }
    diagnostics
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::{
        project_memory::domain::{MemoryRevisionId, ProjectId, ProjectNamespace},
        repository_graph::domain::{
            Confidence, Digest, ExtractorId, ExtractorIdentity, FactProvenance, ResolutionState,
        },
    };

    #[test]
    fn only_explicit_inline_repository_references_are_accepted() {
        let references = explicit_references(
            "Use src/lib.rs, `path:src/lib.rs`, `symbol:rust:function:src/lib.rs:run`, and `https://example.com`.",
        );
        assert_eq!(references.len(), 2);
        assert!(matches!(
            references[0],
            ExplicitRepositoryReference::Path(_)
        ));
        assert!(matches!(
            references[1],
            ExplicitRepositoryReference::Symbol(_)
        ));
    }

    #[test]
    fn snapshot_link_resolution_checks_the_deadline_during_node_scans() {
        let snapshot_id = SnapshotId::new("snapshot").unwrap();
        let revision = MemoryRevision {
            id: MemoryRevisionId::new("revision").unwrap(),
            project: ProjectRef {
                namespace: ProjectNamespace::new("local:test").unwrap(),
                project_id: ProjectId::new("project").unwrap(),
            },
            source_set_digest: Digest::new("sha256", "11").unwrap(),
            policy_digest: Digest::new("sha256", "22").unwrap(),
            memory_model_version: 1,
            extractor_set_digest: Digest::new("sha256", "33").unwrap(),
            completed_by: MemoryBuildId::new("build").unwrap(),
        };
        let repository = RepositoryRef {
            namespace: RepositoryNamespace::new("local:test").unwrap(),
            repository_id: RepositoryId::new("repository").unwrap(),
        };
        let node = GraphNode {
            snapshot_id: snapshot_id.clone(),
            id: NodeId::new("node").unwrap(),
            kind: "symbol".to_string(),
            semantic_key: None,
            provenance: FactProvenance {
                extractor: ExtractorIdentity {
                    id: ExtractorId::new("test.graph").unwrap(),
                    version: "1".to_string(),
                    contract_version: 1,
                },
                evidence: None,
                resolution: ResolutionState::Resolved,
                confidence: Confidence::Exact,
            },
            properties: BTreeMap::new(),
        };
        let checks = Cell::new(0usize);

        let result = resolve_repository_links_for_snapshot(
            &revision,
            &[],
            RepositoryLinkSnapshotSet {
                repository: &repository,
                current: RepositoryLinkSnapshot {
                    snapshot_id: &snapshot_id,
                    nodes: &[node],
                },
                origins: &[],
            },
            Utc::now(),
            || {
                checks.set(checks.get() + 1);
                checks.get() > 1
            },
        );

        assert_eq!(result, Err(SnapshotLinkResolutionError::DeadlineExceeded));
    }

    #[test]
    fn explicit_reference_scan_checks_the_deadline_between_candidates() {
        let checks = Cell::new(0usize);
        let result = explicit_references_until(
            "`path:src/lib.rs` and `symbol:rust:function:src/lib.rs:run`",
            &mut || {
                checks.set(checks.get() + 1);
                checks.get() > 2
            },
        );

        assert_eq!(result, Err(SnapshotLinkResolutionError::DeadlineExceeded));
    }
}
