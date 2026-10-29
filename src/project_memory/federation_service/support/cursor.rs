//! Federated revision fingerprints and pagination cursors.

use super::*;

pub(in super::super) fn revision_key(
    repository: Option<&RepositoryDomainState>,
    memory: Option<&MemoryDomainState>,
) -> Result<String, MemoryQueryError> {
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
    .map_err(|_| {
        backend_error(
            const {
                crate::project_memory::diagnostics::MemoryDiagnosticCode::from_static(
                    "federation.serialization",
                )
            },
        )
    })
}

#[derive(Serialize)]
pub(in super::super) struct SearchCursorShape<'a> {
    pub(super) project: &'a super::super::super::domain::ProjectRef,
    pub(super) target: &'a FederatedTarget,
    pub(super) text: String,
    pub(super) repository_kinds: Vec<String>,
    pub(super) repository_paths: Vec<String>,
    pub(super) memory_kinds: Vec<String>,
    pub(super) memory_sources: Vec<String>,
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
pub(in super::super) struct ContextCursorShape<'a> {
    pub(super) project: &'a super::super::super::domain::ProjectRef,
    pub(super) target: &'a FederatedTarget,
    pub(super) seeds: Vec<String>,
    pub(super) repository_policy: &'a crate::repository_graph::query::ContextPolicy,
    pub(super) memory_policy: &'a super::super::super::query::MemoryContextPolicy,
    pub(super) max_depth: u32,
}

impl<'a> TryFrom<&'a FederatedContextRequest> for ContextCursorShape<'a> {
    type Error = MemoryQueryError;

    fn try_from(request: &'a FederatedContextRequest) -> Result<Self, Self::Error> {
        let mut seeds = request
            .seeds
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| {
                backend_error(
                    const {
                        crate::project_memory::diagnostics::MemoryDiagnosticCode::from_static(
                            "federation.serialization",
                        )
                    },
                )
            })?;
        seeds.sort();
        seeds.dedup();
        Ok(Self {
            project: &request.scope.project,
            target: &request.scope.target,
            seeds,
            repository_policy: &request.repository_policy,
            memory_policy: &request.memory_policy,
            max_depth: request.scope.budget.max_depth.get(),
        })
    }
}

#[derive(Serialize, Deserialize)]
pub(in super::super) struct FederationCursorPayload {
    pub(super) version: u32,
    pub(super) operation: String,
    pub(super) fingerprint: String,
    pub(super) revision_key: String,
    pub(in super::super) offset: u64,
}

pub(in super::super) fn request_fingerprint(
    operation: &str,
    value: &impl Serialize,
) -> Result<String, MemoryQueryError> {
    hash(&(operation, value))
}

pub(in super::super) fn hash(value: &impl Serialize) -> Result<String, MemoryQueryError> {
    let encoded = serde_json::to_vec(value).map_err(|_| {
        backend_error(
            const {
                crate::project_memory::diagnostics::MemoryDiagnosticCode::from_static(
                    "federation.serialization",
                )
            },
        )
    })?;
    let mut hasher = Sha256::new();
    hasher.update(b"ferrus.project-memory.federation.v1\0");
    hasher.update(encoded);
    Ok(hex(&hasher.finalize()))
}

pub(in super::super) fn encode_cursor(
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
    let bytes = serde_json::to_vec(&payload).map_err(|_| {
        backend_error(
            const {
                crate::project_memory::diagnostics::MemoryDiagnosticCode::from_static(
                    "federation.serialization",
                )
            },
        )
    })?;
    FederationPageCursor::new(format!("cursor:{}", hex(&bytes))).map_err(|_| {
        backend_error(
            const {
                crate::project_memory::diagnostics::MemoryDiagnosticCode::from_static(
                    "federation.cursor",
                )
            },
        )
    })
}

pub(in super::super) fn decode_cursor(
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

pub(in super::super) fn validate_cursor_revision(
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
pub(in super::super) fn federated_page(
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

pub(in super::super) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(in super::super) fn unhex(value: &str) -> Option<Vec<u8>> {
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16)?;
            let low = (pair[1] as char).to_digit(16)?;
            Some(((high << 4) | low) as u8)
        })
        .collect()
}
