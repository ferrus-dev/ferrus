//! SQLite persistence for complete index snapshots and reusable raw fragments.

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};

use super::{
    domain::{
        BuildId, BuildState, Confidence, DiagnosticSeverity, EdgeTarget, GraphDiagnostic,
        GraphEdge, GraphNode, GraphSnapshot, GraphValue, ResolutionState, SourceEvidence,
        SourceSpan,
    },
    ports::{
        FragmentCacheKey, GraphFragment, IndexBuildMetrics, IndexCommit, IndexStore,
        SourceFileDescriptor, SourceFileMode,
    },
    sqlite::Sidecar,
    store::{
        StoreError, load_build, load_snapshot, timestamp, validate_equivalent_snapshot,
        validate_snapshot_for_build,
    },
};

impl IndexStore for Sidecar {
    fn load_cached_fragment(
        &mut self,
        key: &FragmentCacheKey,
    ) -> Result<Option<GraphFragment>, Self::Error> {
        let raw = self
            .connection()
            .prepare_cached(
                "SELECT fragment_json FROM fragment_cache WHERE \
                 repository_namespace = ?1 AND repository_id = ?2 AND path = ?3 AND \
                 content_algorithm = ?4 AND content_digest = ?5 AND byte_length = ?6 AND \
                 file_mode = ?7 AND analysis_config_algorithm = ?8 AND \
                 analysis_config_digest = ?9 AND extractor_id = ?10 AND \
                 extractor_version = ?11 AND extractor_contract_version = ?12",
            )?
            .query_row(cache_params(key)?, |row| row.get::<_, String>(0))
            .optional()?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        let fragment = match serde_json::from_str::<GraphFragment>(&raw) {
            Ok(fragment) => fragment,
            Err(_) => {
                self.connection_mut().execute(
                    "DELETE FROM fragment_cache WHERE \
                     repository_namespace = ?1 AND repository_id = ?2 AND path = ?3 AND \
                     content_algorithm = ?4 AND content_digest = ?5 AND byte_length = ?6 AND \
                     file_mode = ?7 AND analysis_config_algorithm = ?8 AND \
                     analysis_config_digest = ?9 AND extractor_id = ?10 AND \
                     extractor_version = ?11 AND extractor_contract_version = ?12",
                    cache_params(key)?,
                )?;
                return Ok(None);
            }
        };
        Ok(Some(fragment))
    }

    fn complete_index(&mut self, commit: &IndexCommit) -> Result<GraphSnapshot, Self::Error> {
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let build = load_build(&transaction, &commit.snapshot.completed_by)?.ok_or_else(|| {
            StoreError::BuildNotFound(commit.snapshot.completed_by.as_str().to_string())
        })?;
        if build.state != BuildState::Building {
            return Err(StoreError::InvalidTransition {
                state: build.state,
                operation: "complete index",
            });
        }
        validate_snapshot_for_build(&commit.snapshot, &build)?;

        let completed = if let Some(existing) = load_snapshot(&transaction, &commit.snapshot.id)? {
            validate_equivalent_snapshot(&commit.snapshot, &existing)?;
            existing
        } else {
            insert_snapshot(&transaction, &commit.snapshot)?;
            insert_files(&transaction, &commit.snapshot, &commit.files)?;
            insert_nodes(&transaction, &commit.graph.nodes)?;
            insert_edges(&transaction, &commit.graph.edges)?;
            commit.snapshot.clone()
        };
        replace_snapshot_diagnostics(&transaction, &commit.snapshot, &commit.graph.diagnostics)?;
        for cached in &commit.cache_writes {
            upsert_cached_fragment(&transaction, &cached.key, &cached.fragment)?;
        }
        touch_cached_fragments(&transaction, &commit.cache_hits)?;
        upsert_metrics(&transaction, &commit.snapshot.completed_by, &commit.metrics)?;
        transaction.execute(
            "UPDATE index_builds SET finished_at = ?2 WHERE id = ?1",
            params![commit.snapshot.completed_by.as_str(), timestamp()],
        )?;
        transaction.commit()?;
        Ok(completed)
    }

    fn record_build_metrics(
        &mut self,
        build_id: &BuildId,
        metrics: &IndexBuildMetrics,
    ) -> Result<(), Self::Error> {
        upsert_metrics(self.connection(), build_id, metrics)
    }
}

impl Sidecar {
    pub fn index_build_metrics(
        &self,
        build_id: &BuildId,
    ) -> Result<Option<IndexBuildMetrics>, StoreError> {
        self.connection()
            .query_row(
                "SELECT discovered_files, reused_files, parsed_files, skipped_files, \
                        failed_files, processed_bytes, nodes, edges, diagnostics, duration_ms \
                 FROM build_metrics WHERE build_id = ?1",
                [build_id.as_str()],
                |row| {
                    Ok(IndexBuildMetrics {
                        discovered_files: unsigned(row.get(0)?)?,
                        reused_files: unsigned(row.get(1)?)?,
                        parsed_files: unsigned(row.get(2)?)?,
                        skipped_files: unsigned(row.get(3)?)?,
                        failed_files: unsigned(row.get(4)?)?,
                        processed_bytes: unsigned(row.get(5)?)?,
                        nodes: unsigned(row.get(6)?)?,
                        edges: unsigned(row.get(7)?)?,
                        diagnostics: unsigned(row.get(8)?)?,
                        duration_ms: unsigned(row.get(9)?)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn snapshot_fact_counts(
        &self,
        snapshot: &super::domain::SnapshotId,
    ) -> Result<(u64, u64, u64), StoreError> {
        let counts = self.connection().query_row(
            "SELECT \
                (SELECT COUNT(*) FROM files WHERE snapshot_id = ?1), \
                (SELECT COUNT(*) FROM nodes WHERE snapshot_id = ?1), \
                (SELECT COUNT(*) FROM edges WHERE snapshot_id = ?1)",
            [snapshot.as_str()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )?;
        Ok((
            unsigned(counts.0)?,
            unsigned(counts.1)?,
            unsigned(counts.2)?,
        ))
    }
}

fn insert_snapshot(
    transaction: &Transaction<'_>,
    snapshot: &GraphSnapshot,
) -> Result<(), StoreError> {
    transaction.execute(
        "INSERT INTO snapshots(\
            id, repository_namespace, repository_id, source_revision_id, \
            source_manifest_algorithm, source_manifest_digest, graph_model_version, \
            analysis_config_algorithm, analysis_config_digest, extractor_set_algorithm, \
            extractor_set_digest, completed_by_build_id, created_at\
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            snapshot.id.as_str(),
            snapshot.repository.namespace.as_str(),
            snapshot.repository.repository_id.as_str(),
            snapshot.source_revision_id.as_str(),
            snapshot.source_manifest_digest.algorithm(),
            snapshot.source_manifest_digest.value(),
            snapshot.graph_model_version,
            snapshot.analysis_config_digest.algorithm(),
            snapshot.analysis_config_digest.value(),
            snapshot.extractor_set_digest.algorithm(),
            snapshot.extractor_set_digest.value(),
            snapshot.completed_by.as_str(),
            timestamp(),
        ],
    )?;
    Ok(())
}

fn insert_files(
    transaction: &Transaction<'_>,
    snapshot: &GraphSnapshot,
    files: &[SourceFileDescriptor],
) -> Result<(), StoreError> {
    let mut statement = transaction.prepare(
        "INSERT INTO files(\
            snapshot_id, path, content_algorithm, content_digest, byte_length, file_mode, language\
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)",
    )?;
    for file in files {
        statement.execute(params![
            snapshot.id.as_str(),
            file.path.as_str(),
            file.content_identity.algorithm(),
            file.content_identity.value(),
            integer(file.byte_len, "file byte length")?,
            file_mode(file.file_mode),
        ])?;
    }
    Ok(())
}

fn insert_nodes(transaction: &Transaction<'_>, nodes: &[GraphNode]) -> Result<(), StoreError> {
    let mut statement = transaction.prepare(
        "INSERT INTO nodes(\
            snapshot_id, id, kind, semantic_key, extractor_id, extractor_version, \
            extractor_contract_version, resolution_state, confidence, evidence_path, \
            evidence_content_algorithm, evidence_content_digest, span_start_byte, span_end_byte, \
            properties_json, span_start_line, span_start_column, span_end_line, span_end_column, \
            normalized_name\
         ) VALUES (\
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, \
            ?16, ?17, ?18, ?19, ?20\
         )",
    )?;
    for node in nodes {
        let evidence = EvidenceColumns::new(node.provenance.evidence.as_ref())?;
        statement.execute(params![
            node.snapshot_id.as_str(),
            node.id.as_str(),
            node.kind,
            node.semantic_key.as_ref().map(|key| key.as_str()),
            node.provenance.extractor.id.as_str(),
            node.provenance.extractor.version,
            i64::from(node.provenance.extractor.contract_version),
            resolution_name(node.provenance.resolution),
            confidence_name(node.provenance.confidence),
            evidence.path,
            evidence.algorithm,
            evidence.digest,
            evidence.start_byte,
            evidence.end_byte,
            serde_json::to_string(&node.properties)?,
            evidence.start_line,
            evidence.start_column,
            evidence.end_line,
            evidence.end_column,
            normalized_node_name(node),
        ])?;
    }
    Ok(())
}

fn normalized_node_name(node: &GraphNode) -> String {
    node.properties
        .get("name")
        .or_else(|| node.properties.get("path"))
        .and_then(|value| match value {
            GraphValue::String(value) => Some(value.as_str()),
            _ => None,
        })
        .or_else(|| node.semantic_key.as_ref().map(|key| key.as_str()))
        .unwrap_or(&node.kind)
        .to_lowercase()
}

fn insert_edges(transaction: &Transaction<'_>, edges: &[GraphEdge]) -> Result<(), StoreError> {
    let mut statement = transaction.prepare(
        "INSERT INTO edges(\
            snapshot_id, id, kind, source_node_id, target_node_id, external_target, \
            extractor_id, extractor_version, extractor_contract_version, resolution_state, \
            confidence, evidence_path, evidence_content_algorithm, evidence_content_digest, \
            span_start_byte, span_end_byte, properties_json, span_start_line, span_start_column, \
            span_end_line, span_end_column\
         ) VALUES (\
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, \
            ?16, ?17, ?18, ?19, ?20, ?21\
         )",
    )?;
    for edge in edges {
        let evidence = EvidenceColumns::new(edge.provenance.evidence.as_ref())?;
        let (target_node, external_target) = match &edge.target {
            EdgeTarget::Node(target) => (Some(target.as_str()), None),
            EdgeTarget::External(target) | EdgeTarget::Unresolved(target) => {
                (None, Some(target.as_str()))
            }
        };
        statement.execute(params![
            edge.snapshot_id.as_str(),
            edge.id.as_str(),
            edge.kind,
            edge.source.as_str(),
            target_node,
            external_target,
            edge.provenance.extractor.id.as_str(),
            edge.provenance.extractor.version,
            i64::from(edge.provenance.extractor.contract_version),
            resolution_name(edge.provenance.resolution),
            confidence_name(edge.provenance.confidence),
            evidence.path,
            evidence.algorithm,
            evidence.digest,
            evidence.start_byte,
            evidence.end_byte,
            serde_json::to_string(&edge.properties)?,
            evidence.start_line,
            evidence.start_column,
            evidence.end_line,
            evidence.end_column,
        ])?;
    }
    Ok(())
}

fn insert_diagnostics(
    transaction: &Transaction<'_>,
    diagnostics: &[GraphDiagnostic],
) -> Result<(), StoreError> {
    let mut statement = transaction.prepare(
        "INSERT INTO diagnostics(\
            build_id, snapshot_id, severity, code, message, path, span_start_byte, \
            span_start_line, span_start_column, span_end_byte, span_end_line, \
            span_end_column, metadata_json, created_at\
         ) VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
    )?;
    for diagnostic in diagnostics {
        // Source diagnostics may name paths that were deliberately omitted from
        // `files` (for example sensitive, binary, symlink, or runtime paths).
        let (path, span) = diagnostic
            .location
            .as_ref()
            .map_or((None, None), |location| {
                (Some(location.path.as_str()), location.span.as_ref())
            });
        let span = SpanColumns::new(span)?;
        statement.execute(params![
            diagnostic.build_id.as_str(),
            diagnostic.snapshot_id.as_ref().map(|id| id.as_str()),
            severity_name(diagnostic.severity),
            diagnostic.code.as_str(),
            path,
            span.start_byte,
            span.start_line,
            span.start_column,
            span.end_byte,
            span.end_line,
            span.end_column,
            serde_json::to_string(&diagnostic.metrics)?,
            timestamp(),
        ])?;
    }
    Ok(())
}

fn replace_snapshot_diagnostics(
    transaction: &Transaction<'_>,
    snapshot: &GraphSnapshot,
    diagnostics: &[GraphDiagnostic],
) -> Result<(), StoreError> {
    if diagnostics.iter().any(|diagnostic| {
        diagnostic.build_id != snapshot.completed_by
            || diagnostic.snapshot_id.as_ref() != Some(&snapshot.id)
    }) {
        return Err(StoreError::IdentityMismatch("snapshot diagnostics"));
    }
    transaction.execute(
        "DELETE FROM diagnostics WHERE build_id = ?1 AND snapshot_id = ?2",
        params![snapshot.completed_by.as_str(), snapshot.id.as_str()],
    )?;
    insert_diagnostics(transaction, diagnostics)?;
    transaction.execute(
        "INSERT INTO snapshot_diagnostic_sets(snapshot_id, build_id) VALUES (?1, ?2) \
         ON CONFLICT(snapshot_id) DO UPDATE SET build_id = excluded.build_id",
        params![snapshot.id.as_str(), snapshot.completed_by.as_str()],
    )?;
    Ok(())
}

fn upsert_cached_fragment(
    transaction: &Transaction<'_>,
    key: &FragmentCacheKey,
    fragment: &GraphFragment,
) -> Result<(), StoreError> {
    let now = timestamp();
    transaction
        .prepare_cached(
            "INSERT INTO fragment_cache(\
            repository_namespace, repository_id, path, content_algorithm, content_digest, \
            byte_length, file_mode, analysis_config_algorithm, analysis_config_digest, \
            extractor_id, extractor_version, extractor_contract_version, fragment_json, \
            created_at, last_used_at\
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14) \
         ON CONFLICT DO UPDATE SET fragment_json = excluded.fragment_json, \
            last_used_at = excluded.last_used_at",
        )?
        .execute(params![
            key.repository.namespace.as_str(),
            key.repository.repository_id.as_str(),
            key.path.as_str(),
            key.content_identity.algorithm(),
            key.content_identity.value(),
            integer(key.byte_len, "fragment byte length")?,
            file_mode(key.file_mode),
            key.analysis_config_digest.algorithm(),
            key.analysis_config_digest.value(),
            key.extractor.id.as_str(),
            key.extractor.version,
            i64::from(key.extractor.contract_version),
            serde_json::to_string(fragment)?,
            now,
        ])?;
    Ok(())
}

fn touch_cached_fragments(
    transaction: &Transaction<'_>,
    hits: &[FragmentCacheKey],
) -> Result<(), StoreError> {
    if hits.is_empty() {
        return Ok(());
    }
    let now = timestamp();
    let mut statement = transaction.prepare_cached(
        "UPDATE fragment_cache SET last_used_at = ?13 WHERE \
         repository_namespace = ?1 AND repository_id = ?2 AND path = ?3 AND \
         content_algorithm = ?4 AND content_digest = ?5 AND byte_length = ?6 AND \
         file_mode = ?7 AND analysis_config_algorithm = ?8 AND \
         analysis_config_digest = ?9 AND extractor_id = ?10 AND \
         extractor_version = ?11 AND extractor_contract_version = ?12",
    )?;
    for key in hits {
        statement.execute(params![
            key.repository.namespace.as_str(),
            key.repository.repository_id.as_str(),
            key.path.as_str(),
            key.content_identity.algorithm(),
            key.content_identity.value(),
            integer(key.byte_len, "fragment byte length")?,
            file_mode(key.file_mode),
            key.analysis_config_digest.algorithm(),
            key.analysis_config_digest.value(),
            key.extractor.id.as_str(),
            key.extractor.version,
            i64::from(key.extractor.contract_version),
            now,
        ])?;
    }
    Ok(())
}

fn upsert_metrics(
    connection: &rusqlite::Connection,
    build_id: &BuildId,
    metrics: &IndexBuildMetrics,
) -> Result<(), StoreError> {
    connection.execute(
        "INSERT INTO build_metrics(\
            build_id, discovered_files, reused_files, parsed_files, skipped_files, \
            failed_files, processed_bytes, nodes, edges, diagnostics, duration_ms\
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
         ON CONFLICT(build_id) DO UPDATE SET \
            discovered_files = excluded.discovered_files, reused_files = excluded.reused_files, \
            parsed_files = excluded.parsed_files, skipped_files = excluded.skipped_files, \
            failed_files = excluded.failed_files, processed_bytes = excluded.processed_bytes, \
            nodes = excluded.nodes, edges = excluded.edges, diagnostics = excluded.diagnostics, \
            duration_ms = excluded.duration_ms",
        params![
            build_id.as_str(),
            integer(metrics.discovered_files, "discovered file count")?,
            integer(metrics.reused_files, "reused file count")?,
            integer(metrics.parsed_files, "parsed file count")?,
            integer(metrics.skipped_files, "skipped file count")?,
            integer(metrics.failed_files, "failed file count")?,
            integer(metrics.processed_bytes, "processed byte count")?,
            integer(metrics.nodes, "node count")?,
            integer(metrics.edges, "edge count")?,
            integer(metrics.diagnostics, "diagnostic count")?,
            integer(metrics.duration_ms, "duration")?,
        ],
    )?;
    Ok(())
}

fn cache_params(key: &FragmentCacheKey) -> Result<[rusqlite::types::Value; 12], StoreError> {
    use rusqlite::types::Value;
    Ok([
        Value::Text(key.repository.namespace.as_str().to_string()),
        Value::Text(key.repository.repository_id.as_str().to_string()),
        Value::Text(key.path.as_str().to_string()),
        Value::Text(key.content_identity.algorithm().to_string()),
        Value::Text(key.content_identity.value().to_string()),
        Value::Integer(integer(key.byte_len, "fragment byte length")?),
        Value::Integer(file_mode(key.file_mode)),
        Value::Text(key.analysis_config_digest.algorithm().to_string()),
        Value::Text(key.analysis_config_digest.value().to_string()),
        Value::Text(key.extractor.id.as_str().to_string()),
        Value::Text(key.extractor.version.clone()),
        Value::Integer(i64::from(key.extractor.contract_version)),
    ])
}

struct EvidenceColumns<'a> {
    path: Option<&'a str>,
    algorithm: Option<&'a str>,
    digest: Option<&'a str>,
    start_byte: Option<i64>,
    start_line: Option<i64>,
    start_column: Option<i64>,
    end_byte: Option<i64>,
    end_line: Option<i64>,
    end_column: Option<i64>,
}

impl<'a> EvidenceColumns<'a> {
    fn new(evidence: Option<&'a SourceEvidence>) -> Result<Self, StoreError> {
        let Some(evidence) = evidence else {
            return Ok(Self {
                path: None,
                algorithm: None,
                digest: None,
                start_byte: None,
                start_line: None,
                start_column: None,
                end_byte: None,
                end_line: None,
                end_column: None,
            });
        };
        let span = SpanColumns::new(evidence.span.as_ref())?;
        Ok(Self {
            path: Some(evidence.path.as_str()),
            algorithm: Some(evidence.content_identity.algorithm()),
            digest: Some(evidence.content_identity.value()),
            start_byte: span.start_byte,
            start_line: span.start_line,
            start_column: span.start_column,
            end_byte: span.end_byte,
            end_line: span.end_line,
            end_column: span.end_column,
        })
    }
}

struct SpanColumns {
    start_byte: Option<i64>,
    start_line: Option<i64>,
    start_column: Option<i64>,
    end_byte: Option<i64>,
    end_line: Option<i64>,
    end_column: Option<i64>,
}

impl SpanColumns {
    fn new(span: Option<&SourceSpan>) -> Result<Self, StoreError> {
        let Some(span) = span else {
            return Ok(Self {
                start_byte: None,
                start_line: None,
                start_column: None,
                end_byte: None,
                end_line: None,
                end_column: None,
            });
        };
        Ok(Self {
            start_byte: Some(integer(span.start.byte_offset, "span start")?),
            start_line: span.start.line.map(i64::from),
            start_column: span.start.column.map(i64::from),
            end_byte: Some(integer(span.end.byte_offset, "span end")?),
            end_line: span.end.line.map(i64::from),
            end_column: span.end.column.map(i64::from),
        })
    }
}

fn resolution_name(value: ResolutionState) -> &'static str {
    match value {
        ResolutionState::Resolved => "resolved",
        ResolutionState::Unresolved => "unresolved",
        ResolutionState::External => "external",
    }
}

fn confidence_name(value: Confidence) -> &'static str {
    match value {
        Confidence::Exact => "exact",
        Confidence::High => "high",
        Confidence::Medium => "medium",
        Confidence::Low => "low",
    }
}

fn severity_name(value: DiagnosticSeverity) -> &'static str {
    match value {
        DiagnosticSeverity::Info => "info",
        DiagnosticSeverity::Warning => "warning",
        DiagnosticSeverity::Error => "error",
    }
}

fn file_mode(value: SourceFileMode) -> i64 {
    match value {
        SourceFileMode::Regular => 0,
        SourceFileMode::Executable => 1,
    }
}

fn integer(value: u64, name: &'static str) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::Corrupt(format!("{name} exceeds SQLite range")))
}

fn unsigned(value: i64) -> Result<u64, rusqlite::Error> {
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}
