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
        let (path, span_start, span_end) = if let Some(location) = diagnostic.location.as_ref() {
            let start = location
                .span
                .as_ref()
                .map(|span| i64::try_from(span.start.byte_offset))
                .transpose()
                .map_err(|_| DiagnosticStoreError::InvalidSpan)?;
            let end = location
                .span
                .as_ref()
                .map(|span| i64::try_from(span.end.byte_offset))
                .transpose()
                .map_err(|_| DiagnosticStoreError::InvalidSpan)?;
            (Some(location.path.as_str()), start, end)
        } else {
            (None, None, None)
        };
        let metrics = serde_json::to_string(&diagnostic.metrics)?;
        self.connection_mut().execute(
            "INSERT INTO diagnostics(\
                build_id, snapshot_id, severity, code, message, path, span_start_byte, \
                span_end_byte, metadata_json, created_at\
             ) VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                diagnostic.build_id.as_str(),
                diagnostic.snapshot_id.as_ref().map(SnapshotId::as_str),
                severity_name(diagnostic.severity),
                diagnostic.code.as_str(),
                path,
                span_start,
                span_end,
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
            "SELECT snapshot_id, severity, code, path, span_start_byte, span_end_byte, metadata_json \
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
                row.get::<_, String>(6)?,
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
    String,
);

fn diagnostic_from_row(
    build_id: &BuildId,
    row: DiagnosticRow,
) -> Result<GraphDiagnostic, DiagnosticStoreError> {
    let (snapshot_id, severity, code, path, span_start, span_end, metrics) = row;
    let span = match (span_start, span_end) {
        (None, None) => None,
        (Some(start), Some(end)) if start >= 0 && end >= start => Some(SourceSpan {
            start: SourcePosition {
                byte_offset: start as u64,
                line: None,
                column: None,
            },
            end: SourcePosition {
                byte_offset: end as u64,
                line: None,
                column: None,
            },
        }),
        _ => return Err(DiagnosticStoreError::InvalidSpan),
    };
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
                span: None,
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
