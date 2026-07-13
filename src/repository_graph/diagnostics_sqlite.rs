//! SQLite persistence adapter for content-free graph diagnostics.

use std::collections::BTreeMap;

use rusqlite::params;
use thiserror::Error;

use super::{
    domain::{
        BuildId, DiagnosticCode, DiagnosticLocation, DiagnosticSeverity, GraphDiagnostic, RepoPath,
        SnapshotId, SourcePosition, SourceSpan,
    },
    sqlite::Sidecar,
};

#[derive(Debug, Error)]
pub enum DiagnosticStoreError {
    #[error("repository graph diagnostic contains inconsistent optional span fields")]
    InvalidSpan,
    #[error("repository graph diagnostic row contains invalid data: {0}")]
    Corrupt(String),
    #[error(transparent)]
    Database(#[from] rusqlite::Error),
    #[error(transparent)]
    Serialization(#[from] serde_json::Error),
}

impl Sidecar {
    pub fn record_diagnostic(
        &mut self,
        diagnostic: &GraphDiagnostic,
    ) -> Result<(), DiagnosticStoreError> {
        let (path, start_byte, start_line, start_column, end_byte, end_line, end_column) =
            if let Some(location) = diagnostic.location.as_ref() {
                let start_byte = location
                    .span
                    .as_ref()
                    .map(|span| i64::try_from(span.start.byte_offset))
                    .transpose()
                    .map_err(|_| DiagnosticStoreError::InvalidSpan)?;
                let end_byte = location
                    .span
                    .as_ref()
                    .map(|span| i64::try_from(span.end.byte_offset))
                    .transpose()
                    .map_err(|_| DiagnosticStoreError::InvalidSpan)?;
                let start_line = location
                    .span
                    .as_ref()
                    .and_then(|span| span.start.line)
                    .map(i64::from);
                let start_column = location
                    .span
                    .as_ref()
                    .and_then(|span| span.start.column)
                    .map(i64::from);
                let end_line = location
                    .span
                    .as_ref()
                    .and_then(|span| span.end.line)
                    .map(i64::from);
                let end_column = location
                    .span
                    .as_ref()
                    .and_then(|span| span.end.column)
                    .map(i64::from);
                (
                    Some(location.path.as_str()),
                    start_byte,
                    start_line,
                    start_column,
                    end_byte,
                    end_line,
                    end_column,
                )
            } else {
                (None, None, None, None, None, None, None)
            };
        let metrics = serde_json::to_string(&diagnostic.metrics)?;
        self.connection_mut().execute(
            "INSERT INTO diagnostics(\
                build_id, snapshot_id, severity, code, message, path, span_start_byte, \
                span_start_line, span_start_column, span_end_byte, span_end_line, \
                span_end_column, metadata_json, created_at\
             ) VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                diagnostic.build_id.as_str(),
                diagnostic.snapshot_id.as_ref().map(SnapshotId::as_str),
                severity_name(diagnostic.severity),
                diagnostic.code.as_str(),
                path,
                start_byte,
                start_line,
                start_column,
                end_byte,
                end_line,
                end_column,
                metrics,
                timestamp(),
            ],
        )?;
        Ok(())
    }

    pub fn diagnostics_for_build(
        &self,
        build_id: &BuildId,
    ) -> Result<Vec<GraphDiagnostic>, DiagnosticStoreError> {
        let mut statement = self.connection().prepare(
            "SELECT snapshot_id, severity, code, path, span_start_byte, span_start_line, \
                    span_start_column, span_end_byte, span_end_line, span_end_column, metadata_json \
             FROM diagnostics WHERE build_id = ?1 ORDER BY id",
        )?;
        let rows = statement.query_map([build_id.as_str()], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, Option<i64>>(7)?,
                row.get::<_, Option<i64>>(8)?,
                row.get::<_, Option<i64>>(9)?,
                row.get::<_, String>(10)?,
            ))
        })?;
        rows.map(|row| diagnostic_from_row(build_id, row?))
            .collect()
    }
}

type DiagnosticRow = (
    Option<String>,
    String,
    String,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    String,
);

fn diagnostic_from_row(
    build_id: &BuildId,
    row: DiagnosticRow,
) -> Result<GraphDiagnostic, DiagnosticStoreError> {
    let (
        snapshot_id,
        severity,
        code,
        path,
        start_byte,
        start_line,
        start_column,
        end_byte,
        end_line,
        end_column,
        metrics,
    ) = row;
    let span = span_from_row(
        start_byte,
        start_line,
        start_column,
        end_byte,
        end_line,
        end_column,
    )?;
    let location = match (path, span) {
        (Some(path), span) => Some(DiagnosticLocation {
            path: RepoPath::new(path)
                .map_err(|error| DiagnosticStoreError::Corrupt(error.to_string()))?,
            span,
        }),
        (None, None) => None,
        (None, Some(_)) => return Err(DiagnosticStoreError::InvalidSpan),
    };
    Ok(GraphDiagnostic {
        build_id: build_id.clone(),
        snapshot_id: snapshot_id
            .map(SnapshotId::new)
            .transpose()
            .map_err(|error| DiagnosticStoreError::Corrupt(error.to_string()))?,
        severity: parse_severity(&severity)?,
        code: DiagnosticCode::new(code)
            .map_err(|error| DiagnosticStoreError::Corrupt(error.to_string()))?,
        location,
        metrics: serde_json::from_str::<BTreeMap<DiagnosticCode, i64>>(&metrics)?,
    })
}

fn span_from_row(
    start_byte: Option<i64>,
    start_line: Option<i64>,
    start_column: Option<i64>,
    end_byte: Option<i64>,
    end_line: Option<i64>,
    end_column: Option<i64>,
) -> Result<Option<SourceSpan>, DiagnosticStoreError> {
    match (start_byte, end_byte) {
        (None, None)
            if start_line.is_none()
                && start_column.is_none()
                && end_line.is_none()
                && end_column.is_none() =>
        {
            Ok(None)
        }
        (Some(start_byte), Some(end_byte)) if end_byte >= start_byte => Ok(Some(SourceSpan {
            start: source_position(start_byte, start_line, start_column)?,
            end: source_position(end_byte, end_line, end_column)?,
        })),
        _ => Err(DiagnosticStoreError::InvalidSpan),
    }
}

fn source_position(
    byte_offset: i64,
    line: Option<i64>,
    column: Option<i64>,
) -> Result<SourcePosition, DiagnosticStoreError> {
    Ok(SourcePosition {
        byte_offset: u64::try_from(byte_offset).map_err(|_| DiagnosticStoreError::InvalidSpan)?,
        line: line
            .map(u32::try_from)
            .transpose()
            .map_err(|_| DiagnosticStoreError::InvalidSpan)?,
        column: column
            .map(u32::try_from)
            .transpose()
            .map_err(|_| DiagnosticStoreError::InvalidSpan)?,
    })
}

fn severity_name(severity: DiagnosticSeverity) -> &'static str {
    match severity {
        DiagnosticSeverity::Info => "info",
        DiagnosticSeverity::Warning => "warning",
        DiagnosticSeverity::Error => "error",
    }
}

fn parse_severity(value: &str) -> Result<DiagnosticSeverity, DiagnosticStoreError> {
    match value {
        "info" => Ok(DiagnosticSeverity::Info),
        "warning" => Ok(DiagnosticSeverity::Warning),
        "error" => Ok(DiagnosticSeverity::Error),
        _ => Err(DiagnosticStoreError::Corrupt(
            "unknown diagnostic severity".to_string(),
        )),
    }
}

fn timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository_graph::{
        domain::{
            BuildState, GraphBuild, RepositoryId, RepositoryNamespace, RepositoryRef,
            SourceRevisionId,
        },
        sqlite::{OpenSidecarResult, open_for_build_at},
    };

    #[test]
    fn diagnostic_round_trip_persists_only_code_location_and_numeric_metrics() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("repo-graph.db");
        let OpenSidecarResult::Ready(mut sidecar) = open_for_build_at(&path).unwrap() else {
            panic!("test sidecar unexpectedly requires rebuild");
        };
        let build = GraphBuild {
            id: BuildId::new("build-1").unwrap(),
            repository: RepositoryRef {
                namespace: RepositoryNamespace::new("local:test").unwrap(),
                repository_id: RepositoryId::new("root").unwrap(),
            },
            source_revision_id: SourceRevisionId::new("revision-1").unwrap(),
            prospective_snapshot_id: SnapshotId::new("snapshot-1").unwrap(),
            state: BuildState::Building,
        };
        sidecar.start_build(&build).unwrap();
        let diagnostic = GraphDiagnostic {
            build_id: build.id.clone(),
            snapshot_id: None,
            severity: DiagnosticSeverity::Warning,
            code: DiagnosticCode::new("path_encoding_unsupported").unwrap(),
            location: Some(DiagnosticLocation {
                path: RepoPath::new("src/main.rs").unwrap(),
                span: Some(SourceSpan {
                    start: SourcePosition {
                        byte_offset: 120,
                        line: Some(7),
                        column: Some(5),
                    },
                    end: SourcePosition {
                        byte_offset: 168,
                        line: Some(9),
                        column: None,
                    },
                }),
            }),
            metrics: BTreeMap::from([(DiagnosticCode::new("skipped_files").unwrap(), 1)]),
        };

        sidecar.record_diagnostic(&diagnostic).unwrap();
        assert_eq!(
            sidecar.diagnostics_for_build(&build.id).unwrap(),
            vec![diagnostic]
        );
        let stored_message: String = sidecar
            .connection()
            .query_row("SELECT message FROM diagnostics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(stored_message, "path_encoding_unsupported");
    }
}
