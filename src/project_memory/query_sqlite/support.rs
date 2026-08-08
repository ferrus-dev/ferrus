use super::*;

pub(super) fn matching_seed_entities(
    seed: &MemoryContextSeed,
    entities: &BTreeMap<MemoryEntityId, MemoryEntity>,
    relationships: &[MemoryRelationship],
    policy: &super::super::query::MemoryContextPolicy,
) -> Vec<MemoryEntityId> {
    let mut matches = match seed {
        MemoryContextSeed::Entity(entity_id) => entities
            .contains_key(entity_id)
            .then_some(entity_id.clone())
            .into_iter()
            .collect::<Vec<_>>(),
        MemoryContextSeed::Milestone(record_id) => entities
            .values()
            .filter_map(|entity| match &entity.data {
                MemoryEntityData::Milestone { milestone_id, .. } if milestone_id == record_id => {
                    Some(entity.id.clone())
                }
                MemoryEntityData::TaskReference { milestone_id, .. }
                | MemoryEntityData::FollowUpWork { milestone_id, .. }
                    if milestone_id.as_ref() == Some(record_id) =>
                {
                    Some(entity.id.clone())
                }
                _ => None,
            })
            .collect(),
        MemoryContextSeed::Task(record_id) => entities
            .values()
            .filter_map(|entity| match &entity.data {
                MemoryEntityData::TaskReference { task_id, .. } if task_id == record_id => {
                    Some(entity.id.clone())
                }
                MemoryEntityData::RunReference { task_id, .. } if task_id == record_id => {
                    Some(entity.id.clone())
                }
                _ => None,
            })
            .collect(),
        MemoryContextSeed::Run(record_id) => entities
            .values()
            .filter_map(|entity| match &entity.data {
                MemoryEntityData::RunReference { run_id, .. } if run_id == record_id => {
                    Some(entity.id.clone())
                }
                _ => None,
            })
            .collect(),
        MemoryContextSeed::RepositoryPath(path) => relationships
            .iter()
            .filter(|relationship| resolution_visible(relationship.provenance.resolution, policy))
            .filter_map(|relationship| match &relationship.target {
                MemoryRelationshipTarget::RepositoryPath { path: target, .. } if target == path => {
                    Some(relationship.source.clone())
                }
                MemoryRelationshipTarget::RepositoryNode { .. } => None,
                _ => None,
            })
            .collect(),
        MemoryContextSeed::RepositorySymbol(symbol) => relationships
            .iter()
            .filter(|relationship| resolution_visible(relationship.provenance.resolution, policy))
            .filter_map(|relationship| match &relationship.target {
                MemoryRelationshipTarget::RepositorySymbol { semantic_key, .. }
                    if semantic_key == symbol =>
                {
                    Some(relationship.source.clone())
                }
                _ => None,
            })
            .collect(),
    };
    matches.sort();
    matches.dedup();
    matches
}

pub(super) fn resolution_visible(
    resolution: MemoryResolutionState,
    policy: &super::super::query::MemoryContextPolicy,
) -> bool {
    match resolution {
        MemoryResolutionState::Resolved => true,
        MemoryResolutionState::Stale => policy.include_stale,
        MemoryResolutionState::Unresolved => policy.include_unresolved,
    }
}

pub(super) fn memory_search_hit(entity: MemoryEntity, raw_query: &str) -> Option<MemorySearchHit> {
    let query = raw_query.trim().to_lowercase();
    let id = entity.id.as_str().to_lowercase();
    let title = entity_title(&entity).map(str::to_lowercase);
    let text = entity_text(&entity).map(str::to_lowercase);
    let provenance = serde_json::to_string(&entity.provenance)
        .expect("memory provenance is serializable")
        .to_lowercase();
    let (match_kind, score, reason) = if id == query {
        (MemorySearchMatchKind::ExactId, 1.0, "search.exactid")
    } else if title.as_deref() == Some(query.as_str()) {
        (MemorySearchMatchKind::ExactTitle, 0.95, "search.exacttitle")
    } else if title
        .as_ref()
        .is_some_and(|title| title.starts_with(&query))
    {
        (
            MemorySearchMatchKind::TitlePrefix,
            0.85,
            "search.titleprefix",
        )
    } else if title.as_ref().is_some_and(|title| title.contains(&query)) {
        (
            MemorySearchMatchKind::TitleContains,
            0.75,
            "search.titlecontains",
        )
    } else if text.as_ref().is_some_and(|text| text.contains(&query)) {
        (
            MemorySearchMatchKind::CuratedTextContains,
            0.65,
            "search.textcontains",
        )
    } else if provenance.contains(&query) {
        (
            MemorySearchMatchKind::ProvenanceReference,
            0.55,
            "search.provenance",
        )
    } else {
        return None;
    };
    Some(MemorySearchHit {
        entity,
        match_kind,
        score,
        selection_reasons: vec![diagnostic_code(reason)],
    })
}

pub(super) fn entity_title(entity: &MemoryEntity) -> Option<&str> {
    match &entity.data {
        MemoryEntityData::Specification { title } | MemoryEntityData::Milestone { title, .. } => {
            Some(title.as_str())
        }
        _ => None,
    }
}

pub(super) fn entity_text(entity: &MemoryEntity) -> Option<&str> {
    match &entity.data {
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

pub(super) fn memory_hit_order(
    left: &MemorySearchHit,
    right: &MemorySearchHit,
) -> std::cmp::Ordering {
    right
        .score
        .total_cmp(&left.score)
        .then_with(|| left.entity.data.kind().cmp(&right.entity.data.kind()))
        .then_with(|| left.entity.id.cmp(&right.entity.id))
}

pub(super) fn freshness(
    revision: &MemoryRevision,
    comparison: Option<&super::super::query::MemoryFreshnessComparison>,
) -> MemoryFreshnessEnvelope {
    let Some(comparison) = comparison else {
        return unknown_freshness();
    };
    let mut reason_codes = Vec::new();
    if comparison.source_set_digest != revision.source_set_digest {
        reason_codes.push(diagnostic_code("freshness.sourceschanged"));
    }
    if comparison.policy_digest != revision.policy_digest {
        reason_codes.push(diagnostic_code("freshness.policychanged"));
    }
    if comparison.extractor_set_digest != revision.extractor_set_digest {
        reason_codes.push(diagnostic_code("freshness.extractorschanged"));
    }
    MemoryFreshnessEnvelope {
        freshness: if reason_codes.is_empty() {
            MemoryFreshness::Fresh
        } else {
            MemoryFreshness::Stale
        },
        compared_source_set_digest: Some(comparison.source_set_digest.clone()),
        reason_codes,
    }
}

pub(super) fn unknown_freshness() -> MemoryFreshnessEnvelope {
    MemoryFreshnessEnvelope {
        freshness: MemoryFreshness::Unknown,
        compared_source_set_digest: None,
        reason_codes: vec![diagnostic_code("freshness.notcompared")],
    }
}

pub(super) fn freshness_action(
    freshness: &MemoryFreshnessEnvelope,
) -> Option<MemoryRetrievalAction> {
    matches!(freshness.freshness, MemoryFreshness::Stale).then_some(MemoryRetrievalAction::Refresh)
}

pub(super) fn source_policy_status() -> Vec<MemorySourcePolicyStatus> {
    let policy = MemoryPolicy::default();
    MemorySourceCategory::ALL
        .into_iter()
        .filter_map(|category| {
            policy
                .category(category)
                .copied()
                .map(|policy| MemorySourcePolicyStatus { category, policy })
        })
        .collect()
}

pub(super) fn validate_wire_version(version: u32) -> Result<(), MemoryQueryError> {
    if version == MEMORY_QUERY_WIRE_VERSION {
        Ok(())
    } else {
        Err(invalid_request("query.wireversion"))
    }
}

pub(super) fn invalid_request(code: &str) -> MemoryQueryError {
    MemoryQueryError::Backend(diagnostic_code(code))
}

pub(super) fn store_error(error: MemoryStoreError) -> MemoryQueryError {
    match error {
        MemoryStoreError::RevisionNotFound => MemoryQueryError::RevisionNotFound,
        MemoryStoreError::RequiresRebuild => backend_error("storage.incompatible"),
        _ => backend_error("storage.unavailable"),
    }
}

pub(super) fn sqlite_error(error: SqliteError) -> MemoryQueryError {
    if matches!(
        error,
        SqliteError::SqliteFailure(ref failure, _) if failure.code == ErrorCode::OperationInterrupted
    ) {
        MemoryQueryError::BudgetExceeded(MemoryTruncationReason::Duration)
    } else {
        backend_error("storage.unavailable")
    }
}

pub(super) fn serialization_error(_: serde_json::Error) -> MemoryQueryError {
    backend_error("storage.corrupt")
}

pub(super) fn backend_error(code: &str) -> MemoryQueryError {
    MemoryQueryError::Backend(diagnostic_code(code))
}

pub(super) fn diagnostic_code(value: &str) -> MemoryDiagnosticCode {
    MemoryDiagnosticCode::new(value).expect("static memory diagnostic code is valid")
}

pub(super) fn unsigned(value: i64) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

pub(super) fn serialized_len(value: &impl Serialize) -> Result<u64, MemoryQueryError> {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len() as u64)
        .map_err(serialization_error)
}

#[derive(Serialize, Deserialize)]
pub(super) struct CursorPayload {
    version: u32,
    operation: String,
    revision_id: MemoryRevisionId,
    fingerprint: String,
    offset: u64,
}

pub(super) fn encode_cursor(
    operation: &str,
    revision_id: &MemoryRevisionId,
    fingerprint: &str,
    offset: usize,
) -> Result<MemoryPageCursor, MemoryQueryError> {
    let payload = CursorPayload {
        version: CURSOR_VERSION,
        operation: operation.to_string(),
        revision_id: revision_id.clone(),
        fingerprint: fingerprint.to_string(),
        offset: u64::try_from(offset).unwrap_or(u64::MAX),
    };
    let bytes = serde_json::to_vec(&payload).map_err(serialization_error)?;
    MemoryPageCursor::new(format!("cursor:{}", hex(&bytes)))
        .map_err(|_| backend_error("query.cursor"))
}

pub(super) fn decode_cursor(
    cursor: Option<&MemoryPageCursor>,
    operation: &str,
    revision_id: &MemoryRevisionId,
    fingerprint: &str,
) -> Result<u64, MemoryQueryError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let encoded = cursor
        .as_str()
        .strip_prefix("cursor:")
        .ok_or(MemoryQueryError::StaleCursor)?;
    if encoded.len() > 4_096 || encoded.len() % 2 != 0 {
        return Err(MemoryQueryError::StaleCursor);
    }
    let bytes = unhex(encoded).ok_or(MemoryQueryError::StaleCursor)?;
    let payload: CursorPayload =
        serde_json::from_slice(&bytes).map_err(|_| MemoryQueryError::StaleCursor)?;
    if payload.version != CURSOR_VERSION
        || payload.operation != operation
        || payload.revision_id != *revision_id
        || payload.fingerprint != fingerprint
    {
        return Err(MemoryQueryError::StaleCursor);
    }
    Ok(payload.offset)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn memory_page(
    reason: Option<MemoryTruncationReason>,
    returned_results: usize,
    returned_bytes: u64,
    explored_depth: u32,
    has_more: bool,
    operation: &str,
    revision_id: &MemoryRevisionId,
    fingerprint: &str,
    offset: usize,
) -> Result<MemoryPageInfo, MemoryQueryError> {
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
    .then(|| encode_cursor(operation, revision_id, fingerprint, offset))
    .transpose()?;
    Ok(MemoryPageInfo {
        next_cursor,
        truncation,
    })
}

#[derive(Serialize)]
struct SearchFingerprint<'a> {
    text: String,
    entity_kinds: Vec<MemoryEntityKind>,
    source_categories: Vec<MemorySourceCategory>,
    marker: &'a str,
}

pub(super) fn search_fingerprint(
    request: &MemorySearchRequest,
) -> Result<String, MemoryQueryError> {
    let mut entity_kinds = request.entity_kinds.clone();
    entity_kinds.sort();
    entity_kinds.dedup();
    let mut source_categories = request.source_categories.clone();
    source_categories.sort();
    source_categories.dedup();
    fingerprint(&SearchFingerprint {
        text: request.text.as_str().trim().to_lowercase(),
        entity_kinds,
        source_categories,
        marker: "search",
    })
}

#[derive(Serialize)]
struct ContextFingerprint<'a> {
    seeds: Vec<String>,
    relationship_kinds: Vec<MemoryRelationshipKind>,
    include_unresolved: bool,
    include_stale: bool,
    include_snippets: bool,
    max_depth: u32,
    marker: &'a str,
}

pub(super) fn context_fingerprint(
    request: &MemoryContextRequest,
    max_depth: u32,
) -> Result<String, MemoryQueryError> {
    let mut seeds = request
        .seeds
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()
        .map_err(serialization_error)?;
    seeds.sort();
    seeds.dedup();
    let mut relationship_kinds = request.policy.relationship_kinds.clone();
    relationship_kinds.sort();
    relationship_kinds.dedup();
    fingerprint(&ContextFingerprint {
        seeds,
        relationship_kinds,
        include_unresolved: request.policy.include_unresolved,
        include_stale: request.policy.include_stale,
        include_snippets: request.policy.include_snippets,
        max_depth,
        marker: "context",
    })
}

pub(super) fn fingerprint(value: &impl Serialize) -> Result<String, MemoryQueryError> {
    let encoded = serde_json::to_vec(value).map_err(serialization_error)?;
    let mut hasher = Sha256::new();
    hasher.update(b"ferrus.project-memory.cursor.v1\0");
    hasher.update(encoded);
    Ok(hex(&hasher.finalize()))
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
