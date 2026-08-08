use super::*;

pub(super) struct LoadedTarget {
    pub(super) graph: Option<StoredRemoteGraphSnapshot>,
    pub(super) memory: Option<StoredRemoteMemoryRevision>,
}

#[derive(Clone, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub(super) enum GraphUnit {
    Node(GraphNode),
    Edge(crate::repository_graph::domain::GraphEdge),
}

#[derive(Clone, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub(super) enum ContextUnit {
    GraphNode(GraphNode),
    GraphEdge(crate::repository_graph::domain::GraphEdge),
    MemoryEntity(MemoryEntity),
    MemoryRelationship(MemoryRelationship),
}

pub(super) fn authorize_query(
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

pub(super) fn authorize(
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

pub(super) fn validate_operation_target(
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

pub(super) fn validate_context_seeds(
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

pub(super) fn search_candidates(
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

pub(super) fn graph_match(node: &GraphNode, needle: &str) -> Option<(u32, RemoteSearchMatchKind)> {
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

pub(super) fn graph_search_values(node: &GraphNode) -> Vec<String> {
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

pub(super) fn memory_match(
    entity: &MemoryEntity,
    needle: &str,
) -> Option<(u32, RemoteSearchMatchKind)> {
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

pub(super) fn memory_title(data: &MemoryEntityData) -> Option<&str> {
    match data {
        MemoryEntityData::Specification { title } | MemoryEntityData::Milestone { title, .. } => {
            Some(title.as_str())
        }
        _ => None,
    }
}

pub(super) fn graph_neighborhood(
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
pub(super) fn context_units(
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

pub(super) fn graph_seed_ids(
    graph: &StoredRemoteGraphSnapshot,
    seeds: &[RemoteContextSeed],
) -> Vec<NodeId> {
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

pub(super) fn memory_context_units(
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

pub(super) fn bounded_diagnostics(loaded: &LoadedTarget, max: u32) -> Vec<RemoteQueryDiagnostic> {
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

pub(super) fn paginate<T: Clone + Serialize>(
    items: Vec<T>,
    cursor: Option<&RemotePageCursor>,
    fingerprint: &str,
    budget: super::super::api::RemoteQueryBudget,
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

pub(super) fn query_fingerprint(
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

pub(super) fn cursor_offset(
    cursor: Option<&RemotePageCursor>,
    fingerprint: &str,
) -> Result<usize, ()> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let (stored, offset) = cursor.as_str().rsplit_once('.').ok_or(())?;
    if stored != fingerprint {
        return Err(());
    }
    offset.parse().map_err(|_| ())
}

pub(super) fn evidence_slice<'a>(
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

pub(super) fn bounded_utf8(bytes: &[u8], max_bytes: u64) -> Option<(String, bool, u64)> {
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

pub(super) fn empty_page() -> RemotePageInfo {
    RemotePageInfo {
        next_cursor: None,
        truncation: None,
        returned_results: 0,
        returned_bytes: 0,
        explored_depth: 0,
    }
}

pub(super) fn encoded_len(value: &impl Serialize) -> Result<u64, ()> {
    serde_json::to_vec(value)
        .ok()
        .and_then(|encoded| u64::try_from(encoded.len()).ok())
        .ok_or(())
}

pub(super) fn protocol_error(
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

pub(super) fn control_backend_error(
    request_id: &RequestId,
    error: CoordinatorError,
) -> RemoteError {
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

pub(super) fn remote_error(
    request_id: &RequestId,
    code: RemoteErrorCode,
    retryable: bool,
) -> RemoteError {
    RemoteError {
        protocol_version: DISTRIBUTED_QUERY_PROTOCOL_VERSION,
        request_id: request_id.clone(),
        code,
        retryable,
    }
}

pub(super) fn query_invalid(request_id: &RequestId) -> RemoteError {
    remote_error(request_id, RemoteErrorCode::InvalidRequest, false)
}

pub(super) fn query_not_found(request_id: &RequestId) -> RemoteError {
    remote_error(request_id, RemoteErrorCode::NotFound, false)
}

pub(super) fn query_stale_cursor(request_id: &RequestId) -> RemoteError {
    remote_error(request_id, RemoteErrorCode::StaleCursor, false)
}

pub(super) fn query_budget(request_id: &RequestId) -> RemoteError {
    remote_error(request_id, RemoteErrorCode::BudgetExceeded, false)
}

pub(super) fn query_internal(request_id: &RequestId) -> RemoteError {
    remote_error(request_id, RemoteErrorCode::Internal, false)
}

pub(super) fn query_store_error(request_id: &RequestId, error: RemoteStoreError) -> RemoteError {
    match error {
        RemoteStoreError::ReadBudgetExceeded => query_budget(request_id),
        RemoteStoreError::Database(_) => {
            remote_error(request_id, RemoteErrorCode::TemporarilyUnavailable, true)
        }
        _ => query_internal(request_id),
    }
}

pub(super) fn query_object_error(request_id: &RequestId, error: ObjectStoreError) -> RemoteError {
    match error {
        ObjectStoreError::Database(_)
        | ObjectStoreError::Io(_)
        | ObjectStoreError::ObjectUnavailable => {
            remote_error(request_id, RemoteErrorCode::TemporarilyUnavailable, true)
        }
        ObjectStoreError::InsecureProtection
        | ObjectStoreError::ContentIdentityMismatch
        | ObjectStoreError::ObjectQuotaExceeded
        | ObjectStoreError::ProjectObjectQuotaExceeded
        | ObjectStoreError::ProjectByteQuotaExceeded
        | ObjectStoreError::IntegrityFailure
        | ObjectStoreError::IncompatibleSchema
        | ObjectStoreError::Encryption => query_internal(request_id),
    }
}
