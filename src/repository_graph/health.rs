//! Read-only sidecar health inspection.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};

use super::{
    domain::Availability,
    sqlite::{SIDECAR_SCHEMA_VERSION, SidecarStatus, inspect_at},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    Absent,
    Healthy,
    Degraded,
    RequiresRebuild,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthIssueCode {
    SidecarUnreadable,
    IntegrityCheckFailed,
    ForeignKeyViolation,
    StatisticsUnavailable,
    IncompatibleSchema,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticCounts {
    pub info: u64,
    pub warning: u64,
    pub error: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarHealth {
    pub state: HealthState,
    pub availability: Availability,
    pub schema_version: Option<u32>,
    pub supported_schema_version: u32,
    pub published_views: u64,
    pub active_builds: u64,
    pub failed_builds: u64,
    pub diagnostics: DiagnosticCounts,
    pub issues: Vec<HealthIssueCode>,
}

impl SidecarHealth {
    fn absent() -> Self {
        Self {
            state: HealthState::Absent,
            availability: Availability::NotBuilt,
            schema_version: None,
            supported_schema_version: SIDECAR_SCHEMA_VERSION,
            published_views: 0,
            active_builds: 0,
            failed_builds: 0,
            diagnostics: DiagnosticCounts::default(),
            issues: vec![],
        }
    }

    fn unreadable() -> Self {
        Self {
            state: HealthState::Degraded,
            availability: Availability::Incompatible,
            schema_version: None,
            supported_schema_version: SIDECAR_SCHEMA_VERSION,
            published_views: 0,
            active_builds: 0,
            failed_builds: 0,
            diagnostics: DiagnosticCounts::default(),
            issues: vec![HealthIssueCode::SidecarUnreadable],
        }
    }
}

pub fn inspect_health_at(path: &Path) -> Result<SidecarHealth> {
    let status = match inspect_at(path) {
        Ok(status) => status,
        Err(_) if path.exists() => return Ok(SidecarHealth::unreadable()),
        Err(error) => return Err(error),
    };
    match status {
        SidecarStatus::Absent => Ok(SidecarHealth::absent()),
        SidecarStatus::RequiresRebuild(reason) => Ok(SidecarHealth {
            state: HealthState::RequiresRebuild,
            availability: Availability::Incompatible,
            schema_version: Some(reason.found_schema_version),
            supported_schema_version: reason.supported_schema_version,
            published_views: 0,
            active_builds: 0,
            failed_builds: 0,
            diagnostics: DiagnosticCounts::default(),
            issues: vec![HealthIssueCode::IncompatibleSchema],
        }),
        SidecarStatus::Ready { schema_version } => inspect_ready(path, schema_version),
    }
}

fn inspect_ready(path: &Path, schema_version: u32) -> Result<SidecarHealth> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| {
        format!(
            "Failed to inspect repository graph sidecar {}",
            path.display()
        )
    })?;
    let mut issues = Vec::new();
    let integrity = connection
        .pragma_query_value(None, "quick_check", |row| row.get::<_, String>(0))
        .unwrap_or_else(|_| "failed".to_string());
    if integrity != "ok" {
        issues.push(HealthIssueCode::IntegrityCheckFailed);
    }
    let foreign_key_violation = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_foreign_key_check)",
            [],
            |row| row.get::<_, bool>(0),
        )
        .unwrap_or(true);
    if foreign_key_violation {
        issues.push(HealthIssueCode::ForeignKeyViolation);
    }

    let statistics = read_statistics(&connection);
    let (published_views, active_builds, failed_builds, diagnostics) = match statistics {
        Ok(statistics) => statistics,
        Err(_) => {
            issues.push(HealthIssueCode::StatisticsUnavailable);
            (0, 0, 0, DiagnosticCounts::default())
        }
    };
    let availability = if published_views == 0 {
        Availability::NotBuilt
    } else {
        Availability::Available
    };
    Ok(SidecarHealth {
        state: if issues.is_empty() {
            HealthState::Healthy
        } else {
            HealthState::Degraded
        },
        availability,
        schema_version: Some(schema_version),
        supported_schema_version: SIDECAR_SCHEMA_VERSION,
        published_views,
        active_builds,
        failed_builds,
        diagnostics,
        issues,
    })
}

fn read_statistics(connection: &Connection) -> rusqlite::Result<(u64, u64, u64, DiagnosticCounts)> {
    let published_views = count(connection, "SELECT COUNT(*) FROM published_views")?;
    let active_builds = count(
        connection,
        "SELECT COUNT(*) FROM index_builds WHERE state = 'building' AND finished_at IS NULL",
    )?;
    let failed_builds = count(
        connection,
        "SELECT COUNT(*) FROM index_builds WHERE state = 'failed'",
    )?;
    let diagnostics = DiagnosticCounts {
        info: count(
            connection,
            "SELECT COUNT(*) FROM diagnostics WHERE severity = 'info'",
        )?,
        warning: count(
            connection,
            "SELECT COUNT(*) FROM diagnostics WHERE severity = 'warning'",
        )?,
        error: count(
            connection,
            "SELECT COUNT(*) FROM diagnostics WHERE severity = 'error'",
        )?,
    };
    Ok((published_views, active_builds, failed_builds, diagnostics))
}

fn count(connection: &Connection, sql: &str) -> rusqlite::Result<u64> {
    let value = connection.query_row(sql, [], |row| row.get::<_, i64>(0))?;
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository_graph::{
        domain::{
            BuildId, BuildState, DiagnosticCode, DiagnosticSeverity, GraphBuild, GraphDiagnostic,
            RepositoryId, RepositoryNamespace, RepositoryRef, SnapshotId, SourceRevisionId,
        },
        sqlite::{OpenSidecarResult, open_for_build_at},
    };

    #[test]
    fn absent_health_is_read_only_and_reports_not_built() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("repo-graph.db");
        let health = inspect_health_at(&path).unwrap();
        assert_eq!(health.state, HealthState::Absent);
        assert_eq!(health.availability, Availability::NotBuilt);
        assert!(!path.exists());
    }

    #[test]
    fn empty_compatible_sidecar_is_healthy_but_not_built() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("repo-graph.db");
        let OpenSidecarResult::Ready(sidecar) = open_for_build_at(&path).unwrap() else {
            panic!("test sidecar unexpectedly requires rebuild");
        };
        drop(sidecar);
        let health = inspect_health_at(&path).unwrap();
        assert_eq!(health.state, HealthState::Healthy);
        assert_eq!(health.availability, Availability::NotBuilt);
        assert_eq!(health.schema_version, Some(SIDECAR_SCHEMA_VERSION));
    }

    #[test]
    fn malformed_sidecar_reports_bounded_unreadable_issue() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("repo-graph.db");
        std::fs::write(&path, b"not sqlite and no source content").unwrap();
        let health = inspect_health_at(&path).unwrap();
        assert_eq!(health.state, HealthState::Degraded);
        assert_eq!(health.issues, vec![HealthIssueCode::SidecarUnreadable]);
    }

    #[test]
    fn health_reports_active_build_and_diagnostic_counts() {
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
        sidecar
            .record_diagnostic(&GraphDiagnostic {
                build_id: build.id,
                snapshot_id: None,
                severity: DiagnosticSeverity::Warning,
                code: DiagnosticCode::new("file_limit_reached").unwrap(),
                location: None,
                metrics: Default::default(),
            })
            .unwrap();
        drop(sidecar);

        let health = inspect_health_at(&path).unwrap();
        assert_eq!(health.state, HealthState::Healthy);
        assert_eq!(health.active_builds, 1);
        assert_eq!(health.diagnostics.warning, 1);
    }
}
