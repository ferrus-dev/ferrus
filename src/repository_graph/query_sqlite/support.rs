//! Graph cursor fingerprints, SQLite row decoding, and query error/serialization helpers.

use super::*;

#[derive(Serialize, Deserialize)]
struct CursorPayload {
    version: u32,
    operation: String,
    snapshot_id: SnapshotId,
    query_fingerprint: String,
    offset: u64,
}

pub(super) fn encode_cursor(
    operation: &str,
    snapshot: &SnapshotId,
    query_fingerprint: &str,
    offset: u64,
) -> Result<PageCursor, QueryError> {
    let payload = CursorPayload {
        version: 2,
        operation: operation.to_string(),
        snapshot_id: snapshot.clone(),
        query_fingerprint: query_fingerprint.to_string(),
        offset,
    };
    let bytes = serde_json::to_vec(&payload).map_err(|_| backend_error())?;
    PageCursor::new(format!("cursor:{}", hex(&bytes))).map_err(|_| backend_error())
}

pub(super) fn decode_cursor(
    cursor: Option<&PageCursor>,
    operation: &str,
    snapshot: &SnapshotId,
    query_fingerprint: &str,
) -> Result<u64, QueryError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let encoded = cursor
        .as_str()
        .strip_prefix("cursor:")
        .ok_or_else(stale_cursor_error)?;
    if encoded.len() > 4096 || encoded.len() % 2 != 0 {
        return Err(stale_cursor_error());
    }
    let bytes = unhex(encoded).ok_or_else(stale_cursor_error)?;
    let payload: CursorPayload =
        serde_json::from_slice(&bytes).map_err(|_| stale_cursor_error())?;
    if payload.version != 2
        || payload.operation != operation
        || payload.snapshot_id != *snapshot
        || payload.query_fingerprint != query_fingerprint
    {
        return Err(stale_cursor_error());
    }
    Ok(payload.offset)
}

#[derive(Serialize)]
struct SearchCursorParameters<'a> {
    text: String,
    node_kinds: Vec<&'a str>,
    paths: Vec<&'a str>,
}

pub(super) fn search_cursor_fingerprint(request: &SearchRequest) -> Result<String, QueryError> {
    let mut node_kinds = request
        .node_kinds
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    node_kinds.sort_unstable();
    node_kinds.dedup();
    let mut paths = request
        .paths
        .iter()
        .map(RepoPath::as_str)
        .collect::<Vec<_>>();
    paths.sort_unstable();
    paths.dedup();
    cursor_fingerprint(
        "search",
        &SearchCursorParameters {
            text: request.text.trim().to_lowercase(),
            node_kinds,
            paths,
        },
    )
}

pub(super) fn show_cursor_fingerprint(request: &ShowRequest) -> Result<String, QueryError> {
    cursor_fingerprint("show", &request.lookup)
}

#[derive(Serialize)]
struct ContextCursorParameters<'a> {
    seeds: Vec<String>,
    max_depth: u32,
    direction: EdgeDirection,
    edge_kinds: Vec<&'a str>,
    include_unresolved: bool,
    include_external: bool,
}

pub(super) fn context_cursor_fingerprint(
    request: &ContextRequest,
    max_depth: u32,
) -> Result<String, QueryError> {
    let mut seeds = request
        .seeds
        .iter()
        .map(context_seed_key)
        .collect::<Result<Vec<_>, _>>()?;
    seeds.sort_unstable();
    seeds.dedup();
    let mut edge_kinds = request
        .policy
        .edge_kinds
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    edge_kinds.sort_unstable();
    edge_kinds.dedup();
    cursor_fingerprint(
        "context",
        &ContextCursorParameters {
            seeds,
            max_depth,
            direction: request.policy.direction,
            edge_kinds,
            include_unresolved: request.policy.include_unresolved,
            include_external: request.policy.include_external,
        },
    )
}

pub(super) fn context_seed_key(seed: &ContextSeed) -> Result<String, QueryError> {
    serde_json::to_string(seed).map_err(|_| backend_error())
}

pub(super) fn cursor_fingerprint<T: Serialize>(
    operation: &str,
    parameters: &T,
) -> Result<String, QueryError> {
    let bytes = serde_json::to_vec(parameters).map_err(|_| backend_error())?;
    let mut hasher = Sha256::new();
    hasher.update(b"ferrus.repository-graph.cursor-parameters.v1\0");
    hasher.update(operation.as_bytes());
    hasher.update(b"\0");
    hasher.update(bytes);
    Ok(hex(&hasher.finalize()))
}

pub(super) fn decode_node(row: &Row<'_>) -> Result<GraphNode, QueryError> {
    let snapshot_id = SnapshotId::new(value::<String>(row, 0)?).map_err(|_| backend_error())?;
    let id = NodeId::new(value::<String>(row, 1)?).map_err(|_| backend_error())?;
    let kind = value(row, 2)?;
    let semantic_key = value::<Option<String>>(row, 3)?
        .map(SemanticKey::new)
        .transpose()
        .map_err(|_| backend_error())?;
    let provenance = decode_provenance(row, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 15, 16, 17, 18)?;
    let properties =
        serde_json::from_str(&value::<String>(row, 14)?).map_err(|_| backend_error())?;
    Ok(GraphNode {
        snapshot_id,
        id,
        kind,
        semantic_key,
        provenance,
        properties,
    })
}

pub(super) fn decode_edge(row: &Row<'_>) -> Result<GraphEdge, QueryError> {
    let snapshot_id = SnapshotId::new(value::<String>(row, 0)?).map_err(|_| backend_error())?;
    let id = EdgeId::new(value::<String>(row, 1)?).map_err(|_| backend_error())?;
    let kind = value(row, 2)?;
    let source = NodeId::new(value::<String>(row, 3)?).map_err(|_| backend_error())?;
    let target_node = value::<Option<String>>(row, 4)?;
    let external_target = value::<Option<String>>(row, 5)?;
    let provenance = decode_provenance(row, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 17, 18, 19, 20)?;
    let target = match (target_node, external_target, provenance.resolution) {
        (Some(target), None, _) => {
            EdgeTarget::Node(NodeId::new(target).map_err(|_| backend_error())?)
        }
        (None, Some(target), ResolutionState::External) => EdgeTarget::External(target),
        (None, Some(target), _) => EdgeTarget::Unresolved(target),
        _ => return Err(backend_error()),
    };
    let properties =
        serde_json::from_str(&value::<String>(row, 16)?).map_err(|_| backend_error())?;
    Ok(GraphEdge {
        snapshot_id,
        id,
        kind,
        source,
        target,
        provenance,
        properties,
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn decode_provenance(
    row: &Row<'_>,
    extractor_id: usize,
    extractor_version: usize,
    contract_version: usize,
    resolution: usize,
    confidence_index: usize,
    evidence_path: usize,
    evidence_algorithm: usize,
    evidence_digest: usize,
    start_byte: usize,
    end_byte: usize,
    start_line: usize,
    start_column: usize,
    end_line: usize,
    end_column: usize,
) -> Result<FactProvenance, QueryError> {
    let resolution = resolution_state(&value::<String>(row, resolution)?)?;
    let confidence = confidence(&value::<String>(row, confidence_index)?)?;
    let evidence = match value::<Option<String>>(row, evidence_path)? {
        None => None,
        Some(path) => {
            let algorithm =
                value::<Option<String>>(row, evidence_algorithm)?.ok_or_else(backend_error)?;
            let digest =
                value::<Option<String>>(row, evidence_digest)?.ok_or_else(backend_error)?;
            Some(SourceEvidence {
                path: RepoPath::new(path).map_err(|_| backend_error())?,
                content_identity: Digest::new(algorithm, digest).map_err(|_| backend_error())?,
                span: decode_span(
                    row,
                    start_byte,
                    end_byte,
                    start_line,
                    start_column,
                    end_line,
                    end_column,
                )?,
            })
        }
    };
    Ok(FactProvenance {
        extractor: ExtractorIdentity {
            id: ExtractorId::new(value::<String>(row, extractor_id)?)
                .map_err(|_| backend_error())?,
            version: value(row, extractor_version)?,
            contract_version: value(row, contract_version)?,
        },
        evidence,
        resolution,
        confidence,
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn decode_span(
    row: &Row<'_>,
    start_byte: usize,
    end_byte: usize,
    start_line: usize,
    start_column: usize,
    end_line: usize,
    end_column: usize,
) -> Result<Option<SourceSpan>, QueryError> {
    let start = value::<Option<i64>>(row, start_byte)?;
    let end = value::<Option<i64>>(row, end_byte)?;
    match (start, end) {
        (None, None) => Ok(None),
        (Some(start), Some(end)) => Ok(Some(SourceSpan {
            start: SourcePosition {
                byte_offset: unsigned(start).map_err(|_| backend_error())?,
                line: optional_u32(value(row, start_line)?)?,
                column: optional_u32(value(row, start_column)?)?,
            },
            end: SourcePosition {
                byte_offset: unsigned(end).map_err(|_| backend_error())?,
                line: optional_u32(value(row, end_line)?)?,
                column: optional_u32(value(row, end_column)?)?,
            },
        })),
        _ => Err(backend_error()),
    }
}

pub(super) fn resolution_state(value: &str) -> Result<ResolutionState, QueryError> {
    match value {
        "resolved" => Ok(ResolutionState::Resolved),
        "unresolved" => Ok(ResolutionState::Unresolved),
        "external" => Ok(ResolutionState::External),
        _ => Err(backend_error()),
    }
}

pub(super) fn confidence(value: &str) -> Result<Confidence, QueryError> {
    match value {
        "exact" => Ok(Confidence::Exact),
        "high" => Ok(Confidence::High),
        "medium" => Ok(Confidence::Medium),
        "low" => Ok(Confidence::Low),
        _ => Err(backend_error()),
    }
}

pub(super) fn diagnostic_severity(value: &str) -> Result<DiagnosticSeverity, QueryError> {
    match value {
        "info" => Ok(DiagnosticSeverity::Info),
        "warning" => Ok(DiagnosticSeverity::Warning),
        "error" => Ok(DiagnosticSeverity::Error),
        _ => Err(backend_error()),
    }
}

pub(super) fn optional_u32(value: Option<i64>) -> Result<Option<u32>, QueryError> {
    value
        .map(u32::try_from)
        .transpose()
        .map_err(|_| backend_error())
}

pub(super) fn value<T: rusqlite::types::FromSql>(
    row: &Row<'_>,
    index: usize,
) -> Result<T, QueryError> {
    row.get(index).map_err(|_| backend_error())
}

pub(super) fn unsigned(value: i64) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, value))
}

pub(super) fn serialized_len<T: Serialize>(value: &T) -> Result<u64, QueryError> {
    let len = serde_json::to_vec(value)
        .map_err(|_| backend_error())?
        .len();
    Ok(u64::try_from(len).unwrap_or(u64::MAX))
}

pub(super) fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

pub(super) fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

pub(super) fn unhex(value: &str) -> Option<Vec<u8>> {
    fn nibble(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        }
    }
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| Some((nibble(pair[0])? << 4) | nibble(pair[1])?))
        .collect()
}

pub(super) fn invalid_request(message: &'static str) -> QueryError {
    QueryError {
        wire_version: QUERY_WIRE_VERSION,
        code: QueryErrorCode::InvalidRequest,
        message: message.to_string(),
        retryable: false,
        recommended_action: None,
        details: BTreeMap::new(),
    }
}

pub(super) fn backend_error() -> QueryError {
    QueryError {
        wire_version: QUERY_WIRE_VERSION,
        code: QueryErrorCode::BackendUnavailable,
        message: "repository graph storage is unavailable or inconsistent".to_string(),
        retryable: true,
        recommended_action: None,
        details: BTreeMap::new(),
    }
}

pub(super) fn duration_budget_exceeded_error() -> QueryError {
    QueryError {
        wire_version: QUERY_WIRE_VERSION,
        code: QueryErrorCode::BudgetExceeded,
        message: "repository graph query exceeded the duration budget".to_string(),
        retryable: false,
        recommended_action: None,
        details: BTreeMap::new(),
    }
}

pub(super) fn not_built_error() -> QueryError {
    QueryError {
        wire_version: QUERY_WIRE_VERSION,
        code: QueryErrorCode::NotBuilt,
        message: "repository graph is not built; run `ferrus graph index`".to_string(),
        retryable: false,
        recommended_action: Some(RetrievalAction::Index),
        details: BTreeMap::new(),
    }
}

pub(super) fn index_building_error() -> QueryError {
    QueryError {
        wire_version: QUERY_WIRE_VERSION,
        code: QueryErrorCode::IndexBuilding,
        message: "repository graph is currently building; retry after indexing completes"
            .to_string(),
        retryable: true,
        recommended_action: Some(RetrievalAction::WaitForBuild),
        details: BTreeMap::new(),
    }
}

pub(super) fn index_failed_error() -> QueryError {
    QueryError {
        wire_version: QUERY_WIRE_VERSION,
        code: QueryErrorCode::IndexFailed,
        message: "repository graph build failed; run `ferrus graph index` to retry".to_string(),
        retryable: false,
        recommended_action: Some(RetrievalAction::RetryIndex),
        details: BTreeMap::new(),
    }
}

pub(super) fn snapshot_not_found_error() -> QueryError {
    QueryError {
        wire_version: QUERY_WIRE_VERSION,
        code: QueryErrorCode::SnapshotNotFound,
        message: "repository graph snapshot was not found".to_string(),
        retryable: false,
        recommended_action: None,
        details: BTreeMap::new(),
    }
}

pub(super) fn stale_cursor_error() -> QueryError {
    QueryError {
        wire_version: QUERY_WIRE_VERSION,
        code: QueryErrorCode::StaleCursor,
        message: "repository graph cursor does not match this query snapshot or parameters"
            .to_string(),
        retryable: false,
        recommended_action: None,
        details: BTreeMap::new(),
    }
}
