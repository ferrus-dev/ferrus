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
    object_store::{EncryptedFilesystemObjectStore, TenantObjectStore},
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

pub struct SqliteRemoteControlApi {
    coordinator: SqliteIndexJobCoordinator,
}

impl SqliteRemoteControlApi {
    pub fn new(coordinator: SqliteIndexJobCoordinator) -> Self {
        Self { coordinator }
    }

    pub fn into_inner(self) -> SqliteIndexJobCoordinator {
        self.coordinator
    }
}

impl RemoteControlApi for SqliteRemoteControlApi {
    fn submit_build(
        &mut self,
        authorization: &AuthorizationContext,
        request: &SubmitIndexJobRequest,
        now: DateTime<Utc>,
    ) -> Result<RemoteControlResponse<IndexJobRecord>, RemoteError> {
        authorize(
            authorization,
            RemotePermission::SubmitBuild,
            AuthorizationScope::Project(request.project.clone()),
            &request.request_id,
            DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
        )?;
        request.validate().map_err(|error| {
            protocol_error(
                &request.request_id,
                DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                error,
            )
        })?;
        let record = self
            .coordinator
            .submit(request, now)
            .map_err(|error| control_backend_error(&request.request_id, error))?;
        Ok(control_response(
            request.request_id.clone(),
            request.project.clone(),
            record,
        ))
    }

    fn inspect_build(
        &self,
        authorization: &AuthorizationContext,
        request: &InspectIndexJobRequest,
    ) -> Result<RemoteControlResponse<IndexJobRecord>, RemoteError> {
        authorize(
            authorization,
            RemotePermission::InspectJob,
            AuthorizationScope::Project(request.job.project.clone()),
            &request.request_id,
            DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
        )?;
        request.validate().map_err(|error| {
            protocol_error(
                &request.request_id,
                DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                error,
            )
        })?;
        let record = self
            .coordinator
            .inspect(request)
            .map_err(|error| control_backend_error(&request.request_id, error))?
            .ok_or_else(|| remote_error(&request.request_id, RemoteErrorCode::NotFound, false))?;
        Ok(control_response(
            request.request_id.clone(),
            request.job.project.clone(),
            record,
        ))
    }

    fn cancel_build(
        &mut self,
        authorization: &AuthorizationContext,
        request: &CancelIndexJobRequest,
        now: DateTime<Utc>,
    ) -> Result<RemoteControlResponse<IndexJobRecord>, RemoteError> {
        authorize(
            authorization,
            RemotePermission::CancelJob,
            AuthorizationScope::Project(request.job.project.clone()),
            &request.request_id,
            DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
        )?;
        request.validate().map_err(|error| {
            protocol_error(
                &request.request_id,
                DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                error,
            )
        })?;
        let record = self
            .coordinator
            .cancel(request, now)
            .map_err(|error| control_backend_error(&request.request_id, error))?;
        Ok(control_response(
            request.request_id.clone(),
            request.job.project.clone(),
            record,
        ))
    }
}

fn control_response<T>(
    request_id: RequestId,
    project: RemoteProjectRef,
    body: T,
) -> RemoteControlResponse<T> {
    RemoteControlResponse {
        protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
        request_id,
        project,
        body,
    }
}

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
                        .graph_snapshot_bounded(snapshot, started, duration)
                        .map_err(|error| query_store_error(request_id, error))?
                        .ok_or_else(|| query_not_found(request_id))?,
                ),
                memory: None,
            },
            RemoteQueryTarget::Memory(revision) => LoadedTarget {
                graph: None,
                memory: Some(
                    self.publication
                        .memory_revision_bounded(revision, started, duration)
                        .map_err(|error| query_store_error(request_id, error))?
                        .ok_or_else(|| query_not_found(request_id))?,
                ),
            },
            RemoteQueryTarget::Federated(view) => LoadedTarget {
                graph: Some(
                    self.publication
                        .graph_snapshot_bounded(&view.graph, started, duration)
                        .map_err(|error| query_store_error(request_id, error))?
                        .ok_or_else(|| query_not_found(request_id))?,
                ),
                memory: Some(
                    self.publication
                        .memory_revision_bounded(&view.memory, started, duration)
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
                let bytes = match self.objects.read_verified(&descriptor.object) {
                    Ok(bytes) => bytes,
                    Err(_) => continue,
                };
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
                let bytes = match self.objects.read_verified(&descriptor.object) {
                    Ok(bytes) => bytes,
                    Err(_) => continue,
                };
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
            .map_err(|_| query_internal(request_id))?;
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
            .map_err(|_| query_internal(request_id))?;
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

struct LoadedTarget {
    graph: Option<StoredRemoteGraphSnapshot>,
    memory: Option<StoredRemoteMemoryRevision>,
}

#[derive(Clone, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
enum GraphUnit {
    Node(GraphNode),
    Edge(crate::repository_graph::domain::GraphEdge),
}

#[derive(Clone, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
enum ContextUnit {
    GraphNode(GraphNode),
    GraphEdge(crate::repository_graph::domain::GraphEdge),
    MemoryEntity(MemoryEntity),
    MemoryRelationship(MemoryRelationship),
}

fn authorize_query(
    authorization: &AuthorizationContext,
    request: &RemoteQueryRequest<RemoteQueryBody>,
) -> Result<(), RemoteError> {
    let request_id = &request.request_id;
    let authorize_one = |permission, scope| {
        authorize(
            authorization,
            permission,
            scope,
            request_id,
            DISTRIBUTED_QUERY_PROTOCOL_VERSION,
        )
    };
    match &request.target {
        RemoteQueryTarget::Repository(snapshot) => authorize_one(
            RemotePermission::QueryGraph,
            AuthorizationScope::Repository(snapshot.repository.clone()),
        )?,
        RemoteQueryTarget::Memory(revision) => authorize_one(
            RemotePermission::QueryMemory,
            AuthorizationScope::Project(revision.project.clone()),
        )?,
        RemoteQueryTarget::Federated(view) => {
            authorize_one(
                RemotePermission::QueryGraph,
                AuthorizationScope::Repository(view.graph.repository.clone()),
            )?;
            authorize_one(
                RemotePermission::QueryMemory,
                AuthorizationScope::Project(view.memory.project.clone()),
            )?;
        }
        RemoteQueryTarget::RepositoryView { repository, .. } => authorize_one(
            RemotePermission::QueryGraph,
            AuthorizationScope::Repository(repository.clone()),
        )?,
        RemoteQueryTarget::MemoryView { project, .. } => authorize_one(
            RemotePermission::QueryMemory,
            AuthorizationScope::Project(project.clone()),
        )?,
        RemoteQueryTarget::FederatedView { repository, .. } => {
            authorize_one(
                RemotePermission::QueryGraph,
                AuthorizationScope::Repository(repository.clone()),
            )?;
            authorize_one(
                RemotePermission::QueryMemory,
                AuthorizationScope::Project(repository.project.clone()),
            )?;
        }
    }
    if request.body.includes_snippets() {
        match &request.target {
            RemoteQueryTarget::Repository(snapshot) => authorize_one(
                RemotePermission::ReadVerifiedContent,
                AuthorizationScope::Repository(snapshot.repository.clone()),
            )?,
            RemoteQueryTarget::RepositoryView { repository, .. } => authorize_one(
                RemotePermission::ReadVerifiedContent,
                AuthorizationScope::Repository(repository.clone()),
            )?,
            RemoteQueryTarget::Memory(revision) => authorize_one(
                RemotePermission::ReadVerifiedContent,
                AuthorizationScope::Project(revision.project.clone()),
            )?,
            RemoteQueryTarget::MemoryView { project, .. } => authorize_one(
                RemotePermission::ReadVerifiedContent,
                AuthorizationScope::Project(project.clone()),
            )?,
            RemoteQueryTarget::Federated(view) => {
                authorize_one(
                    RemotePermission::ReadVerifiedContent,
                    AuthorizationScope::Repository(view.graph.repository.clone()),
                )?;
                authorize_one(
                    RemotePermission::ReadVerifiedContent,
                    AuthorizationScope::Project(view.memory.project.clone()),
                )?;
            }
            RemoteQueryTarget::FederatedView { repository, .. } => {
                authorize_one(
                    RemotePermission::ReadVerifiedContent,
                    AuthorizationScope::Repository(repository.clone()),
                )?;
                authorize_one(
                    RemotePermission::ReadVerifiedContent,
                    AuthorizationScope::Project(repository.project.clone()),
                )?;
            }
        }
    }
    Ok(())
}

fn authorize(
    authorization: &AuthorizationContext,
    permission: RemotePermission,
    scope: AuthorizationScope,
    request_id: &RequestId,
    protocol_version: u32,
) -> Result<(), RemoteError> {
    authorization
        .authorize(permission, &scope)
        .map_err(|_| RemoteError {
            protocol_version,
            request_id: request_id.clone(),
            code: RemoteErrorCode::Unauthorized,
            retryable: false,
        })
}

fn validate_operation_target(
    request_id: &RequestId,
    target: &RemoteQueryTarget,
    operation: &RemoteQueryOperation,
) -> Result<(), RemoteError> {
    if matches!(operation, RemoteQueryOperation::Neighborhood { .. })
        && matches!(target, RemoteQueryTarget::Memory(_))
    {
        return Err(query_invalid(request_id));
    }
    Ok(())
}

fn validate_context_seeds(
    request_id: &RequestId,
    target: &RemoteQueryTarget,
    seeds: &[RemoteContextSeed],
) -> Result<(), RemoteError> {
    let has_graph = !matches!(target, RemoteQueryTarget::Memory(_));
    let has_memory = !matches!(target, RemoteQueryTarget::Repository(_));
    if seeds.iter().any(|seed| match seed {
        RemoteContextSeed::GraphNode(_)
        | RemoteContextSeed::GraphSymbol(_)
        | RemoteContextSeed::GraphPath(_) => !has_graph,
        RemoteContextSeed::MemoryEntity(_) => !has_memory,
    }) {
        return Err(query_invalid(request_id));
    }
    Ok(())
}

fn search_candidates(
    loaded: &LoadedTarget,
    text: &str,
    graph_kinds: &[String],
    graph_paths: &[crate::repository_graph::domain::RepoPath],
    memory_kinds: &[crate::project_memory::domain::MemoryEntityKind],
) -> Vec<RemoteSearchItem> {
    let needle = text.to_lowercase();
    let mut scored = Vec::<(u32, String, RemoteSearchItem)>::new();
    if let Some(graph) = &loaded.graph {
        for node in &graph.nodes {
            if !graph_kinds.is_empty() && !graph_kinds.iter().any(|kind| kind == &node.kind) {
                continue;
            }
            let path = node
                .provenance
                .evidence
                .as_ref()
                .map(|evidence| &evidence.path);
            if !graph_paths.is_empty()
                && path.is_none_or(|path| {
                    !graph_paths.iter().any(|filter| {
                        path == filter
                            || path
                                .as_str()
                                .strip_prefix(filter.as_str())
                                .is_some_and(|suffix| suffix.starts_with('/'))
                    })
                })
            {
                continue;
            }
            if let Some((rank, match_kind)) = graph_match(node, &needle) {
                scored.push((
                    rank,
                    node.id.to_string(),
                    RemoteSearchItem::Repository {
                        node: node.clone(),
                        match_kind,
                        score: f64::from(rank),
                    },
                ));
            }
        }
    }
    if let Some(memory) = &loaded.memory {
        for entity in &memory.entities {
            if !memory_kinds.is_empty() && !memory_kinds.contains(&entity.data.kind()) {
                continue;
            }
            if let Some((rank, match_kind)) = memory_match(entity, &needle) {
                scored.push((
                    rank,
                    entity.id.to_string(),
                    RemoteSearchItem::Memory {
                        entity: entity.clone(),
                        match_kind,
                        score: f64::from(rank),
                    },
                ));
            }
        }
    }
    scored.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    scored.into_iter().map(|(_, _, item)| item).collect()
}

fn graph_match(node: &GraphNode, needle: &str) -> Option<(u32, RemoteSearchMatchKind)> {
    let id = node.id.to_string().to_lowercase();
    if id == needle {
        return Some((100, RemoteSearchMatchKind::ExactId));
    }
    if node
        .semantic_key
        .as_ref()
        .is_some_and(|key| key.to_string().to_lowercase() == needle)
    {
        return Some((95, RemoteSearchMatchKind::ExactSemanticKey));
    }
    if node
        .provenance
        .evidence
        .as_ref()
        .is_some_and(|evidence| evidence.path.as_str().to_lowercase() == needle)
    {
        return Some((90, RemoteSearchMatchKind::ExactPath));
    }
    let values = graph_search_values(node);
    if values.iter().any(|value| value.starts_with(needle)) {
        Some((70, RemoteSearchMatchKind::Prefix))
    } else if values.iter().any(|value| value.contains(needle)) {
        Some((50, RemoteSearchMatchKind::Contains))
    } else {
        None
    }
}

fn graph_search_values(node: &GraphNode) -> Vec<String> {
    let mut values = vec![node.id.to_string().to_lowercase(), node.kind.to_lowercase()];
    if let Some(key) = &node.semantic_key {
        values.push(key.to_string().to_lowercase());
    }
    if let Some(evidence) = &node.provenance.evidence {
        values.push(evidence.path.as_str().to_lowercase());
    }
    for value in node.properties.values() {
        match value {
            GraphValue::String(value) => values.push(value.to_lowercase()),
            GraphValue::StringList(items) => {
                values.extend(items.iter().map(|item| item.to_lowercase()));
            }
            GraphValue::Boolean(_) | GraphValue::Integer(_) | GraphValue::Float(_) => {}
        }
    }
    values
}

fn memory_match(entity: &MemoryEntity, needle: &str) -> Option<(u32, RemoteSearchMatchKind)> {
    if entity.id.to_string().to_lowercase() == needle {
        return Some((100, RemoteSearchMatchKind::ExactId));
    }
    if memory_title(&entity.data).is_some_and(|title| title.to_lowercase() == needle) {
        return Some((90, RemoteSearchMatchKind::ExactTitle));
    }
    let encoded = serde_json::to_string(&entity.data).ok()?.to_lowercase();
    if memory_title(&entity.data).is_some_and(|title| title.to_lowercase().starts_with(needle)) {
        Some((70, RemoteSearchMatchKind::Prefix))
    } else if encoded.contains(needle) {
        Some((50, RemoteSearchMatchKind::Contains))
    } else {
        None
    }
}

fn memory_title(data: &MemoryEntityData) -> Option<&str> {
    match data {
        MemoryEntityData::Specification { title } | MemoryEntityData::Milestone { title, .. } => {
            Some(title.as_str())
        }
        _ => None,
    }
}

fn graph_neighborhood(
    graph: &StoredRemoteGraphSnapshot,
    roots: &[NodeId],
    direction: EdgeDirection,
    edge_kinds: &[String],
    max_depth: u32,
    started: Instant,
    duration: Duration,
) -> (Vec<GraphUnit>, u32) {
    let nodes = graph
        .nodes
        .iter()
        .map(|node| (node.id.clone(), node))
        .collect::<BTreeMap<_, _>>();
    let mut selected_nodes = BTreeSet::new();
    let mut selected_edges = BTreeSet::new();
    let mut frontier = roots.iter().cloned().collect::<BTreeSet<_>>();
    selected_nodes.extend(
        frontier
            .iter()
            .filter(|id| nodes.contains_key(*id))
            .cloned(),
    );
    let mut explored = 0;
    while !frontier.is_empty() && explored < max_depth && started.elapsed() < duration {
        let mut next = BTreeSet::new();
        for edge in &graph.edges {
            if !edge_kinds.is_empty() && !edge_kinds.iter().any(|kind| kind == &edge.kind) {
                continue;
            }
            let target = match &edge.target {
                EdgeTarget::Node(target) => Some(target),
                EdgeTarget::External(_) | EdgeTarget::Unresolved(_) => None,
            };
            let outgoing = frontier.contains(&edge.source)
                && matches!(direction, EdgeDirection::Outgoing | EdgeDirection::Both);
            let incoming = target.is_some_and(|target| frontier.contains(target))
                && matches!(direction, EdgeDirection::Incoming | EdgeDirection::Both);
            if outgoing || incoming {
                selected_edges.insert(edge.id.clone());
                if outgoing && let Some(target) = target {
                    next.insert(target.clone());
                }
                if incoming {
                    next.insert(edge.source.clone());
                }
            }
        }
        next.retain(|id| !selected_nodes.contains(id));
        selected_nodes.extend(next.iter().filter(|id| nodes.contains_key(*id)).cloned());
        frontier = next;
        explored += 1;
    }
    let mut units = selected_nodes
        .into_iter()
        .filter_map(|id| nodes.get(&id).map(|node| GraphUnit::Node((*node).clone())))
        .collect::<Vec<_>>();
    units.extend(
        graph
            .edges
            .iter()
            .filter(|edge| selected_edges.contains(&edge.id))
            .cloned()
            .map(GraphUnit::Edge),
    );
    (units, explored)
}

#[allow(clippy::too_many_arguments)]
fn context_units(
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
) -> (Vec<ContextUnit>, u32) {
    let mut units = Vec::new();
    let mut explored = 0;
    if let Some(graph) = &loaded.graph {
        let roots = graph_seed_ids(graph, seeds);
        let filtered = StoredRemoteGraphSnapshot {
            record: graph.record.clone(),
            nodes: graph.nodes.clone(),
            edges: graph
                .edges
                .iter()
                .filter(|edge| match &edge.target {
                    EdgeTarget::Node(_) => true,
                    EdgeTarget::External(_) => include_external,
                    EdgeTarget::Unresolved(_) => include_unresolved,
                })
                .cloned()
                .collect(),
            diagnostics: Vec::new(),
        };
        let (graph_units, depth) = graph_neighborhood(
            &filtered,
            &roots,
            direction,
            graph_edge_kinds,
            max_depth,
            started,
            duration,
        );
        explored = explored.max(depth);
        units.extend(graph_units.into_iter().map(|unit| match unit {
            GraphUnit::Node(node) => ContextUnit::GraphNode(node),
            GraphUnit::Edge(edge) => ContextUnit::GraphEdge(edge),
        }));
    }
    if let Some(memory) = &loaded.memory {
        let (memory_units, depth) = memory_context_units(
            memory,
            seeds,
            memory_relationship_kinds,
            max_depth,
            started,
            duration,
        );
        explored = explored.max(depth);
        units.extend(memory_units);
    }
    (units, explored)
}

fn graph_seed_ids(graph: &StoredRemoteGraphSnapshot, seeds: &[RemoteContextSeed]) -> Vec<NodeId> {
    let mut roots = BTreeSet::new();
    for seed in seeds {
        match seed {
            RemoteContextSeed::GraphNode(id) => {
                roots.insert(id.clone());
            }
            RemoteContextSeed::GraphSymbol(key) => {
                roots.extend(
                    graph
                        .nodes
                        .iter()
                        .filter(|node| node.semantic_key.as_ref() == Some(key))
                        .map(|node| node.id.clone()),
                );
            }
            RemoteContextSeed::GraphPath(path) => {
                roots.extend(
                    graph
                        .nodes
                        .iter()
                        .filter(|node| {
                            node.provenance
                                .evidence
                                .as_ref()
                                .is_some_and(|evidence| &evidence.path == path)
                        })
                        .map(|node| node.id.clone()),
                );
            }
            RemoteContextSeed::MemoryEntity(_) => {}
        }
    }
    roots.into_iter().collect()
}

fn memory_context_units(
    memory: &StoredRemoteMemoryRevision,
    seeds: &[RemoteContextSeed],
    relationship_kinds: &[crate::project_memory::domain::MemoryRelationshipKind],
    max_depth: u32,
    started: Instant,
    duration: Duration,
) -> (Vec<ContextUnit>, u32) {
    let entities = memory
        .entities
        .iter()
        .map(|entity| (entity.id.clone(), entity))
        .collect::<BTreeMap<_, _>>();
    let mut selected = seeds
        .iter()
        .filter_map(|seed| match seed {
            RemoteContextSeed::MemoryEntity(id) if entities.contains_key(id) => Some(id.clone()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let mut frontier = selected.clone();
    let mut relationships = BTreeSet::new();
    let mut explored = 0;
    while !frontier.is_empty() && explored < max_depth && started.elapsed() < duration {
        let mut next = BTreeSet::new();
        for relationship in &memory.relationships {
            if !relationship_kinds.is_empty() && !relationship_kinds.contains(&relationship.kind) {
                continue;
            }
            let target = match &relationship.target {
                MemoryRelationshipTarget::MemoryEntity { entity_id } => Some(entity_id),
                _ => None,
            };
            if frontier.contains(&relationship.source)
                || target.is_some_and(|target| frontier.contains(target))
            {
                relationships.insert(relationship.id.clone());
                next.insert(relationship.source.clone());
                if let Some(target) = target {
                    next.insert(target.clone());
                }
            }
        }
        next.retain(|id| !selected.contains(id));
        selected.extend(next.iter().filter(|id| entities.contains_key(*id)).cloned());
        frontier = next;
        explored += 1;
    }
    let mut units = selected
        .into_iter()
        .filter_map(|id| {
            entities
                .get(&id)
                .map(|entity| ContextUnit::MemoryEntity((*entity).clone()))
        })
        .collect::<Vec<_>>();
    units.extend(
        memory
            .relationships
            .iter()
            .filter(|relationship| relationships.contains(&relationship.id))
            .cloned()
            .map(ContextUnit::MemoryRelationship),
    );
    (units, explored)
}

fn bounded_diagnostics(loaded: &LoadedTarget, max: u32) -> Vec<RemoteQueryDiagnostic> {
    loaded
        .graph
        .iter()
        .flat_map(|snapshot| {
            snapshot
                .diagnostics
                .iter()
                .cloned()
                .map(RemoteQueryDiagnostic::Repository)
        })
        .chain(loaded.memory.iter().flat_map(|revision| {
            revision
                .diagnostics
                .iter()
                .cloned()
                .map(RemoteQueryDiagnostic::Memory)
        }))
        .take(max as usize)
        .collect()
}

fn paginate<T: Clone + Serialize>(
    items: Vec<T>,
    cursor: Option<&RemotePageCursor>,
    fingerprint: &str,
    budget: super::api::RemoteQueryBudget,
    started: Instant,
    explored_depth: u32,
    request_id: &RequestId,
) -> Result<(Vec<T>, RemotePageInfo), RemoteError> {
    let offset = cursor_offset(cursor, fingerprint).map_err(|_| query_stale_cursor(request_id))?;
    if offset > items.len() {
        return Err(query_stale_cursor(request_id));
    }
    let duration = Duration::from_millis(budget.max_duration_ms.get());
    let mut returned = Vec::new();
    let mut bytes = 0u64;
    let mut index = offset;
    let mut truncation = None;
    while index < items.len() {
        if started.elapsed() >= duration {
            truncation = Some(RemoteTruncationReason::Duration);
            break;
        }
        if returned.len() >= budget.max_results.get() as usize {
            truncation = Some(RemoteTruncationReason::Results);
            break;
        }
        let item_bytes = encoded_len(&items[index]).map_err(|_| query_internal(request_id))?;
        if bytes.saturating_add(item_bytes) > budget.max_bytes.get() {
            if returned.is_empty() {
                return Err(query_budget(request_id));
            }
            truncation = Some(RemoteTruncationReason::Bytes);
            break;
        }
        bytes = bytes.saturating_add(item_bytes);
        returned.push(items[index].clone());
        index += 1;
    }
    let next_cursor = (index < items.len())
        .then(|| RemotePageCursor::new(format!("{fingerprint}.{index}")))
        .transpose()
        .map_err(|_| query_internal(request_id))?;
    let returned_results = u32::try_from(returned.len()).unwrap_or(u32::MAX);
    Ok((
        returned,
        RemotePageInfo {
            next_cursor,
            truncation,
            returned_results,
            returned_bytes: bytes,
            explored_depth,
        },
    ))
}

fn query_fingerprint(
    request_id: &RequestId,
    target: &RemoteQueryTarget,
    operation: &RemoteQueryOperation,
    max_depth: u32,
) -> Result<String, RemoteError> {
    let encoded = serde_json::to_vec(&(target, operation, max_depth))
        .map_err(|_| query_internal(request_id))?;
    let mut digest = Sha256::new();
    digest.update(b"ferrus.remote.query-cursor.v1\0");
    digest.update(encoded);
    Ok(digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn cursor_offset(cursor: Option<&RemotePageCursor>, fingerprint: &str) -> Result<usize, ()> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let (stored, offset) = cursor.as_str().rsplit_once('.').ok_or(())?;
    if stored != fingerprint {
        return Err(());
    }
    offset.parse().map_err(|_| ())
}

fn evidence_slice<'a>(
    bytes: &'a [u8],
    span: Option<&crate::repository_graph::domain::SourceSpan>,
) -> Option<&'a [u8]> {
    let Some(span) = span else {
        return Some(bytes);
    };
    let start = usize::try_from(span.start.byte_offset).ok()?;
    let end = usize::try_from(span.end.byte_offset).ok()?;
    (start <= end && end <= bytes.len()).then_some(&bytes[start..end])
}

fn bounded_utf8(bytes: &[u8], max_bytes: u64) -> Option<(String, bool, u64)> {
    let text = std::str::from_utf8(bytes).ok()?;
    let limit = usize::try_from(max_bytes)
        .unwrap_or(usize::MAX)
        .min(text.len());
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let truncated = end < text.len();
    Some((text[..end].to_string(), truncated, end as u64))
}

fn empty_page() -> RemotePageInfo {
    RemotePageInfo {
        next_cursor: None,
        truncation: None,
        returned_results: 0,
        returned_bytes: 0,
        explored_depth: 0,
    }
}

fn encoded_len(value: &impl Serialize) -> Result<u64, ()> {
    serde_json::to_vec(value)
        .ok()
        .and_then(|encoded| u64::try_from(encoded.len()).ok())
        .ok_or(())
}

fn protocol_error(
    request_id: &RequestId,
    version: u32,
    error: DistributedProtocolError,
) -> RemoteError {
    RemoteError {
        protocol_version: version,
        request_id: request_id.clone(),
        code: if error == DistributedProtocolError::UnsupportedVersion {
            RemoteErrorCode::UnsupportedVersion
        } else {
            RemoteErrorCode::InvalidRequest
        },
        retryable: false,
    }
}

fn control_backend_error(request_id: &RequestId, error: CoordinatorError) -> RemoteError {
    let (code, retryable) = match error {
        CoordinatorError::InvalidRequest => (RemoteErrorCode::InvalidRequest, false),
        CoordinatorError::NotFound => (RemoteErrorCode::NotFound, false),
        CoordinatorError::Conflict | CoordinatorError::LeaseLost => {
            (RemoteErrorCode::Conflict, false)
        }
        CoordinatorError::Cancelled => (RemoteErrorCode::Cancelled, false),
        CoordinatorError::IncompatibleSchema => (RemoteErrorCode::UnsupportedVersion, false),
        CoordinatorError::Database(_) => (RemoteErrorCode::TemporarilyUnavailable, true),
        CoordinatorError::Serialization => (RemoteErrorCode::Internal, false),
    };
    RemoteError {
        protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
        request_id: request_id.clone(),
        code,
        retryable,
    }
}

fn remote_error(request_id: &RequestId, code: RemoteErrorCode, retryable: bool) -> RemoteError {
    RemoteError {
        protocol_version: DISTRIBUTED_QUERY_PROTOCOL_VERSION,
        request_id: request_id.clone(),
        code,
        retryable,
    }
}

fn query_invalid(request_id: &RequestId) -> RemoteError {
    remote_error(request_id, RemoteErrorCode::InvalidRequest, false)
}

fn query_not_found(request_id: &RequestId) -> RemoteError {
    remote_error(request_id, RemoteErrorCode::NotFound, false)
}

fn query_stale_cursor(request_id: &RequestId) -> RemoteError {
    remote_error(request_id, RemoteErrorCode::StaleCursor, false)
}

fn query_budget(request_id: &RequestId) -> RemoteError {
    remote_error(request_id, RemoteErrorCode::BudgetExceeded, false)
}

fn query_internal(request_id: &RequestId) -> RemoteError {
    remote_error(request_id, RemoteErrorCode::Internal, false)
}

fn query_store_error(request_id: &RequestId, error: RemoteStoreError) -> RemoteError {
    match error {
        RemoteStoreError::ReadBudgetExceeded => query_budget(request_id),
        RemoteStoreError::Database(_) => {
            remote_error(request_id, RemoteErrorCode::TemporarilyUnavailable, true)
        }
        _ => query_internal(request_id),
    }
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroU64};

    use super::*;
    use crate::{
        distributed::{
            DISTRIBUTED_FACT_PROTOCOL_VERSION, DISTRIBUTED_SOURCE_MANIFEST_VERSION,
            api::{RemotePageRequest, RemoteQueryBudget},
            coordinator::{AdvanceIndexJobRequest, ClaimIndexJobRequest},
            coordinator_sqlite::CoordinatorLimits,
            identity::{
                CredentialId, FactShardId, FederatedViewRef, IndexJobId, MemoryManifestId,
                PrincipalId, RemoteProjectId, RemoteRepositoryId, RemoteRepositoryRef,
                RepositoryManifestId, TenantId,
            },
            object_store::{ObjectStoreProtection, ObjectStoreQuota},
            protocol::{
                FactBatch, FactBatchPayload, FactTarget, IndexJobKind, IndexJobSpec,
                IndexSemantics, SubmitIndexJobRequest,
            },
            publication::RemotePublicationStore,
            publication_sqlite::RemoteStoreLimits,
            security::{AuthorizationScope, CredentialClass},
            source::{
                MemorySourceObject, PackagingSummary, REMOTE_REPOSITORY_SOURCE_POLICY_VERSION,
                RepositoryFileRole, RepositorySourceObject,
            },
        },
        project_memory::{
            domain::{
                MemoryBuildId, MemoryConfidence, MemoryEntityData, MemoryEntityId,
                MemoryExtractorId, MemoryExtractorIdentity, MemoryIndexTimestamps,
                MemoryProvenance, MemoryResolutionState, MemoryRevisionId, MemorySourceCategory,
                MemorySourceLocator, MemoryStatusToken, MemoryText, ProjectId, ProjectNamespace,
                ProjectRef,
            },
            policy::{MEMORY_POLICY_SCHEMA_VERSION, MemoryContentAccess, MemorySourceSensitivity},
        },
        repository_graph::{
            domain::{
                BuildId, Confidence, Digest, EdgeId, EdgeTarget, ExtractorId, ExtractorIdentity,
                FactProvenance, GraphEdge, GraphNode, PublishedViewName, RepoPath, RepositoryId,
                RepositoryNamespace, RepositoryRef, ResolutionState, SemanticKey, SnapshotId,
                SourceEvidence, SourceKind, SourceRevision, SourceRevisionId,
            },
            ports::SourceFileMode,
        },
    };

    const KEY: [u8; 32] = [71; 32];

    struct Fixture {
        directory: tempfile::TempDir,
        control_path: std::path::PathBuf,
        object_root: std::path::PathBuf,
        query: SqliteRemoteQueryApi,
        project: RemoteProjectRef,
        repository: RemoteRepositoryRef,
        graph: RemoteGraphSnapshotRef,
        memory: RemoteMemoryRevisionRef,
        graph_node: NodeId,
        source_text: String,
    }

    fn digest(value: &str) -> Digest {
        Digest::new("sha256", value).unwrap()
    }

    fn sha256(bytes: &[u8]) -> Digest {
        let value = Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        Digest::new("sha256", value).unwrap()
    }

    fn project() -> RemoteProjectRef {
        RemoteProjectRef {
            tenant_id: TenantId::new("tenant-a").unwrap(),
            project_id: RemoteProjectId::new("project").unwrap(),
        }
    }

    fn repository() -> RemoteRepositoryRef {
        RemoteRepositoryRef {
            project: project(),
            repository_id: RemoteRepositoryId::new("repository").unwrap(),
        }
    }

    fn local_repository() -> RepositoryRef {
        RepositoryRef {
            namespace: RepositoryNamespace::new("remote:test").unwrap(),
            repository_id: RepositoryId::new("root").unwrap(),
        }
    }

    fn local_project() -> ProjectRef {
        ProjectRef {
            namespace: ProjectNamespace::new("remote:test").unwrap(),
            project_id: ProjectId::new("project").unwrap(),
        }
    }

    fn coordinator(path: &std::path::Path) -> SqliteIndexJobCoordinator {
        SqliteIndexJobCoordinator::open(
            path,
            CoordinatorLimits {
                max_attempts: NonZeroU32::new(3).unwrap(),
                lease_ttl_ms: NonZeroU64::new(60_000).unwrap(),
                max_job_duration_ms: NonZeroU64::new(120_000).unwrap(),
            },
        )
        .unwrap()
    }

    fn object_quota() -> ObjectStoreQuota {
        ObjectStoreQuota {
            max_objects_per_project: NonZeroU64::new(100).unwrap(),
            max_bytes_per_project: NonZeroU64::new(16 * 1024 * 1024).unwrap(),
            max_object_bytes: NonZeroU64::new(1024 * 1024).unwrap(),
        }
    }

    fn object_store(root: &std::path::Path) -> EncryptedFilesystemObjectStore {
        let store = EncryptedFilesystemObjectStore::open(root, KEY, object_quota(), true).unwrap();
        assert_eq!(
            store.protection(),
            ObjectStoreProtection {
                authenticated_transport: true,
                encrypted_at_rest: true,
            }
        );
        store
    }

    fn publication_limits() -> RemoteStoreLimits {
        RemoteStoreLimits {
            max_snapshots_per_project: NonZeroU64::new(100).unwrap(),
            max_facts_per_project: NonZeroU64::new(10_000).unwrap(),
            max_bytes_per_project: NonZeroU64::new(16 * 1024 * 1024).unwrap(),
            max_facts_per_snapshot: NonZeroU64::new(1_000).unwrap(),
            max_fact_bytes: NonZeroU64::new(1024 * 1024).unwrap(),
        }
    }

    fn query_limits() -> RemoteQueryLimits {
        RemoteQueryLimits {
            max_results: NonZeroU32::new(50).unwrap(),
            max_bytes: NonZeroU64::new(256 * 1024).unwrap(),
            max_depth: NonZeroU32::new(4).unwrap(),
            max_duration_ms: NonZeroU64::new(2_000).unwrap(),
            max_diagnostics: NonZeroU32::new(10).unwrap(),
            max_snippet_bytes: NonZeroU64::new(4_096).unwrap(),
        }
    }

    fn query_budget(max_results: u32) -> RemoteQueryBudget {
        RemoteQueryBudget {
            max_results: NonZeroU32::new(max_results).unwrap(),
            max_bytes: NonZeroU64::new(128 * 1024).unwrap(),
            max_depth: NonZeroU32::new(3).unwrap(),
            max_duration_ms: NonZeroU64::new(1_000).unwrap(),
            max_diagnostics: NonZeroU32::new(5).unwrap(),
            max_snippet_bytes: NonZeroU64::new(1_024).unwrap(),
        }
    }

    fn auth(class: CredentialClass, scope: AuthorizationScope) -> AuthorizationContext {
        AuthorizationContext::for_class(
            PrincipalId::new("principal").unwrap(),
            CredentialId::new("credential").unwrap(),
            class,
            scope,
        )
    }

    fn repository_manifest(
        objects: &mut EncryptedFilesystemObjectStore,
        source_text: &str,
    ) -> super::RepositorySourceManifest {
        let content_identity = sha256(source_text.as_bytes());
        let source_object = objects
            .put_verified(&project(), &content_identity, source_text.as_bytes())
            .unwrap()
            .object;
        let body = RepositorySourceManifestBody {
            protocol_version: DISTRIBUTED_SOURCE_MANIFEST_VERSION,
            repository: repository(),
            source_policy_digest: digest("11"),
            source_revision: SourceRevision {
                id: SourceRevisionId::new("source-revision").unwrap(),
                repository: local_repository(),
                source_kind: SourceKind::CommittedTree,
                base_revision: Some(digest("12")),
                manifest_digest: digest("13"),
                analysis_config_digest: digest("14"),
                dirty: false,
                includes_untracked: false,
            },
            extractor_set_digest: digest("44"),
            policy_schema_version: REMOTE_REPOSITORY_SOURCE_POLICY_VERSION,
            files: vec![RepositorySourceObject {
                path: RepoPath::new("src/lib.rs").unwrap(),
                content_identity: content_identity.clone(),
                byte_len: source_text.len() as u64,
                file_mode: SourceFileMode::Regular,
                file_role: RepositoryFileRole::Source,
                object: source_object,
            }],
            summary: PackagingSummary {
                included_objects: 1,
                total_bytes: source_text.len() as u64,
                source_diagnostic_codes: BTreeMap::new(),
            },
        };
        let encoded = serde_json::to_vec(&body).unwrap();
        let manifest_digest = sha256(&encoded);
        let manifest_object = objects
            .put_verified(&project(), &manifest_digest, &encoded)
            .unwrap()
            .object;
        let manifest = RepositorySourceManifest {
            reference: super::super::identity::RepositoryManifestRef {
                repository: repository(),
                manifest_id: RepositoryManifestId::new(manifest_digest.value()).unwrap(),
                manifest_digest,
                source_policy_digest: digest("11"),
                manifest_object,
            },
            body,
        };
        manifest.validate::<(), ()>().unwrap();
        manifest
    }

    fn memory_manifest(objects: &mut EncryptedFilesystemObjectStore) -> MemorySourceManifest {
        let content = b"approved memory outcome";
        let content_identity = sha256(content);
        let source_object = objects
            .put_verified(&project(), &content_identity, content)
            .unwrap()
            .object;
        let body = MemorySourceManifestBody {
            protocol_version: DISTRIBUTED_SOURCE_MANIFEST_VERSION,
            project: project(),
            memory_policy_digest: digest("21"),
            project_identity: local_project(),
            source_set_digest: digest("22"),
            extractor_set_digest: digest("44"),
            policy_schema_version: MEMORY_POLICY_SCHEMA_VERSION,
            sources: vec![MemorySourceObject {
                category: MemorySourceCategory::ApprovedOutcome,
                locator: MemorySourceLocator::TrackedFile {
                    path: RepoPath::new("docs/spec.md").unwrap(),
                },
                source_fingerprint: digest("23"),
                sanitized_byte_len: content.len() as u64,
                sensitivity: MemorySourceSensitivity::Curated,
                content_access: MemoryContentAccess::CuratedSections,
                object: source_object,
            }],
            summary: PackagingSummary {
                included_objects: 1,
                total_bytes: content.len() as u64,
                source_diagnostic_codes: BTreeMap::new(),
            },
        };
        let encoded = serde_json::to_vec(&body).unwrap();
        let manifest_digest = sha256(&encoded);
        let manifest_object = objects
            .put_verified(&project(), &manifest_digest, &encoded)
            .unwrap()
            .object;
        let manifest = MemorySourceManifest {
            reference: super::super::identity::MemoryManifestRef {
                project: project(),
                manifest_id: MemoryManifestId::new(manifest_digest.value()).unwrap(),
                manifest_digest,
                memory_policy_digest: digest("21"),
                manifest_object,
            },
            body,
        };
        manifest.validate::<(), ()>().unwrap();
        manifest
    }

    fn publishing_job(
        coordinator: &mut SqliteIndexJobCoordinator,
        input: IndexInputRef,
        kind: IndexJobKind,
        unique: &str,
    ) -> IndexJobRecord {
        let now = Utc::now();
        let spec = IndexJobSpec::new(
            kind,
            input,
            IndexSemantics {
                semantic_config_digest: digest("33"),
                model_version: NonZeroU32::new(1).unwrap(),
                extractor_set_digest: digest("44"),
            },
        )
        .unwrap();
        coordinator
            .submit(
                &SubmitIndexJobRequest {
                    protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                    request_id: RequestId::new(format!("submit-{unique}")).unwrap(),
                    project: project(),
                    job: spec,
                },
                now,
            )
            .unwrap();
        let leased = coordinator
            .claim(
                &ClaimIndexJobRequest {
                    protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                    request_id: RequestId::new(format!("claim-{unique}")).unwrap(),
                    project: project(),
                    kind,
                    worker_id: super::super::identity::WorkerId::new(format!("worker-{unique}"))
                        .unwrap(),
                },
                now,
            )
            .unwrap()
            .unwrap();
        let advance = |record: &IndexJobRecord, phase: &str| AdvanceIndexJobRequest {
            protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
            request_id: RequestId::new(format!("{phase}-{unique}")).unwrap(),
            job: record.job.clone(),
            worker_id: record.lease.as_ref().unwrap().worker_id.clone(),
            lease_generation: record.lease.as_ref().unwrap().generation,
        };
        let running = coordinator.start(&advance(&leased, "start"), now).unwrap();
        coordinator
            .begin_publication(&advance(&running, "publish"), now)
            .unwrap()
    }

    fn fixture() -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let control_path = directory.path().join("control.db");
        let object_root = directory.path().join("objects");
        let mut objects = object_store(&object_root);
        let repository_manifest = repository_manifest(&mut objects, "pub struct ImportantType;\n");
        let memory_manifest = memory_manifest(&mut objects);
        let mut coordinator_backend = coordinator(&control_path);
        let graph_job = publishing_job(
            &mut coordinator_backend,
            IndexInputRef::Repository(repository_manifest.reference.clone()),
            IndexJobKind::RepositoryGraph,
            "graph",
        );
        let snapshot_id = SnapshotId::new("snapshot-query").unwrap();
        let node_id = NodeId::new("node-important").unwrap();
        let child_id = NodeId::new("node-child").unwrap();
        let provenance = FactProvenance {
            extractor: ExtractorIdentity {
                id: ExtractorId::new("test.extractor").unwrap(),
                version: "1".to_string(),
                contract_version: 1,
            },
            evidence: Some(SourceEvidence {
                path: RepoPath::new("src/lib.rs").unwrap(),
                content_identity: repository_manifest.body.files[0].content_identity.clone(),
                span: None,
            }),
            resolution: ResolutionState::Resolved,
            confidence: Confidence::Exact,
        };
        let graph_batch = FactBatch::new(
            graph_job.job.clone(),
            FactTarget::RepositoryGraph {
                snapshot: RemoteGraphSnapshotRef {
                    repository: repository(),
                    snapshot_id: snapshot_id.clone(),
                },
                build_id: BuildId::new("build-graph").unwrap(),
            },
            FactShardId::new("graph-all").unwrap(),
            0,
            digest("44"),
            true,
            FactBatchPayload::RepositoryGraph {
                nodes: vec![
                    GraphNode {
                        snapshot_id: snapshot_id.clone(),
                        id: node_id.clone(),
                        kind: "struct".to_string(),
                        semantic_key: Some(SemanticKey::new("important-type").unwrap()),
                        provenance: provenance.clone(),
                        properties: BTreeMap::new(),
                    },
                    GraphNode {
                        snapshot_id: snapshot_id.clone(),
                        id: child_id.clone(),
                        kind: "field".to_string(),
                        semantic_key: Some(SemanticKey::new("important-type-child").unwrap()),
                        provenance: provenance.clone(),
                        properties: BTreeMap::new(),
                    },
                ],
                edges: vec![GraphEdge {
                    snapshot_id: snapshot_id.clone(),
                    id: EdgeId::new("edge-child").unwrap(),
                    kind: "contains".to_string(),
                    source: node_id.clone(),
                    target: EdgeTarget::Node(child_id),
                    provenance,
                    properties: BTreeMap::new(),
                }],
                diagnostics: Vec::new(),
            },
        )
        .unwrap();
        assert_eq!(
            graph_batch.header.protocol_version,
            DISTRIBUTED_FACT_PROTOCOL_VERSION
        );
        let mut publication =
            SqliteRemotePublicationStore::open(&control_path, KEY, publication_limits(), true)
                .unwrap();
        let graph_request = super::super::protocol::PublishGraphRequest {
            protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
            request_id: RequestId::new("publish-graph").unwrap(),
            job: graph_job.job.clone(),
            worker_id: graph_job.lease.as_ref().unwrap().worker_id.clone(),
            lease_generation: graph_job.lease.as_ref().unwrap().generation,
            repository: repository(),
            view_name: PublishedViewName::new("canonical").unwrap(),
            snapshot_id: snapshot_id.clone(),
            expected: None,
        };
        publication
            .publish_graph(&graph_request, &[graph_batch], Utc::now())
            .unwrap();

        let memory_job = publishing_job(
            &mut coordinator_backend,
            IndexInputRef::Memory(memory_manifest.reference.clone()),
            IndexJobKind::ProjectMemory,
            "memory",
        );
        let revision_id = MemoryRevisionId::new("memory-query").unwrap();
        let entity = MemoryEntity {
            project: local_project(),
            memory_revision_id: revision_id.clone(),
            id: MemoryEntityId::new("memory-important").unwrap(),
            data: MemoryEntityData::Outcome {
                text: MemoryText::new("Important approved outcome").unwrap(),
            },
            provenance: MemoryProvenance {
                source_category: MemorySourceCategory::ApprovedOutcome,
                source_locator: MemorySourceLocator::TrackedFile {
                    path: RepoPath::new("docs/spec.md").unwrap(),
                },
                source_fingerprint: digest("23"),
                extractor: MemoryExtractorIdentity::current(
                    MemoryExtractorId::new("memory.test").unwrap(),
                    MemoryStatusToken::new("v1").unwrap(),
                ),
                evidence: crate::project_memory::domain::MemoryEvidenceLocator::Record(
                    crate::project_memory::domain::MemoryRecordId::new("outcome").unwrap(),
                ),
                resolution: MemoryResolutionState::Resolved,
                confidence: MemoryConfidence::Exact,
                timestamps: MemoryIndexTimestamps {
                    source_observed_at: Utc::now(),
                    indexed_at: Utc::now(),
                },
            },
        };
        let memory_batch = FactBatch::new(
            memory_job.job.clone(),
            FactTarget::ProjectMemory {
                revision: RemoteMemoryRevisionRef {
                    project: project(),
                    revision_id: revision_id.clone(),
                },
                build_id: MemoryBuildId::new("build-memory").unwrap(),
            },
            FactShardId::new("memory-all").unwrap(),
            0,
            digest("44"),
            true,
            FactBatchPayload::ProjectMemory {
                entities: vec![entity],
                relationships: Vec::new(),
                diagnostics: Vec::new(),
            },
        )
        .unwrap();
        let memory_request = super::super::protocol::PublishMemoryRequest {
            protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
            request_id: RequestId::new("publish-memory").unwrap(),
            job: memory_job.job.clone(),
            worker_id: memory_job.lease.as_ref().unwrap().worker_id.clone(),
            lease_generation: memory_job.lease.as_ref().unwrap().generation,
            project: project(),
            view_name: crate::project_memory::domain::MemoryViewName::new("project").unwrap(),
            revision_id: revision_id.clone(),
            expected: None,
        };
        publication
            .publish_memory(&memory_request, &[memory_batch], Utc::now())
            .unwrap();

        let query = SqliteRemoteQueryApi::new(
            SqliteRemotePublicationStore::open(&control_path, KEY, publication_limits(), true)
                .unwrap(),
            coordinator(&control_path),
            object_store(&object_root),
            query_limits(),
        );
        Fixture {
            directory,
            control_path,
            object_root,
            query,
            project: project(),
            repository: repository(),
            graph: RemoteGraphSnapshotRef {
                repository: repository(),
                snapshot_id,
            },
            memory: RemoteMemoryRevisionRef {
                project: project(),
                revision_id,
            },
            graph_node: node_id,
            source_text: "pub struct ImportantType;\n".to_string(),
        }
    }

    fn query_request(
        fixture: &Fixture,
        target: RemoteQueryTarget,
        operation: RemoteQueryOperation,
        max_results: u32,
        cursor: Option<RemotePageCursor>,
    ) -> RemoteQueryRequest<RemoteQueryBody> {
        RemoteQueryRequest {
            protocol_version: DISTRIBUTED_QUERY_PROTOCOL_VERSION,
            request_id: RequestId::new("query").unwrap(),
            project: fixture.project.clone(),
            target,
            body: RemoteQueryBody {
                budget: query_budget(max_results),
                page: RemotePageRequest { cursor },
                operation,
            },
        }
    }

    #[test]
    fn query_agent_is_read_only_and_denied_before_control_lookup() {
        let fixture = fixture();
        let query_agent = auth(
            CredentialClass::QueryAgent,
            AuthorizationScope::Project(fixture.project.clone()),
        );
        let unknown = InspectIndexJobRequest {
            protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
            request_id: RequestId::new("inspect-unknown").unwrap(),
            job: super::super::protocol::IndexJobRef {
                project: fixture.project.clone(),
                job_id: IndexJobId::new("unknown").unwrap(),
                kind: IndexJobKind::RepositoryGraph,
            },
        };
        let control = SqliteRemoteControlApi::new(coordinator(&fixture.control_path));
        let error = control.inspect_build(&query_agent, &unknown).unwrap_err();
        assert_eq!(error.code, RemoteErrorCode::Unauthorized);

        let foreign = auth(
            CredentialClass::Coordinator,
            AuthorizationScope::Project(RemoteProjectRef {
                tenant_id: TenantId::new("tenant-b").unwrap(),
                project_id: RemoteProjectId::new("project").unwrap(),
            }),
        );
        let error = control.inspect_build(&foreign, &unknown).unwrap_err();
        assert_eq!(error.code, RemoteErrorCode::Unauthorized);
    }

    #[test]
    fn project_operator_can_submit_inspect_and_cancel_a_build() {
        let fixture = fixture();
        let mut objects = object_store(&fixture.object_root);
        let manifest = repository_manifest(&mut objects, "pub struct LaterType;\n");
        let spec = IndexJobSpec::new(
            IndexJobKind::RepositoryGraph,
            IndexInputRef::Repository(manifest.reference),
            IndexSemantics {
                semantic_config_digest: digest("91"),
                model_version: NonZeroU32::new(1).unwrap(),
                extractor_set_digest: digest("44"),
            },
        )
        .unwrap();
        let submit = SubmitIndexJobRequest {
            protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
            request_id: RequestId::new("control-submit").unwrap(),
            project: fixture.project.clone(),
            job: spec,
        };
        let operator = auth(
            CredentialClass::ProjectOperator,
            AuthorizationScope::Project(fixture.project.clone()),
        );
        let query_agent = auth(
            CredentialClass::QueryAgent,
            AuthorizationScope::Project(fixture.project.clone()),
        );
        let mut control = SqliteRemoteControlApi::new(coordinator(&fixture.control_path));
        assert_eq!(
            control
                .submit_build(&query_agent, &submit, Utc::now())
                .unwrap_err()
                .code,
            RemoteErrorCode::Unauthorized
        );
        let submitted = control
            .submit_build(&operator, &submit, Utc::now())
            .unwrap()
            .body;
        assert_eq!(
            submitted.state,
            super::super::protocol::IndexJobState::Queued
        );
        let inspected = control
            .inspect_build(
                &operator,
                &InspectIndexJobRequest {
                    protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                    request_id: RequestId::new("control-inspect").unwrap(),
                    job: submitted.job.clone(),
                },
            )
            .unwrap()
            .body;
        assert_eq!(inspected.job, submitted.job);
        let cancelled = control
            .cancel_build(
                &operator,
                &CancelIndexJobRequest {
                    protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                    request_id: RequestId::new("control-cancel").unwrap(),
                    job: submitted.job,
                    expected_state: Some(super::super::protocol::IndexJobState::Queued),
                },
                Utc::now(),
            )
            .unwrap()
            .body;
        assert_eq!(
            cancelled.state,
            super::super::protocol::IndexJobState::Cancelled
        );
    }

    #[test]
    fn latest_search_is_resolved_once_and_pagination_is_snapshot_bound() {
        let fixture = fixture();
        let authorization = auth(
            CredentialClass::QueryAgent,
            AuthorizationScope::Project(fixture.project.clone()),
        );
        let operation = RemoteQueryOperation::Search {
            text: "important".to_string(),
            graph_kinds: Vec::new(),
            graph_paths: Vec::new(),
            memory_kinds: Vec::new(),
        };
        let request = query_request(
            &fixture,
            RemoteQueryTarget::FederatedView {
                repository: fixture.repository.clone(),
                graph_view: PublishedViewName::new("canonical").unwrap(),
                memory_view: crate::project_memory::domain::MemoryViewName::new("project").unwrap(),
            },
            operation.clone(),
            1,
            None,
        );
        let first = fixture.query.query(&authorization, &request).unwrap();
        assert_eq!(
            first.resolved_target,
            RemoteQueryTarget::Federated(
                FederatedViewRef::new(fixture.graph.clone(), fixture.memory.clone()).unwrap()
            )
        );
        let RemoteQueryData::Search(data) = first.body.data else {
            panic!("search response expected");
        };
        assert_eq!(data.items.len(), 1);
        let cursor = first.body.page.next_cursor.unwrap();
        let changed_query = query_request(
            &fixture,
            first.resolved_target.clone(),
            RemoteQueryOperation::Search {
                text: "different".to_string(),
                graph_kinds: Vec::new(),
                graph_paths: Vec::new(),
                memory_kinds: Vec::new(),
            },
            1,
            Some(cursor.clone()),
        );
        assert_eq!(
            fixture
                .query
                .query(&authorization, &changed_query)
                .unwrap_err()
                .code,
            RemoteErrorCode::StaleCursor
        );
        let pinned = query_request(&fixture, first.resolved_target, operation, 1, Some(cursor));
        let second = fixture.query.query(&authorization, &pinned).unwrap();
        let RemoteQueryData::Search(data) = second.body.data else {
            panic!("search response expected");
        };
        assert!(!data.items.is_empty());
    }

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
        assert_eq!(text, "approved memory outcome");
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

    #[test]
    fn stale_cursor_and_tiny_response_budget_fail_without_reissuing_pages() {
        let fixture = fixture();
        let authorization = auth(
            CredentialClass::QueryAgent,
            AuthorizationScope::Project(fixture.project.clone()),
        );
        let mut request = query_request(
            &fixture,
            RemoteQueryTarget::Repository(fixture.graph.clone()),
            RemoteQueryOperation::Search {
                text: "important".to_string(),
                graph_kinds: Vec::new(),
                graph_paths: Vec::new(),
                memory_kinds: Vec::new(),
            },
            1,
            Some(RemotePageCursor::new("deadbeef.0").unwrap()),
        );
        let error = fixture.query.query(&authorization, &request).unwrap_err();
        assert_eq!(error.code, RemoteErrorCode::StaleCursor);

        request.body.page.cursor = None;
        request.body.budget.max_bytes = NonZeroU64::new(1).unwrap();
        let error = fixture.query.query(&authorization, &request).unwrap_err();
        assert_eq!(error.code, RemoteErrorCode::BudgetExceeded);
    }

    #[test]
    fn fixture_keeps_remote_state_outside_local_runtime_databases() {
        let fixture = fixture();
        assert!(fixture.control_path.exists());
        assert!(fixture.object_root.join("object-store.db").exists());
        assert!(!fixture.directory.path().join("ferrus.db").exists());
        assert!(!fixture.directory.path().join("repo-graph.db").exists());
        assert!(!fixture.directory.path().join("project-memory.db").exists());
    }
}
