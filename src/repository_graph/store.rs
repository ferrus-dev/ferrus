//! SQLite implementation of immutable snapshot and publication lifecycle.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
    domain::{
        BuildId, BuildState, DiagnosticCode, Digest, GraphBuild, GraphSnapshot, PublishedViewName,
        RepositoryId, RepositoryNamespace, RepositoryRef, SnapshotId, SourceRevisionId,
    },
    ports::GraphStore,
    sqlite::Sidecar,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildFailure {
    pub build_id: BuildId,
    pub code: DiagnosticCode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicationVersion {
    pub snapshot_id: SnapshotId,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishRequest {
    pub repository: RepositoryRef,
    pub view_name: PublishedViewName,
    pub build_id: BuildId,
    pub expected: Option<PublicationVersion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishedView {
    pub repository: RepositoryRef,
    pub view_name: PublishedViewName,
    pub snapshot_id: SnapshotId,
    pub build_id: BuildId,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PublicationOutcome {
    Published { view: PublishedView },
    Superseded { current: PublishedView },
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("repository graph build {0} was not found")]
    BuildNotFound(String),
    #[error("repository graph snapshot {0} was not found")]
    SnapshotNotFound(String),
    #[error("invalid build transition from {state:?}: {operation}")]
    InvalidTransition {
        state: BuildState,
        operation: &'static str,
    },
    #[error("build and snapshot identities do not match: {0}")]
    IdentityMismatch(&'static str),
    #[error("publication compare-and-set failed")]
    PublicationConflict {
        expected: Option<PublicationVersion>,
        actual: Option<PublicationVersion>,
    },
    #[error("repository graph sidecar contains invalid data: {0}")]
    Corrupt(String),
    #[error(transparent)]
    Database(#[from] rusqlite::Error),
}

impl Sidecar {
    pub fn start_build(&mut self, build: &GraphBuild) -> Result<(), StoreError> {
        <Self as GraphStore>::start_build(self, build)
    }

    pub fn fail_build(&mut self, failure: &BuildFailure) -> Result<GraphBuild, StoreError> {
        <Self as GraphStore>::fail_build(self, failure)
    }

    pub fn complete_build(
        &mut self,
        snapshot: &GraphSnapshot,
    ) -> Result<GraphSnapshot, StoreError> {
        <Self as GraphStore>::complete_build(self, snapshot)
    }

    pub fn publish(&mut self, request: &PublishRequest) -> Result<PublicationOutcome, StoreError> {
        <Self as GraphStore>::publish(self, request)
    }

    pub fn supersede_build(&mut self, build_id: &BuildId) -> Result<GraphBuild, StoreError> {
        <Self as GraphStore>::supersede_build(self, build_id)
    }

    pub fn build(&self, id: &BuildId) -> Result<Option<GraphBuild>, StoreError> {
        <Self as GraphStore>::build(self, id)
    }

    pub fn snapshot(&self, id: &SnapshotId) -> Result<Option<GraphSnapshot>, StoreError> {
        <Self as GraphStore>::snapshot(self, id)
    }

    pub fn published_view(
        &self,
        repository: &RepositoryRef,
        name: &PublishedViewName,
    ) -> Result<Option<PublishedView>, StoreError> {
        <Self as GraphStore>::published_view(self, repository, name)
    }

    pub fn published_snapshot(
        &self,
        repository: &RepositoryRef,
        name: &PublishedViewName,
    ) -> Result<Option<GraphSnapshot>, StoreError> {
        let Some(view) = self.published_view(repository, name)? else {
            return Ok(None);
        };
        self.snapshot(&view.snapshot_id)
    }
}

impl GraphStore for Sidecar {
    type Error = StoreError;

    fn start_build(&mut self, build: &GraphBuild) -> Result<(), Self::Error> {
        if build.state != BuildState::Building {
            return Err(StoreError::InvalidTransition {
                state: build.state,
                operation: "start",
            });
        }
        self.connection_mut().execute(
            "INSERT INTO index_builds(\
                id, repository_namespace, repository_id, source_revision_id, \
                prospective_snapshot_id, state, started_at\
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'building', ?6)",
            params![
                build.id.as_str(),
                build.repository.namespace.as_str(),
                build.repository.repository_id.as_str(),
                build.source_revision_id.as_str(),
                build.prospective_snapshot_id.as_str(),
                timestamp(),
            ],
        )?;
        Ok(())
    }

    fn fail_build(&mut self, failure: &BuildFailure) -> Result<GraphBuild, Self::Error> {
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let build = load_build(&transaction, &failure.build_id)?
            .ok_or_else(|| StoreError::BuildNotFound(failure.build_id.as_str().to_string()))?;
        if build.state != BuildState::Building {
            return Err(StoreError::InvalidTransition {
                state: build.state,
                operation: "fail",
            });
        }
        transaction.execute(
            "UPDATE index_builds SET state = 'failed', finished_at = ?2, \
             failure_code = ?3, failure_message = NULL WHERE id = ?1",
            params![
                failure.build_id.as_str(),
                timestamp(),
                failure.code.as_str(),
            ],
        )?;
        transaction.commit()?;
        Ok(GraphBuild {
            state: BuildState::Failed,
            ..build
        })
    }

    fn complete_build(&mut self, snapshot: &GraphSnapshot) -> Result<GraphSnapshot, Self::Error> {
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let build = load_build(&transaction, &snapshot.completed_by)?
            .ok_or_else(|| StoreError::BuildNotFound(snapshot.completed_by.as_str().to_string()))?;
        if build.state != BuildState::Building {
            return Err(StoreError::InvalidTransition {
                state: build.state,
                operation: "complete",
            });
        }
        validate_snapshot_for_build(snapshot, &build)?;

        let completed = if let Some(existing) = load_snapshot(&transaction, &snapshot.id)? {
            validate_equivalent_snapshot(snapshot, &existing)?;
            existing
        } else {
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
            snapshot.clone()
        };
        transaction.execute(
            "UPDATE index_builds SET finished_at = ?2 WHERE id = ?1",
            params![build.id.as_str(), timestamp()],
        )?;
        transaction.commit()?;
        Ok(completed)
    }

    fn publish(&mut self, request: &PublishRequest) -> Result<PublicationOutcome, Self::Error> {
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let build = load_build(&transaction, &request.build_id)?
            .ok_or_else(|| StoreError::BuildNotFound(request.build_id.as_str().to_string()))?;
        if build.repository != request.repository {
            return Err(StoreError::IdentityMismatch("repository"));
        }
        if !matches!(build.state, BuildState::Complete | BuildState::Published) {
            return Err(StoreError::InvalidTransition {
                state: build.state,
                operation: "publish",
            });
        }
        let snapshot =
            load_snapshot(&transaction, &build.prospective_snapshot_id)?.ok_or_else(|| {
                StoreError::SnapshotNotFound(build.prospective_snapshot_id.as_str().to_string())
            })?;
        let current = load_published_view(&transaction, &request.repository, &request.view_name)?;

        let actual = current.as_ref().map(|view| PublicationVersion {
            snapshot_id: view.snapshot_id.clone(),
            generation: view.generation,
        });
        if request.expected != actual {
            return Err(StoreError::PublicationConflict {
                expected: request.expected.clone(),
                actual,
            });
        }

        if let Some(view) = current.as_ref()
            && view.build_id == build.id
            && view.snapshot_id == snapshot.id
        {
            transaction.commit()?;
            return Ok(PublicationOutcome::Published { view: view.clone() });
        }

        if let Some(view) = current.as_ref() {
            let candidate_order = build_order(&transaction, &build.id)?;
            let current_order = build_order(&transaction, &view.build_id)?;
            if candidate_order < current_order {
                transaction.execute(
                    "UPDATE index_builds SET state = 'superseded' WHERE id = ?1 AND state = 'building'",
                    [build.id.as_str()],
                )?;
                transaction.commit()?;
                return Ok(PublicationOutcome::Superseded {
                    current: view.clone(),
                });
            }
        }

        let generation = current.as_ref().map_or(1, |view| view.generation + 1);
        let stored_generation = i64::try_from(generation)
            .map_err(|_| StoreError::Corrupt("publication generation overflow".to_string()))?;
        if let Some(current) = current.as_ref() {
            let updated = transaction.execute(
                "UPDATE published_views SET snapshot_id = ?4, build_id = ?5, generation = ?6, \
                 published_at = ?7 WHERE repository_namespace = ?1 AND repository_id = ?2 \
                 AND view_name = ?3 AND snapshot_id = ?8 AND build_id = ?9 AND generation = ?10",
                params![
                    request.repository.namespace.as_str(),
                    request.repository.repository_id.as_str(),
                    request.view_name.as_str(),
                    snapshot.id.as_str(),
                    build.id.as_str(),
                    stored_generation,
                    timestamp(),
                    current.snapshot_id.as_str(),
                    current.build_id.as_str(),
                    i64::try_from(current.generation).map_err(|_| StoreError::Corrupt(
                        "publication generation overflow".to_string()
                    ))?,
                ],
            )?;
            if updated != 1 {
                return Err(StoreError::PublicationConflict {
                    expected: request.expected.clone(),
                    actual,
                });
            }
        } else {
            transaction.execute(
                "INSERT INTO published_views(\
                    repository_namespace, repository_id, view_name, snapshot_id, build_id, \
                    generation, published_at\
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    request.repository.namespace.as_str(),
                    request.repository.repository_id.as_str(),
                    request.view_name.as_str(),
                    snapshot.id.as_str(),
                    build.id.as_str(),
                    stored_generation,
                    timestamp(),
                ],
            )?;
        }
        transaction.execute(
            "UPDATE index_builds SET state = 'published' WHERE id = ?1",
            [build.id.as_str()],
        )?;
        let view = PublishedView {
            repository: request.repository.clone(),
            view_name: request.view_name.clone(),
            snapshot_id: snapshot.id,
            build_id: build.id,
            generation,
        };
        transaction.commit()?;
        Ok(PublicationOutcome::Published { view })
    }

    fn supersede_build(&mut self, build_id: &BuildId) -> Result<GraphBuild, Self::Error> {
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let build = load_build(&transaction, build_id)?
            .ok_or_else(|| StoreError::BuildNotFound(build_id.as_str().to_string()))?;
        if build.state != BuildState::Complete {
            return Err(StoreError::InvalidTransition {
                state: build.state,
                operation: "supersede",
            });
        }
        transaction.execute(
            "UPDATE index_builds SET state = 'superseded' WHERE id = ?1",
            [build_id.as_str()],
        )?;
        transaction.commit()?;
        Ok(GraphBuild {
            state: BuildState::Superseded,
            ..build
        })
    }

    fn build(&self, id: &BuildId) -> Result<Option<GraphBuild>, Self::Error> {
        load_build(self.connection(), id)
    }

    fn snapshot(&self, id: &SnapshotId) -> Result<Option<GraphSnapshot>, Self::Error> {
        load_snapshot(self.connection(), id)
    }

    fn published_view(
        &self,
        repository: &RepositoryRef,
        name: &PublishedViewName,
    ) -> Result<Option<PublishedView>, Self::Error> {
        load_published_view(self.connection(), repository, name)
    }
}

fn load_build(connection: &Connection, id: &BuildId) -> Result<Option<GraphBuild>, StoreError> {
    let raw = connection
        .query_row(
            "SELECT repository_namespace, repository_id, source_revision_id, \
                    prospective_snapshot_id, state, finished_at, \
                    EXISTS(SELECT 1 FROM snapshots WHERE id = prospective_snapshot_id) \
             FROM index_builds WHERE id = ?1",
            [id.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, bool>(6)?,
                ))
            },
        )
        .optional()?;
    let Some((
        namespace,
        repository_id,
        source_revision,
        snapshot,
        stored_state,
        finished,
        has_snapshot,
    )) = raw
    else {
        return Ok(None);
    };
    let state = match stored_state.as_str() {
        "building" if finished.is_none() => BuildState::Building,
        "building" if finished.is_some() && has_snapshot => BuildState::Complete,
        "published" => BuildState::Published,
        "failed" => BuildState::Failed,
        "superseded" => BuildState::Superseded,
        _ => {
            return Err(StoreError::Corrupt(format!(
                "invalid lifecycle state for build {}",
                id.as_str()
            )));
        }
    };
    Ok(Some(GraphBuild {
        id: id.clone(),
        repository: RepositoryRef {
            namespace: RepositoryNamespace::new(namespace)
                .map_err(|error| StoreError::Corrupt(error.to_string()))?,
            repository_id: RepositoryId::new(repository_id)
                .map_err(|error| StoreError::Corrupt(error.to_string()))?,
        },
        source_revision_id: SourceRevisionId::new(source_revision)
            .map_err(|error| StoreError::Corrupt(error.to_string()))?,
        prospective_snapshot_id: SnapshotId::new(snapshot)
            .map_err(|error| StoreError::Corrupt(error.to_string()))?,
        state,
    }))
}

#[allow(clippy::type_complexity)]
fn load_snapshot(
    connection: &Connection,
    id: &SnapshotId,
) -> Result<Option<GraphSnapshot>, StoreError> {
    let raw = connection
        .query_row(
            "SELECT repository_namespace, repository_id, source_revision_id, \
                    source_manifest_algorithm, source_manifest_digest, graph_model_version, \
                    analysis_config_algorithm, analysis_config_digest, extractor_set_algorithm, \
                    extractor_set_digest, completed_by_build_id \
             FROM snapshots WHERE id = ?1",
            [id.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, u32>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                ))
            },
        )
        .optional()?;
    let Some((
        namespace,
        repository_id,
        revision,
        manifest_algorithm,
        manifest_value,
        model,
        config_algorithm,
        config_value,
        extractor_algorithm,
        extractor_value,
        completed_by,
    )) = raw
    else {
        return Ok(None);
    };
    Ok(Some(GraphSnapshot {
        id: id.clone(),
        repository: RepositoryRef {
            namespace: RepositoryNamespace::new(namespace)
                .map_err(|error| StoreError::Corrupt(error.to_string()))?,
            repository_id: RepositoryId::new(repository_id)
                .map_err(|error| StoreError::Corrupt(error.to_string()))?,
        },
        source_revision_id: SourceRevisionId::new(revision)
            .map_err(|error| StoreError::Corrupt(error.to_string()))?,
        source_manifest_digest: Digest::new(manifest_algorithm, manifest_value)
            .map_err(|error| StoreError::Corrupt(error.to_string()))?,
        graph_model_version: model,
        analysis_config_digest: Digest::new(config_algorithm, config_value)
            .map_err(|error| StoreError::Corrupt(error.to_string()))?,
        extractor_set_digest: Digest::new(extractor_algorithm, extractor_value)
            .map_err(|error| StoreError::Corrupt(error.to_string()))?,
        completed_by: BuildId::new(completed_by)
            .map_err(|error| StoreError::Corrupt(error.to_string()))?,
    }))
}

fn load_published_view(
    connection: &Connection,
    repository: &RepositoryRef,
    name: &PublishedViewName,
) -> Result<Option<PublishedView>, StoreError> {
    let raw = connection
        .query_row(
            "SELECT snapshot_id, build_id, generation FROM published_views \
             WHERE repository_namespace = ?1 AND repository_id = ?2 AND view_name = ?3",
            params![
                repository.namespace.as_str(),
                repository.repository_id.as_str(),
                name.as_str()
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((snapshot_id, build_id, stored_generation)) = raw else {
        return Ok(None);
    };
    let generation = u64::try_from(stored_generation)
        .map_err(|_| StoreError::Corrupt("negative publication generation".to_string()))?;
    Ok(Some(PublishedView {
        repository: repository.clone(),
        view_name: name.clone(),
        snapshot_id: SnapshotId::new(snapshot_id)
            .map_err(|error| StoreError::Corrupt(error.to_string()))?,
        build_id: BuildId::new(build_id).map_err(|error| StoreError::Corrupt(error.to_string()))?,
        generation,
    }))
}

fn build_order(connection: &Connection, id: &BuildId) -> Result<i64, StoreError> {
    connection
        .query_row(
            "SELECT rowid FROM index_builds WHERE id = ?1",
            [id.as_str()],
            |row| row.get(0),
        )
        .map_err(StoreError::from)
}

fn validate_snapshot_for_build(
    snapshot: &GraphSnapshot,
    build: &GraphBuild,
) -> Result<(), StoreError> {
    if snapshot.id != build.prospective_snapshot_id {
        return Err(StoreError::IdentityMismatch("snapshot id"));
    }
    if snapshot.repository != build.repository {
        return Err(StoreError::IdentityMismatch("repository"));
    }
    if snapshot.source_revision_id != build.source_revision_id {
        return Err(StoreError::IdentityMismatch("source revision"));
    }
    Ok(())
}

fn validate_equivalent_snapshot(
    requested: &GraphSnapshot,
    existing: &GraphSnapshot,
) -> Result<(), StoreError> {
    if requested.id != existing.id
        || requested.repository != existing.repository
        || requested.source_revision_id != existing.source_revision_id
        || requested.source_manifest_digest != existing.source_manifest_digest
        || requested.graph_model_version != existing.graph_model_version
        || requested.analysis_config_digest != existing.analysis_config_digest
        || requested.extractor_set_digest != existing.extractor_set_digest
    {
        return Err(StoreError::IdentityMismatch("existing snapshot contents"));
    }
    Ok(())
}

fn timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository_graph::sqlite::{OpenSidecarResult, open_for_build_at};

    fn repository() -> RepositoryRef {
        RepositoryRef {
            namespace: RepositoryNamespace::new("local:test-project").unwrap(),
            repository_id: RepositoryId::new("root").unwrap(),
        }
    }

    fn digest(value: &str) -> Digest {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = String::with_capacity(value.len() * 2);
        for byte in value.bytes() {
            encoded.push(HEX[(byte >> 4) as usize] as char);
            encoded.push(HEX[(byte & 0x0f) as usize] as char);
        }
        Digest::new("sha256", encoded).unwrap()
    }

    fn build(number: u32) -> GraphBuild {
        GraphBuild {
            id: BuildId::new(format!("build-{number}")).unwrap(),
            repository: repository(),
            source_revision_id: SourceRevisionId::new(format!("revision-{number}")).unwrap(),
            prospective_snapshot_id: SnapshotId::new(format!("snapshot-{number}")).unwrap(),
            state: BuildState::Building,
        }
    }

    fn snapshot(build: &GraphBuild) -> GraphSnapshot {
        GraphSnapshot {
            id: build.prospective_snapshot_id.clone(),
            repository: build.repository.clone(),
            source_revision_id: build.source_revision_id.clone(),
            source_manifest_digest: digest(&format!("manifest-{}", build.id.as_str())),
            graph_model_version: 1,
            analysis_config_digest: digest("config"),
            extractor_set_digest: digest("extractors"),
            completed_by: build.id.clone(),
        }
    }

    fn sidecar() -> (tempfile::TempDir, Sidecar) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("repo-graph.db");
        let OpenSidecarResult::Ready(sidecar) = open_for_build_at(&path).unwrap() else {
            panic!("test sidecar unexpectedly requires rebuild");
        };
        (directory, sidecar)
    }

    fn canonical() -> PublishedViewName {
        PublishedViewName::new("canonical").unwrap()
    }

    #[test]
    fn partial_and_completed_builds_are_invisible_until_publication() {
        let (_directory, mut sidecar) = sidecar();
        let build = build(1);
        sidecar.start_build(&build).unwrap();
        assert_eq!(
            sidecar.build(&build.id).unwrap().unwrap().state,
            BuildState::Building
        );
        assert!(
            sidecar
                .published_snapshot(&repository(), &canonical())
                .unwrap()
                .is_none()
        );

        let completed = sidecar.complete_build(&snapshot(&build)).unwrap();
        assert_eq!(
            sidecar.build(&build.id).unwrap().unwrap().state,
            BuildState::Complete
        );
        assert!(
            sidecar
                .published_snapshot(&repository(), &canonical())
                .unwrap()
                .is_none()
        );

        let outcome = sidecar
            .publish(&PublishRequest {
                repository: repository(),
                view_name: canonical(),
                build_id: build.id.clone(),
                expected: None,
            })
            .unwrap();
        let PublicationOutcome::Published { view } = outcome else {
            panic!("first publication was unexpectedly superseded");
        };
        assert_eq!(view.generation, 1);
        assert_eq!(
            sidecar
                .published_snapshot(&repository(), &canonical())
                .unwrap(),
            Some(completed)
        );
    }

    #[test]
    fn failed_build_does_not_replace_last_published_snapshot() {
        let (_directory, mut sidecar) = sidecar();
        let first = build(1);
        sidecar.start_build(&first).unwrap();
        sidecar.complete_build(&snapshot(&first)).unwrap();
        sidecar
            .publish(&PublishRequest {
                repository: repository(),
                view_name: canonical(),
                build_id: first.id.clone(),
                expected: None,
            })
            .unwrap();

        let failed = build(2);
        sidecar.start_build(&failed).unwrap();
        let result = sidecar
            .fail_build(&BuildFailure {
                build_id: failed.id.clone(),
                code: DiagnosticCode::new("extractor_failed").unwrap(),
            })
            .unwrap();
        assert_eq!(result.state, BuildState::Failed);
        let stored_failure: (String, Option<String>) = sidecar
            .connection()
            .query_row(
                "SELECT failure_code, failure_message FROM index_builds WHERE id = ?1",
                [failed.id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(stored_failure, ("extractor_failed".to_string(), None));
        assert_eq!(
            sidecar
                .published_view(&repository(), &canonical())
                .unwrap()
                .unwrap()
                .build_id,
            first.id
        );
    }

    #[test]
    fn build_failure_wire_contract_rejects_free_form_details() {
        let with_message = serde_json::json!({
            "build_id": "build-1",
            "code": "extractor_failed",
            "message": "parser failed near source text /absolute/path token=secret"
        });
        assert!(serde_json::from_value::<BuildFailure>(with_message).is_err());

        let invalid_code = serde_json::json!({
            "build_id": "build-1",
            "code": "token=secret"
        });
        assert!(serde_json::from_value::<BuildFailure>(invalid_code).is_err());
    }

    #[test]
    fn older_build_cannot_overwrite_a_newer_publication() {
        let (_directory, mut sidecar) = sidecar();
        let older = build(1);
        let newer = build(2);
        sidecar.start_build(&older).unwrap();
        sidecar.start_build(&newer).unwrap();
        sidecar.complete_build(&snapshot(&newer)).unwrap();
        sidecar
            .publish(&PublishRequest {
                repository: repository(),
                view_name: canonical(),
                build_id: newer.id.clone(),
                expected: None,
            })
            .unwrap();
        sidecar.complete_build(&snapshot(&older)).unwrap();
        let current = sidecar
            .published_view(&repository(), &canonical())
            .unwrap()
            .unwrap();

        let outcome = sidecar
            .publish(&PublishRequest {
                repository: repository(),
                view_name: canonical(),
                build_id: older.id.clone(),
                expected: Some(PublicationVersion {
                    snapshot_id: current.snapshot_id.clone(),
                    generation: current.generation,
                }),
            })
            .unwrap();
        assert_eq!(
            outcome,
            PublicationOutcome::Superseded {
                current: current.clone()
            }
        );
        assert_eq!(
            sidecar.build(&older.id).unwrap().unwrap().state,
            BuildState::Superseded
        );
        assert_eq!(
            sidecar.published_view(&repository(), &canonical()).unwrap(),
            Some(current)
        );
    }

    #[test]
    fn stale_compare_and_set_leaves_pointer_and_candidate_unchanged() {
        let (_directory, mut sidecar) = sidecar();
        let first = build(1);
        sidecar.start_build(&first).unwrap();
        sidecar.complete_build(&snapshot(&first)).unwrap();
        sidecar
            .publish(&PublishRequest {
                repository: repository(),
                view_name: canonical(),
                build_id: first.id.clone(),
                expected: None,
            })
            .unwrap();
        let second = build(2);
        sidecar.start_build(&second).unwrap();
        sidecar.complete_build(&snapshot(&second)).unwrap();

        let error = sidecar
            .publish(&PublishRequest {
                repository: repository(),
                view_name: canonical(),
                build_id: second.id.clone(),
                expected: None,
            })
            .unwrap_err();
        assert!(matches!(error, StoreError::PublicationConflict { .. }));
        assert_eq!(
            sidecar.build(&second.id).unwrap().unwrap().state,
            BuildState::Complete
        );
        assert_eq!(
            sidecar
                .published_view(&repository(), &canonical())
                .unwrap()
                .unwrap()
                .build_id,
            first.id
        );
    }

    #[test]
    fn no_op_publish_still_enforces_compare_and_set_expectation() {
        let (_directory, mut sidecar) = sidecar();
        let build = build(1);
        sidecar.start_build(&build).unwrap();
        sidecar.complete_build(&snapshot(&build)).unwrap();
        let published = match sidecar
            .publish(&PublishRequest {
                repository: repository(),
                view_name: canonical(),
                build_id: build.id.clone(),
                expected: None,
            })
            .unwrap()
        {
            PublicationOutcome::Published { view } => view,
            PublicationOutcome::Superseded { .. } => unreachable!(),
        };
        let actual = PublicationVersion {
            snapshot_id: published.snapshot_id.clone(),
            generation: published.generation,
        };

        let missing_expectation = sidecar
            .publish(&PublishRequest {
                repository: repository(),
                view_name: canonical(),
                build_id: build.id.clone(),
                expected: None,
            })
            .unwrap_err();
        assert!(matches!(
            missing_expectation,
            StoreError::PublicationConflict {
                expected: None,
                actual: Some(ref version),
            } if version == &actual
        ));

        let stale_expectation = sidecar
            .publish(&PublishRequest {
                repository: repository(),
                view_name: canonical(),
                build_id: build.id.clone(),
                expected: Some(PublicationVersion {
                    snapshot_id: actual.snapshot_id.clone(),
                    generation: actual.generation - 1,
                }),
            })
            .unwrap_err();
        assert!(matches!(
            stale_expectation,
            StoreError::PublicationConflict {
                actual: Some(ref version),
                ..
            } if version == &actual
        ));

        let no_op = sidecar
            .publish(&PublishRequest {
                repository: repository(),
                view_name: canonical(),
                build_id: build.id,
                expected: Some(actual),
            })
            .unwrap();
        assert_eq!(no_op, PublicationOutcome::Published { view: published });
    }

    #[test]
    fn completed_snapshot_identity_is_immutable_across_build_attempts() {
        let (_directory, mut sidecar) = sidecar();
        let first = build(1);
        sidecar.start_build(&first).unwrap();
        let original = sidecar.complete_build(&snapshot(&first)).unwrap();

        let retry = GraphBuild {
            id: BuildId::new("build-retry").unwrap(),
            repository: first.repository.clone(),
            source_revision_id: first.source_revision_id.clone(),
            prospective_snapshot_id: first.prospective_snapshot_id.clone(),
            state: BuildState::Building,
        };
        sidecar.start_build(&retry).unwrap();
        let mut conflicting = original.clone();
        conflicting.completed_by = retry.id.clone();
        conflicting.extractor_set_digest = digest("different-extractors");

        assert!(matches!(
            sidecar.complete_build(&conflicting),
            Err(StoreError::IdentityMismatch("existing snapshot contents"))
        ));
        assert_eq!(
            sidecar.snapshot(&original.id).unwrap(),
            Some(original.clone())
        );
        assert_eq!(
            sidecar.build(&retry.id).unwrap().unwrap().state,
            BuildState::Building
        );
    }

    #[test]
    fn completed_candidate_can_be_explicitly_superseded() {
        let (_directory, mut sidecar) = sidecar();
        let candidate = build(1);
        sidecar.start_build(&candidate).unwrap();
        sidecar.complete_build(&snapshot(&candidate)).unwrap();

        let superseded = sidecar.supersede_build(&candidate.id).unwrap();
        assert_eq!(superseded.state, BuildState::Superseded);
        assert!(matches!(
            sidecar.publish(&PublishRequest {
                repository: repository(),
                view_name: canonical(),
                build_id: candidate.id,
                expected: None,
            }),
            Err(StoreError::InvalidTransition {
                state: BuildState::Superseded,
                operation: "publish"
            })
        ));
    }

    #[test]
    fn publication_database_failure_rolls_back_pointer_and_build_state() {
        let (_directory, mut sidecar) = sidecar();
        let first = build(1);
        sidecar.start_build(&first).unwrap();
        sidecar.complete_build(&snapshot(&first)).unwrap();
        let first_view = match sidecar
            .publish(&PublishRequest {
                repository: repository(),
                view_name: canonical(),
                build_id: first.id.clone(),
                expected: None,
            })
            .unwrap()
        {
            PublicationOutcome::Published { view } => view,
            PublicationOutcome::Superseded { .. } => unreachable!(),
        };
        let second = build(2);
        sidecar.start_build(&second).unwrap();
        sidecar.complete_build(&snapshot(&second)).unwrap();
        sidecar
            .connection()
            .execute_batch(
                "CREATE TRIGGER reject_test_publication BEFORE UPDATE ON published_views \
                 BEGIN SELECT RAISE(ABORT, 'simulated publication failure'); END;",
            )
            .unwrap();

        let result = sidecar.publish(&PublishRequest {
            repository: repository(),
            view_name: canonical(),
            build_id: second.id.clone(),
            expected: Some(PublicationVersion {
                snapshot_id: first_view.snapshot_id.clone(),
                generation: first_view.generation,
            }),
        });
        assert!(matches!(result, Err(StoreError::Database(_))));
        assert_eq!(
            sidecar.published_view(&repository(), &canonical()).unwrap(),
            Some(first_view)
        );
        assert_eq!(
            sidecar.build(&second.id).unwrap().unwrap().state,
            BuildState::Complete
        );
    }
}
