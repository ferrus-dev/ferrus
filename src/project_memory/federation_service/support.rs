use super::*;

pub(super) fn repository_only_seeds(
    seeds: &[FederatedContextSeed],
) -> Result<Vec<ContextSeed>, MemoryQueryError> {
    seeds
        .iter()
        .map(|seed| match seed {
            FederatedContextSeed::Repository(seed) => Ok(seed.clone()),
            _ => Err(backend_error("context.domainmismatch")),
        })
        .collect()
}

pub(super) fn memory_only_seeds(
    seeds: &[FederatedContextSeed],
) -> Result<Vec<MemoryContextSeed>, MemoryQueryError> {
    seeds
        .iter()
        .map(|seed| match seed {
            FederatedContextSeed::MemoryEntity(id) => Ok(MemoryContextSeed::Entity(id.clone())),
            FederatedContextSeed::Milestone(id) => Ok(MemoryContextSeed::Milestone(id.clone())),
            FederatedContextSeed::Task(id) => Ok(MemoryContextSeed::Task(id.clone())),
            FederatedContextSeed::Run(id) => Ok(MemoryContextSeed::Run(id.clone())),
            FederatedContextSeed::Repository(_) => Err(backend_error("context.domainmismatch")),
        })
        .collect()
}

pub(super) fn split_all_seeds(
    seeds: &[FederatedContextSeed],
) -> (Vec<ContextSeed>, Vec<MemoryContextSeed>) {
    let mut repository = Vec::new();
    let mut memory = Vec::new();
    for seed in seeds {
        match seed {
            FederatedContextSeed::Repository(seed) => repository.push(seed.clone()),
            FederatedContextSeed::MemoryEntity(id) => {
                memory.push(MemoryContextSeed::Entity(id.clone()));
            }
            FederatedContextSeed::Milestone(id) => {
                memory.push(MemoryContextSeed::Milestone(id.clone()));
            }
            FederatedContextSeed::Task(id) => {
                memory.push(MemoryContextSeed::Task(id.clone()));
            }
            FederatedContextSeed::Run(id) => {
                memory.push(MemoryContextSeed::Run(id.clone()));
            }
        }
    }
    (repository, memory)
}

pub(super) fn repository_seed_matches(
    seed: &ContextSeed,
    target: &MemoryRelationshipTarget,
    snapshot_id: Option<&crate::repository_graph::domain::SnapshotId>,
) -> bool {
    match (seed, target) {
        (
            ContextSeed::Node(node),
            MemoryRelationshipTarget::RepositoryNode {
                snapshot_id: target_snapshot,
                node_id,
                ..
            },
        ) => Some(target_snapshot) == snapshot_id && node_id == node,
        (
            ContextSeed::Path(path),
            MemoryRelationshipTarget::RepositoryPath {
                path: target_path,
                snapshot_id: target_snapshot,
                ..
            },
        ) => {
            target_path == path
                && target_snapshot
                    .as_ref()
                    .is_none_or(|id| Some(id) == snapshot_id)
        }
        (
            ContextSeed::Symbol(symbol),
            MemoryRelationshipTarget::RepositorySymbol {
                semantic_key,
                snapshot_id: target_snapshot,
                ..
            },
        ) => {
            semantic_key == symbol
                && target_snapshot
                    .as_ref()
                    .is_none_or(|id| Some(id) == snapshot_id)
        }
        _ => false,
    }
}

pub(super) fn repository_seed_from_target(
    target: &MemoryRelationshipTarget,
    snapshot_id: Option<&crate::repository_graph::domain::SnapshotId>,
) -> Option<ContextSeed> {
    match target {
        MemoryRelationshipTarget::RepositoryNode {
            snapshot_id: target_snapshot,
            node_id,
            ..
        } if Some(target_snapshot) == snapshot_id => Some(ContextSeed::Node(node_id.clone())),
        MemoryRelationshipTarget::RepositoryPath {
            path,
            snapshot_id: target_snapshot,
            ..
        } if target_snapshot
            .as_ref()
            .is_none_or(|id| Some(id) == snapshot_id) =>
        {
            Some(ContextSeed::Path(path.clone()))
        }
        MemoryRelationshipTarget::RepositorySymbol {
            semantic_key,
            snapshot_id: target_snapshot,
            ..
        } if target_snapshot
            .as_ref()
            .is_none_or(|id| Some(id) == snapshot_id) =>
        {
            Some(ContextSeed::Symbol(semantic_key.clone()))
        }
        _ => None,
    }
}

pub(super) fn repository_item_matches(
    item: &crate::repository_graph::query::ContextItem,
    target: &MemoryRelationshipTarget,
) -> bool {
    match target {
        MemoryRelationshipTarget::RepositoryNode { node_id, .. } => &item.node_id == node_id,
        MemoryRelationshipTarget::RepositoryPath { path, .. } => &item.path == path,
        MemoryRelationshipTarget::RepositorySymbol { semantic_key, .. } => {
            item.semantic_key.as_ref() == Some(semantic_key)
        }
        _ => false,
    }
}

pub(super) fn repository_state_from_search(response: &SearchResponse) -> RepositoryDomainState {
    RepositoryDomainState {
        repository: response.repository.clone(),
        snapshot_id: Some(response.snapshot_id.clone()),
        task_view: response.task_view.clone(),
        freshness: response.freshness.clone(),
        diagnostics: response.diagnostics.clone(),
        page: response.page.clone(),
    }
}

pub(super) fn repository_state_from_context(response: &ContextResponse) -> RepositoryDomainState {
    RepositoryDomainState {
        repository: response.repository.clone(),
        snapshot_id: Some(response.snapshot_id.clone()),
        task_view: response.task_view.clone(),
        freshness: response.freshness.clone(),
        diagnostics: response.diagnostics.clone(),
        page: response.page.clone(),
    }
}

pub(super) fn repository_state_from_status(response: &StatusResponse) -> RepositoryDomainState {
    RepositoryDomainState {
        repository: response.repository.clone(),
        snapshot_id: response.snapshot_id.clone(),
        task_view: response.task_view.clone(),
        freshness: response.freshness.clone(),
        diagnostics: response.diagnostics.clone(),
        page: response.page.clone(),
    }
}

pub(super) fn memory_state_from_search(response: &MemorySearchResponse) -> MemoryDomainState {
    MemoryDomainState {
        revision_id: Some(response.revision_id.clone()),
        freshness: response.freshness.clone(),
        authorized_sources: authorized_sources(),
        diagnostics: response.diagnostics.clone(),
        page: FederatedPageInfo {
            next_cursor: None,
            truncation: response.page.truncation.clone(),
        },
    }
}

pub(super) fn memory_state_from_context(response: &MemoryContextResponse) -> MemoryDomainState {
    MemoryDomainState {
        revision_id: Some(response.revision_id.clone()),
        freshness: response.freshness.clone(),
        authorized_sources: authorized_sources(),
        diagnostics: response.diagnostics.clone(),
        page: FederatedPageInfo {
            next_cursor: None,
            truncation: response.page.truncation.clone(),
        },
    }
}

pub(super) fn memory_state_from_status(response: &MemoryStatusResponse) -> MemoryDomainState {
    MemoryDomainState {
        revision_id: response.revision_id.clone(),
        freshness: response.freshness.clone(),
        authorized_sources: response
            .data
            .source_policy
            .iter()
            .filter_map(|status| status.policy.enabled.then_some(status.category))
            .collect(),
        diagnostics: response.diagnostics.clone(),
        page: FederatedPageInfo::default(),
    }
}

pub(super) fn authorized_sources() -> Vec<MemorySourceCategory> {
    MemoryPolicy::default().authorized_categories().collect()
}

pub(super) fn graph_error(error: QueryError) -> MemoryQueryError {
    match error.code {
        QueryErrorCode::NotBuilt | QueryErrorCode::IndexBuilding | QueryErrorCode::IndexFailed => {
            MemoryQueryError::Unavailable
        }
        QueryErrorCode::SnapshotNotFound => MemoryQueryError::RevisionNotFound,
        QueryErrorCode::StaleCursor => MemoryQueryError::StaleCursor,
        QueryErrorCode::BudgetExceeded => {
            MemoryQueryError::BudgetExceeded(MemoryTruncationReason::Duration)
        }
        QueryErrorCode::ContentChanged => MemoryQueryError::ContentChanged,
        _ => backend_error("federation.repositoryquery"),
    }
}

pub(super) fn backend_error(value: &str) -> MemoryQueryError {
    MemoryQueryError::Backend(diagnostic_code(value))
}

pub(super) fn diagnostic_code(value: &str) -> MemoryDiagnosticCode {
    MemoryDiagnosticCode::new(value).expect("static federation diagnostic code is valid")
}

pub(super) fn serialized_len(value: &impl Serialize) -> Result<u64, MemoryQueryError> {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len() as u64)
        .map_err(|_| backend_error("federation.serialization"))
}

pub(super) fn snippet_span_key(snippet: &ContextSnippet) -> (u64, u64) {
    snippet.span.as_ref().map_or((0, 0), |span| {
        (span.start.byte_offset, span.end.byte_offset)
    })
}

pub(super) fn retain_bounded<T: Serialize>(
    values: &mut Vec<T>,
    max_items: usize,
    remaining_bytes: &mut u64,
) -> Result<bool, MemoryQueryError> {
    let original_len = values.len();
    let mut retained = Vec::new();
    for value in values.drain(..) {
        if retained.len() >= max_items {
            break;
        }
        let bytes = serialized_len(&value)?;
        if bytes > *remaining_bytes {
            break;
        }
        *remaining_bytes -= bytes;
        retained.push(value);
    }
    let truncated = retained.len() < original_len;
    *values = retained;
    Ok(truncated)
}

pub(super) fn retain_snippets_bounded(
    snippets: &mut Vec<ContextSnippet>,
    max_snippet_bytes: u64,
    remaining_bytes: &mut u64,
) -> Result<bool, MemoryQueryError> {
    let original_len = snippets.len();
    let mut retained = Vec::new();
    let mut snippet_bytes = 0_u64;
    for snippet in snippets.drain(..) {
        let text_bytes = snippet.text.len() as u64;
        let bytes = serialized_len(&snippet)?;
        if snippet_bytes.saturating_add(text_bytes) > max_snippet_bytes || bytes > *remaining_bytes
        {
            break;
        }
        snippet_bytes = snippet_bytes.saturating_add(text_bytes);
        *remaining_bytes -= bytes;
        retained.push(snippet);
    }
    let truncated = retained.len() < original_len;
    *snippets = retained;
    Ok(truncated)
}

pub(super) fn dedup<T>(values: Vec<T>) -> Result<Vec<T>, MemoryQueryError>
where
    T: Serialize,
{
    let mut values = values
        .into_iter()
        .map(|value| {
            serde_json::to_string(&value)
                .map(|key| (key, value))
                .map_err(|_| backend_error("federation.serialization"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    values.sort_by(|left, right| left.0.cmp(&right.0));
    values.dedup_by(|left, right| left.0 == right.0);
    Ok(values.into_iter().map(|(_, value)| value).collect())
}

pub(super) fn federated_search_order(
    left: &FederatedSearchResult,
    right: &FederatedSearchResult,
) -> std::cmp::Ordering {
    search_score(right)
        .total_cmp(&search_score(left))
        .then_with(|| search_domain_rank(left).cmp(&search_domain_rank(right)))
        .then_with(|| search_result_key(left).cmp(&search_result_key(right)))
}

pub(super) fn search_score(result: &FederatedSearchResult) -> f64 {
    match result {
        FederatedSearchResult::Repository(hit) => hit.score,
        FederatedSearchResult::Memory(hit) => hit.score,
    }
}

pub(super) fn search_domain_rank(result: &FederatedSearchResult) -> u8 {
    match result {
        FederatedSearchResult::Repository(_) => 0,
        FederatedSearchResult::Memory(_) => 1,
    }
}

pub(super) fn search_result_key(result: &FederatedSearchResult) -> String {
    match result {
        FederatedSearchResult::Repository(hit) => format!("repository:{}", hit.node_id.as_str()),
        FederatedSearchResult::Memory(hit) => format!("memory:{}", hit.entity.id.as_str()),
    }
}

pub(super) fn federated_context_order(
    left: &FederatedContextItem,
    right: &FederatedContextItem,
) -> std::cmp::Ordering {
    context_rank(left)
        .cmp(&context_rank(right))
        .then_with(|| context_item_key(left).cmp(&context_item_key(right)))
}

pub(super) fn context_rank(item: &FederatedContextItem) -> (u8, u8) {
    match item {
        FederatedContextItem::Repository(item) => {
            let exact = item
                .selection_reasons
                .iter()
                .any(|reason| reason.kind == ContextSelectionKind::ExactSeed);
            (u8::from(!exact), 0)
        }
        FederatedContextItem::Memory(item) => {
            let exact = item
                .selection_reasons
                .iter()
                .any(|reason| reason.as_str() == "context.seed");
            (u8::from(!exact), 1)
        }
    }
}

pub(super) fn context_item_key(item: &FederatedContextItem) -> String {
    match item {
        FederatedContextItem::Repository(item) => {
            format!("repository:{}", item.node_id.as_str())
        }
        FederatedContextItem::Memory(item) => format!("memory:{}", item.entity.id.as_str()),
    }
}

pub(super) fn revision_key(
    repository: Option<&RepositoryDomainState>,
    memory: Option<&MemoryDomainState>,
) -> String {
    hash(&(
        repository.and_then(|state| state.snapshot_id.as_ref().map(|id| id.as_str())),
        repository
            .and_then(|state| state.task_view.as_ref())
            .map(|task_view| task_view.task_view_id.as_str()),
        repository
            .and_then(|state| state.task_view.as_ref())
            .and_then(|task_view| task_view.overlay_revision_id.as_ref())
            .map(|id| id.as_str()),
        memory.and_then(|state| state.revision_id.as_ref().map(|id| id.as_str())),
    ))
    .expect("federation revision identity is serializable")
}

#[derive(Serialize)]
pub(super) struct SearchCursorShape<'a> {
    project: &'a super::super::domain::ProjectRef,
    target: &'a FederatedTarget,
    text: String,
    repository_kinds: Vec<String>,
    repository_paths: Vec<String>,
    memory_kinds: Vec<String>,
    memory_sources: Vec<String>,
}

impl<'a> From<&'a FederatedSearchRequest> for SearchCursorShape<'a> {
    fn from(request: &'a FederatedSearchRequest) -> Self {
        let mut repository_kinds = request
            .repository_kinds
            .iter()
            .map(|kind| kind.as_str().to_string())
            .collect::<Vec<_>>();
        repository_kinds.sort();
        repository_kinds.dedup();
        let mut repository_paths = request
            .repository_paths
            .iter()
            .map(|path| path.as_str().to_string())
            .collect::<Vec<_>>();
        repository_paths.sort();
        repository_paths.dedup();
        let mut memory_kinds = request
            .memory_kinds
            .iter()
            .map(|kind| kind.as_str().to_string())
            .collect::<Vec<_>>();
        memory_kinds.sort();
        memory_kinds.dedup();
        let mut memory_sources = request
            .memory_sources
            .iter()
            .map(|source| format!("{source:?}"))
            .collect::<Vec<_>>();
        memory_sources.sort();
        memory_sources.dedup();
        Self {
            project: &request.scope.project,
            target: &request.scope.target,
            text: request.text.as_str().trim().to_lowercase(),
            repository_kinds,
            repository_paths,
            memory_kinds,
            memory_sources,
        }
    }
}

#[derive(Serialize)]
pub(super) struct ContextCursorShape<'a> {
    project: &'a super::super::domain::ProjectRef,
    target: &'a FederatedTarget,
    seeds: Vec<String>,
    repository_policy: &'a crate::repository_graph::query::ContextPolicy,
    memory_policy: &'a super::super::query::MemoryContextPolicy,
    max_depth: u32,
}

impl<'a> From<&'a FederatedContextRequest> for ContextCursorShape<'a> {
    fn from(request: &'a FederatedContextRequest) -> Self {
        let mut seeds = request
            .seeds
            .iter()
            .map(|seed| serde_json::to_string(seed).expect("federation seed is serializable"))
            .collect::<Vec<_>>();
        seeds.sort();
        seeds.dedup();
        Self {
            project: &request.scope.project,
            target: &request.scope.target,
            seeds,
            repository_policy: &request.repository_policy,
            memory_policy: &request.memory_policy,
            max_depth: request.scope.budget.max_depth.get(),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub(super) struct FederationCursorPayload {
    version: u32,
    operation: String,
    fingerprint: String,
    revision_key: String,
    pub(super) offset: u64,
}

pub(super) fn request_fingerprint(
    operation: &str,
    value: &impl Serialize,
) -> Result<String, MemoryQueryError> {
    hash(&(operation, value))
}

pub(super) fn hash(value: &impl Serialize) -> Result<String, MemoryQueryError> {
    let encoded =
        serde_json::to_vec(value).map_err(|_| backend_error("federation.serialization"))?;
    let mut hasher = Sha256::new();
    hasher.update(b"ferrus.project-memory.federation.v1\0");
    hasher.update(encoded);
    Ok(hex(&hasher.finalize()))
}

pub(super) fn encode_cursor(
    operation: &str,
    fingerprint: &str,
    revision_key: &str,
    offset: usize,
) -> Result<FederationPageCursor, MemoryQueryError> {
    let payload = FederationCursorPayload {
        version: CURSOR_VERSION,
        operation: operation.to_string(),
        fingerprint: fingerprint.to_string(),
        revision_key: revision_key.to_string(),
        offset: u64::try_from(offset).unwrap_or(u64::MAX),
    };
    let bytes =
        serde_json::to_vec(&payload).map_err(|_| backend_error("federation.serialization"))?;
    FederationPageCursor::new(format!("cursor:{}", hex(&bytes)))
        .map_err(|_| backend_error("federation.cursor"))
}

pub(super) fn decode_cursor(
    cursor: Option<&FederationPageCursor>,
    operation: &str,
    fingerprint: &str,
) -> Result<Option<FederationCursorPayload>, MemoryQueryError> {
    let Some(cursor) = cursor else {
        return Ok(None);
    };
    let encoded = cursor
        .as_str()
        .strip_prefix("cursor:")
        .ok_or(MemoryQueryError::StaleCursor)?;
    if encoded.len() > 4_096 || encoded.len() % 2 != 0 {
        return Err(MemoryQueryError::StaleCursor);
    }
    let bytes = unhex(encoded).ok_or(MemoryQueryError::StaleCursor)?;
    let payload: FederationCursorPayload =
        serde_json::from_slice(&bytes).map_err(|_| MemoryQueryError::StaleCursor)?;
    if payload.version != CURSOR_VERSION
        || payload.operation != operation
        || payload.fingerprint != fingerprint
    {
        return Err(MemoryQueryError::StaleCursor);
    }
    Ok(Some(payload))
}

pub(super) fn validate_cursor_revision(
    cursor: Option<&FederationCursorPayload>,
    revision_key: &str,
) -> Result<(), MemoryQueryError> {
    if cursor.is_some_and(|cursor| cursor.revision_key != revision_key) {
        Err(MemoryQueryError::StaleCursor)
    } else {
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn federated_page(
    reason: Option<MemoryTruncationReason>,
    returned_results: usize,
    returned_bytes: u64,
    explored_depth: u32,
    has_more: bool,
    operation: &str,
    fingerprint: &str,
    revision_key: &str,
    offset: usize,
) -> Result<FederatedPageInfo, MemoryQueryError> {
    let truncation = reason.map(|reason| MemoryTruncation {
        reason,
        returned_results: u32::try_from(returned_results).unwrap_or(u32::MAX),
        returned_bytes,
        explored_depth,
    });
    let next_cursor = (has_more
        && returned_results > 0
        && matches!(
            reason,
            Some(MemoryTruncationReason::Results | MemoryTruncationReason::Bytes)
        ))
    .then(|| encode_cursor(operation, fingerprint, revision_key, offset))
    .transpose()?;
    Ok(FederatedPageInfo {
        next_cursor,
        truncation,
    })
}

pub(super) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(super) fn unhex(value: &str) -> Option<Vec<u8>> {
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16)?;
            let low = (pair[1] as char).to_digit(16)?;
            Some(((high << 4) | low) as u8)
        })
        .collect()
}
