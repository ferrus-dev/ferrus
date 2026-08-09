//! Authenticated SQLite prototype for distributed control and pinned queries.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::{
    project_memory::domain::{
        MemoryEntity, MemoryEntityData, MemoryRelationship, MemoryRelationshipTarget,
    },
    project_memory::policy::MemoryContentAccess,
    repository_graph::{
        domain::{EdgeTarget, GraphNode, GraphValue, NodeId},
        query::EdgeDirection,
    },
};

use super::{
    DISTRIBUTED_CONTROL_PROTOCOL_VERSION, DISTRIBUTED_QUERY_PROTOCOL_VERSION,
    api::{
        RemoteContextData, RemoteContextSeed, RemoteControlApi, RemoteControlResponse,
        RemoteNeighborhoodData, RemotePageInfo, RemoteQueryBody, RemoteQueryData,
        RemoteQueryDiagnostic, RemoteQueryLimits, RemoteQueryOperation, RemoteQueryResult,
        RemoteSearchData, RemoteSearchItem, RemoteSearchMatchKind, RemoteSnapshotQueryApi,
        RemoteStatusData, RemoteTruncationReason, RemoteVerifiedSnippet,
    },
    coordinator::IndexJobCoordinator,
    coordinator_sqlite::{CoordinatorError, SqliteIndexJobCoordinator},
    identity::{
        RemoteGraphSnapshotRef, RemoteMemoryRevisionRef, RemotePageCursor, RemoteProjectRef,
        RequestId,
    },
    object_store::{EncryptedFilesystemObjectStore, ObjectStoreError, TenantObjectStore},
    protocol::{
        CancelIndexJobRequest, DistributedProtocolError, IndexInputRef, IndexJobRecord,
        InspectIndexJobRequest, RemoteError, RemoteErrorCode, RemoteQueryRequest,
        RemoteQueryResponse, RemoteQueryTarget, SubmitIndexJobRequest,
    },
    publication::{RemotePublicationStore, StoredRemoteGraphSnapshot, StoredRemoteMemoryRevision},
    publication_sqlite::{RemoteStoreError, SqliteRemotePublicationStore},
    security::{AuthorizationContext, AuthorizationScope, RemotePermission},
    source::{
        MemorySourceManifest, MemorySourceManifestBody, RepositorySourceManifest,
        RepositorySourceManifestBody,
    },
};

mod control;
pub use control::*;

pub struct SqliteRemoteQueryApi {
    publication: SqliteRemotePublicationStore,
    coordinator: SqliteIndexJobCoordinator,
    objects: EncryptedFilesystemObjectStore,
    limits: RemoteQueryLimits,
}

impl SqliteRemoteQueryApi {
    pub fn new(
        publication: SqliteRemotePublicationStore,
        coordinator: SqliteIndexJobCoordinator,
        objects: EncryptedFilesystemObjectStore,
        limits: RemoteQueryLimits,
    ) -> Self {
        Self {
            publication,
            coordinator,
            objects,
            limits,
        }
    }

    fn resolve_target(
        &self,
        request_id: &RequestId,
        target: &RemoteQueryTarget,
    ) -> Result<RemoteQueryTarget, RemoteError> {
        let resolved = match target {
            RemoteQueryTarget::Repository(snapshot) => {
                RemoteQueryTarget::Repository(snapshot.clone())
            }
            RemoteQueryTarget::Memory(revision) => RemoteQueryTarget::Memory(revision.clone()),
            RemoteQueryTarget::Federated(view) => RemoteQueryTarget::Federated(view.clone()),
            RemoteQueryTarget::RepositoryView {
                repository,
                view_name,
            } => {
                let view = self
                    .publication
                    .graph_view(repository, view_name)
                    .map_err(|_| query_internal(request_id))?
                    .ok_or_else(|| query_not_found(request_id))?;
                RemoteQueryTarget::Repository(RemoteGraphSnapshotRef {
                    repository: repository.clone(),
                    snapshot_id: view.snapshot_id,
                })
            }
            RemoteQueryTarget::MemoryView { project, view_name } => {
                let view = self
                    .publication
                    .memory_view(project, view_name)
                    .map_err(|_| query_internal(request_id))?
                    .ok_or_else(|| query_not_found(request_id))?;
                RemoteQueryTarget::Memory(RemoteMemoryRevisionRef {
                    project: project.clone(),
                    revision_id: view.revision_id,
                })
            }
            RemoteQueryTarget::FederatedView {
                repository,
                graph_view,
                memory_view,
            } => {
                let view = self
                    .publication
                    .federated_view(repository, graph_view, memory_view)
                    .map_err(|_| query_internal(request_id))?
                    .ok_or_else(|| query_not_found(request_id))?;
                RemoteQueryTarget::Federated(view)
            }
        };
        Ok(resolved)
    }

    fn load_target(
        &self,
        request_id: &RequestId,
        target: &RemoteQueryTarget,
        started: Instant,
        duration: Duration,
    ) -> Result<LoadedTarget, RemoteError> {
        let ensure_time = || {
            (started.elapsed() < duration)
                .then_some(())
                .ok_or_else(|| query_budget(request_id))
        };
        ensure_time()?;
        let loaded = match target {
            RemoteQueryTarget::Repository(snapshot) => LoadedTarget {
                graph: Some(
                    self.publication
                        .published_graph_snapshot_bounded(snapshot, started, duration)
                        .map_err(|error| query_store_error(request_id, error))?
                        .ok_or_else(|| query_not_found(request_id))?,
                ),
                memory: None,
            },
            RemoteQueryTarget::Memory(revision) => LoadedTarget {
                graph: None,
                memory: Some(
                    self.publication
                        .published_memory_revision_bounded(revision, started, duration)
                        .map_err(|error| query_store_error(request_id, error))?
                        .ok_or_else(|| query_not_found(request_id))?,
                ),
            },
            RemoteQueryTarget::Federated(view) => LoadedTarget {
                graph: Some(
                    self.publication
                        .published_graph_snapshot_bounded(&view.graph, started, duration)
                        .map_err(|error| query_store_error(request_id, error))?
                        .ok_or_else(|| query_not_found(request_id))?,
                ),
                memory: Some(
                    self.publication
                        .published_memory_revision_bounded(&view.memory, started, duration)
                        .map_err(|error| query_store_error(request_id, error))?
                        .ok_or_else(|| query_not_found(request_id))?,
                ),
            },
            RemoteQueryTarget::RepositoryView { .. }
            | RemoteQueryTarget::MemoryView { .. }
            | RemoteQueryTarget::FederatedView { .. } => return Err(query_internal(request_id)),
        };
        ensure_time()?;
        Ok(loaded)
    }

    fn execute(
        &self,
        request: &RemoteQueryRequest<RemoteQueryBody>,
        resolved_target: &RemoteQueryTarget,
        loaded: LoadedTarget,
        started: Instant,
    ) -> Result<RemoteQueryResult, RemoteError> {
        let budget = self.limits.clamp(request.body.budget);
        let duration = Duration::from_millis(budget.max_duration_ms.get());
        let fingerprint = query_fingerprint(
            &request.request_id,
            resolved_target,
            &request.body.operation,
            budget.max_depth.get(),
        )?;
        let diagnostics = bounded_diagnostics(&loaded, budget.max_diagnostics.get());
        let (data, mut page) = match &request.body.operation {
            RemoteQueryOperation::Status => (
                RemoteQueryData::Status(Box::new(RemoteStatusData {
                    graph: loaded
                        .graph
                        .as_ref()
                        .map(|snapshot| snapshot.record.clone()),
                    memory: loaded
                        .memory
                        .as_ref()
                        .map(|revision| revision.record.clone()),
                })),
                empty_page(),
            ),
            RemoteQueryOperation::Search {
                text,
                graph_kinds,
                graph_paths,
                memory_kinds,
            } => {
                let candidates =
                    search_candidates(&loaded, text, graph_kinds, graph_paths, memory_kinds);
                let (items, page) = paginate(
                    candidates,
                    request.body.page.cursor.as_ref(),
                    &fingerprint,
                    budget,
                    started,
                    0,
                    &request.request_id,
                )?;
                (RemoteQueryData::Search(RemoteSearchData { items }), page)
            }
            RemoteQueryOperation::Neighborhood {
                roots,
                direction,
                edge_kinds,
            } => {
                let graph = loaded
                    .graph
                    .as_ref()
                    .ok_or_else(|| query_invalid(&request.request_id))?;
                let (units, depth) = graph_neighborhood(
                    graph,
                    roots,
                    *direction,
                    edge_kinds,
                    budget.max_depth.get(),
                    started,
                    duration,
                );
                let (units, page) = paginate(
                    units,
                    request.body.page.cursor.as_ref(),
                    &fingerprint,
                    budget,
                    started,
                    depth,
                    &request.request_id,
                )?;
                let mut data = RemoteNeighborhoodData {
                    nodes: Vec::new(),
                    edges: Vec::new(),
                };
                for unit in units {
                    match unit {
                        GraphUnit::Node(node) => data.nodes.push(node),
                        GraphUnit::Edge(edge) => data.edges.push(edge),
                    }
                }
                (RemoteQueryData::Neighborhood(data), page)
            }
            RemoteQueryOperation::Context {
                seeds,
                direction,
                graph_edge_kinds,
                memory_relationship_kinds,
                include_unresolved,
                include_external,
                include_snippets,
            } => {
                validate_context_seeds(&request.request_id, resolved_target, seeds)?;
                let (units, depth) = context_units(
                    &loaded,
                    seeds,
                    *direction,
                    graph_edge_kinds,
                    memory_relationship_kinds,
                    *include_unresolved,
                    *include_external,
                    budget.max_depth.get(),
                    started,
                    duration,
                );
                let (units, page) = paginate(
                    units,
                    request.body.page.cursor.as_ref(),
                    &fingerprint,
                    budget,
                    started,
                    depth,
                    &request.request_id,
                )?;
                let mut data = RemoteContextData {
                    graph_nodes: Vec::new(),
                    graph_edges: Vec::new(),
                    memory_entities: Vec::new(),
                    memory_relationships: Vec::new(),
                    snippets: Vec::new(),
                };
                for unit in units {
                    match unit {
                        ContextUnit::GraphNode(node) => data.graph_nodes.push(node),
                        ContextUnit::GraphEdge(edge) => data.graph_edges.push(edge),
                        ContextUnit::MemoryEntity(entity) => data.memory_entities.push(entity),
                        ContextUnit::MemoryRelationship(relationship) => {
                            data.memory_relationships.push(relationship);
                        }
                    }
                }
                if *include_snippets {
                    data.snippets = self.snippets(
                        &request.request_id,
                        &loaded,
                        &data,
                        budget.max_snippet_bytes.get(),
                        started,
                        duration,
                    )?;
                }
                (RemoteQueryData::Context(Box::new(data)), page)
            }
        };
        page.returned_bytes =
            encoded_len(&(&diagnostics, &data)).map_err(|_| query_internal(&request.request_id))?;
        let result = RemoteQueryResult {
            page,
            diagnostics,
            data,
        };
        let encoded = encoded_len(&result).map_err(|_| query_internal(&request.request_id))?;
        if encoded > budget.max_bytes.get() || started.elapsed() >= duration {
            return Err(query_budget(&request.request_id));
        }
        Ok(result)
    }

    fn snippets(
        &self,
        request_id: &RequestId,
        loaded: &LoadedTarget,
        context: &RemoteContextData,
        max_bytes: u64,
        started: Instant,
        duration: Duration,
    ) -> Result<Vec<RemoteVerifiedSnippet>, RemoteError> {
        let mut remaining = max_bytes;
        let mut snippets = Vec::new();
        if let Some(snapshot) = &loaded.graph {
            let manifest = self.repository_manifest(request_id, snapshot)?;
            let mut seen = BTreeSet::new();
            for node in &context.graph_nodes {
                if started.elapsed() >= duration || remaining == 0 {
                    break;
                }
                let Some(evidence) = node.provenance.evidence.as_ref() else {
                    continue;
                };
                let key =
                    serde_json::to_string(evidence).map_err(|_| query_internal(request_id))?;
                if !seen.insert(key) {
                    continue;
                }
                let Some(descriptor) = manifest.body.files.iter().find(|file| {
                    file.path == evidence.path && file.content_identity == evidence.content_identity
                }) else {
                    continue;
                };
                let bytes = self
                    .objects
                    .read_verified(&descriptor.object)
                    .map_err(|error| query_object_error(request_id, error))?;
                let Some(slice) = evidence_slice(&bytes, evidence.span.as_ref()) else {
                    continue;
                };
                let Some((text, truncated, used)) = bounded_utf8(slice, remaining) else {
                    continue;
                };
                remaining = remaining.saturating_sub(used);
                snippets.push(RemoteVerifiedSnippet::Repository {
                    path: evidence.path.clone(),
                    span: evidence.span.clone(),
                    verified_content_identity: evidence.content_identity.clone(),
                    text,
                    truncated,
                });
            }
        }
        if let Some(revision) = &loaded.memory {
            let manifest = self.memory_manifest(request_id, revision)?;
            for entity in &context.memory_entities {
                if started.elapsed() >= duration || remaining == 0 {
                    break;
                }
                let Some(descriptor) = manifest.body.sources.iter().find(|source| {
                    source.category == entity.provenance.source_category
                        && source.locator == entity.provenance.source_locator
                        && source.source_fingerprint == entity.provenance.source_fingerprint
                        && matches!(
                            source.content_access,
                            MemoryContentAccess::StructureOnly
                                | MemoryContentAccess::CuratedSections
                        )
                }) else {
                    continue;
                };
                let bytes = self
                    .objects
                    .read_verified(&descriptor.object)
                    .map_err(|error| query_object_error(request_id, error))?;
                let Some((text, truncated, used)) = bounded_utf8(&bytes, remaining) else {
                    continue;
                };
                remaining = remaining.saturating_sub(used);
                snippets.push(RemoteVerifiedSnippet::Memory {
                    entity_id: entity.id.clone(),
                    source_locator: entity.provenance.source_locator.clone(),
                    verified_fingerprint: entity.provenance.source_fingerprint.clone(),
                    text,
                    truncated,
                });
            }
        }
        Ok(snippets)
    }

    fn repository_manifest(
        &self,
        request_id: &RequestId,
        snapshot: &StoredRemoteGraphSnapshot,
    ) -> Result<RepositorySourceManifest, RemoteError> {
        let record = self.job(request_id, &snapshot.record.job)?;
        let IndexInputRef::Repository(reference) = record.spec.input else {
            return Err(query_internal(request_id));
        };
        let bytes = self
            .objects
            .read_verified(&reference.manifest_object)
            .map_err(|error| query_object_error(request_id, error))?;
        let body: RepositorySourceManifestBody =
            serde_json::from_slice(&bytes).map_err(|_| query_internal(request_id))?;
        let manifest = RepositorySourceManifest { reference, body };
        manifest
            .validate::<(), ()>()
            .map_err(|_| query_internal(request_id))?;
        Ok(manifest)
    }

    fn memory_manifest(
        &self,
        request_id: &RequestId,
        revision: &StoredRemoteMemoryRevision,
    ) -> Result<MemorySourceManifest, RemoteError> {
        let record = self.job(request_id, &revision.record.job)?;
        let IndexInputRef::Memory(reference) = record.spec.input else {
            return Err(query_internal(request_id));
        };
        let bytes = self
            .objects
            .read_verified(&reference.manifest_object)
            .map_err(|error| query_object_error(request_id, error))?;
        let body: MemorySourceManifestBody =
            serde_json::from_slice(&bytes).map_err(|_| query_internal(request_id))?;
        let manifest = MemorySourceManifest { reference, body };
        manifest
            .validate::<(), ()>()
            .map_err(|_| query_internal(request_id))?;
        Ok(manifest)
    }

    fn job(
        &self,
        request_id: &RequestId,
        job: &super::protocol::IndexJobRef,
    ) -> Result<IndexJobRecord, RemoteError> {
        self.coordinator
            .inspect(&InspectIndexJobRequest {
                protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                request_id: RequestId::new("query-manifest").expect("static request id is valid"),
                job: job.clone(),
            })
            .map_err(|_| query_internal(request_id))?
            .ok_or_else(|| query_internal(request_id))
    }
}

impl RemoteSnapshotQueryApi for SqliteRemoteQueryApi {
    fn query(
        &self,
        authorization: &AuthorizationContext,
        request: &RemoteQueryRequest<RemoteQueryBody>,
    ) -> Result<RemoteQueryResponse<RemoteQueryResult>, RemoteError> {
        authorize_query(authorization, request)?;
        request.validate().map_err(|error| {
            protocol_error(
                &request.request_id,
                DISTRIBUTED_QUERY_PROTOCOL_VERSION,
                error,
            )
        })?;
        if !request.body.validate() {
            return Err(query_invalid(&request.request_id));
        }
        let started = Instant::now();
        let budget = self.limits.clamp(request.body.budget);
        let resolved_target = self.resolve_target(&request.request_id, &request.target)?;
        validate_operation_target(
            &request.request_id,
            &resolved_target,
            &request.body.operation,
        )?;
        let loaded = self.load_target(
            &request.request_id,
            &resolved_target,
            started,
            Duration::from_millis(budget.max_duration_ms.get()),
        )?;
        let body = self.execute(request, &resolved_target, loaded, started)?;
        Ok(RemoteQueryResponse {
            protocol_version: DISTRIBUTED_QUERY_PROTOCOL_VERSION,
            request_id: request.request_id.clone(),
            project: request.project.clone(),
            resolved_target,
            body,
        })
    }
}

mod support;
use support::*;

#[cfg(test)]
#[path = "api_sqlite_tests.rs"]
mod tests;
