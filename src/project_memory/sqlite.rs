//! Versioned SQLite sidecar for rebuildable project-memory revisions.

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use chrono::{SecondsFormat, Utc};
use rusqlite::{
    Connection, Error as SqliteError, ErrorCode, OpenFlags, OptionalExtension, Transaction,
    TransactionBehavior, params,
};
use thiserror::Error;

use crate::repository_graph::domain::{Digest, RepositoryRef, SnapshotId};

use super::{
    diagnostics::MemoryDiagnostic,
    domain::{
        MemoryBuild, MemoryBuildId, MemoryBuildState, MemoryCommit, MemoryEntity, MemoryFragment,
        MemoryFragmentCacheKey, MemoryPublicationOutcome, MemoryPublicationVersion,
        MemoryPublishRequest, MemoryRelationship, MemoryRepositoryLinkCommit,
        MemoryRepositoryLinkSet, MemoryRepositoryLinkSetId, MemoryResolutionState, MemoryRevision,
        MemoryRevisionId, MemoryViewName, ProjectId, ProjectNamespace, ProjectRef,
        PublishedMemoryRevision,
    },
    ports::{BoundedMemoryLinks, MemoryBuildFailure, MemoryLinkStore, MemoryStore},
};

pub const MEMORY_SIDECAR_FILE_NAME: &str = "project-memory.db";
pub const MEMORY_SIDECAR_SCHEMA_VERSION: u32 = 2;
const MEMORY_APPLICATION_ID: u32 = 0x4650_4d31; // "FPM1"
const MEMORY_QUERY_PROGRESS_OPS: i32 = 100;

struct MemoryReadDeadline<'connection> {
    connection: &'connection Connection,
}

impl<'connection> MemoryReadDeadline<'connection> {
    fn install(
        connection: &'connection Connection,
        started: Instant,
        max_duration_ms: u64,
    ) -> Result<Self, MemoryStoreError> {
        let duration = Duration::from_millis(max_duration_ms);
        connection.progress_handler(
            MEMORY_QUERY_PROGRESS_OPS,
            Some(move || started.elapsed() >= duration),
        )?;
        Ok(Self { connection })
    }
}

impl Drop for MemoryReadDeadline<'_> {
    fn drop(&mut self) {
        let _ = self.connection.progress_handler(0, None::<fn() -> bool>);
    }
}

#[derive(Debug, Error)]
pub enum MemoryStoreError {
    #[error("project-memory build was not found")]
    BuildNotFound,
    #[error("project-memory revision was not found")]
    RevisionNotFound,
    #[error("invalid project-memory build transition")]
    InvalidTransition,
    #[error("project-memory identity mismatch")]
    IdentityMismatch,
    #[error("project-memory publication compare-and-set failed")]
    PublicationConflict,
    #[error("project-memory sidecar schema requires rebuild")]
    RequiresRebuild,
    #[error("project-memory sidecar contains invalid data")]
    Corrupt,
    #[error(transparent)]
    Database(#[from] rusqlite::Error),
    #[error(transparent)]
    Serialization(#[from] serde_json::Error),
    #[error(transparent)]
    Identity(#[from] super::domain::MemoryValueError),
    #[error(transparent)]
    Digest(#[from] crate::repository_graph::domain::DigestError),
}

pub struct MemorySidecar {
    path: PathBuf,
    connection: Connection,
}

pub enum OpenMemoryQuerySidecarResult {
    Ready(Box<MemorySidecar>),
    Absent,
    NeedsMigration { found_schema_version: u32 },
    RequiresRebuild,
}

/// Opens an existing compatible memory sidecar without creating or migrating it.
pub fn open_for_query_at(path: &Path) -> Result<OpenMemoryQuerySidecarResult, MemoryStoreError> {
    if !path
        .try_exists()
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?
    {
        return Ok(OpenMemoryQuerySidecarResult::Absent);
    }
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let application_id: u32 =
        connection.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if application_id != MEMORY_APPLICATION_ID || version > MEMORY_SIDECAR_SCHEMA_VERSION {
        return Ok(OpenMemoryQuerySidecarResult::RequiresRebuild);
    }
    if version < MEMORY_SIDECAR_SCHEMA_VERSION {
        return Ok(OpenMemoryQuerySidecarResult::NeedsMigration {
            found_schema_version: version,
        });
    }
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    connection.pragma_update(None, "foreign_keys", true)?;
    connection.pragma_update(None, "query_only", true)?;
    Ok(OpenMemoryQuerySidecarResult::Ready(Box::new(
        MemorySidecar {
            path: path.to_path_buf(),
            connection,
        },
    )))
}

impl MemorySidecar {
    pub fn open_at(data_dir: &Path) -> Result<Self, MemoryStoreError> {
        Self::open(data_dir.join(MEMORY_SIDECAR_FILE_NAME))
    }

    pub fn open(path: PathBuf) -> Result<Self, MemoryStoreError> {
        let mut connection = Connection::open(&path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        initialize_schema(&mut connection)?;
        connection.pragma_update(None, "foreign_keys", true)?;
        Ok(Self { path, connection })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn connection(&self) -> &Connection {
        &self.connection
    }

    pub fn entities_for_revision(
        &self,
        revision_id: &MemoryRevisionId,
    ) -> Result<Vec<MemoryEntity>, MemoryStoreError> {
        let mut statement = self.connection.prepare(
            "SELECT entity_json FROM memory_entities WHERE revision_id = ?1 ORDER BY id",
        )?;
        let rows = statement.query_map([revision_id.as_str()], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    pub fn relationships_for_revision(
        &self,
        revision_id: &MemoryRevisionId,
    ) -> Result<Vec<MemoryRelationship>, MemoryStoreError> {
        let mut statement = self.connection.prepare(
            "SELECT relationship_json FROM memory_relationships \
             WHERE revision_id = ?1 ORDER BY id",
        )?;
        let rows = statement.query_map([revision_id.as_str()], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    pub fn diagnostics_for_revision(
        &self,
        revision_id: &MemoryRevisionId,
    ) -> Result<Vec<MemoryDiagnostic>, MemoryStoreError> {
        let mut statement = self.connection.prepare(
            "SELECT diagnostics.diagnostic_json FROM memory_diagnostics diagnostics \
             JOIN memory_revision_diagnostic_sets sets ON sets.build_id = diagnostics.build_id \
             WHERE sets.revision_id = ?1 ORDER BY diagnostics.sequence",
        )?;
        let rows = statement.query_map([revision_id.as_str()], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }
}

impl MemoryStore for MemorySidecar {
    type Error = MemoryStoreError;

    fn start_build(&mut self, build: &MemoryBuild) -> Result<(), Self::Error> {
        if build.state != MemoryBuildState::Building {
            return Err(MemoryStoreError::InvalidTransition);
        }
        self.connection.execute(
            "INSERT INTO memory_builds( \
                id, project_namespace, project_id, prospective_revision_id, state, started_at \
             ) VALUES (?1, ?2, ?3, ?4, 'building', ?5)",
            params![
                build.id.as_str(),
                build.project.namespace.as_str(),
                build.project.project_id.as_str(),
                build.prospective_revision_id.as_str(),
                timestamp(),
            ],
        )?;
        Ok(())
    }

    fn fail_build(
        &mut self,
        build_id: &MemoryBuildId,
        failure: &MemoryBuildFailure,
    ) -> Result<(), Self::Error> {
        let changed = self.connection.execute(
            "UPDATE memory_builds SET state = 'failed', finished_at = ?2, failure_code = ?3 \
             WHERE id = ?1 AND state = 'building'",
            params![build_id.as_str(), timestamp(), failure.code.as_str()],
        )?;
        if changed != 1 {
            return Err(MemoryStoreError::InvalidTransition);
        }
        Ok(())
    }

    fn complete_build(&mut self, commit: &MemoryCommit) -> Result<(), Self::Error> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let build = load_build(&transaction, &commit.revision.completed_by)?
            .ok_or(MemoryStoreError::BuildNotFound)?;
        if build.state != MemoryBuildState::Building
            || build.project != commit.revision.project
            || build.prospective_revision_id != commit.revision.id
        {
            return Err(MemoryStoreError::IdentityMismatch);
        }
        if let Some(existing) = load_revision(&transaction, &commit.revision.id)? {
            if existing.project != commit.revision.project
                || existing.source_set_digest != commit.revision.source_set_digest
                || existing.policy_digest != commit.revision.policy_digest
                || existing.memory_model_version != commit.revision.memory_model_version
                || existing.extractor_set_digest != commit.revision.extractor_set_digest
            {
                return Err(MemoryStoreError::IdentityMismatch);
            }
        } else {
            insert_revision(&transaction, &commit.revision)?;
            for entity in &commit.entities {
                if entity.project != commit.revision.project
                    || entity.memory_revision_id != commit.revision.id
                {
                    return Err(MemoryStoreError::IdentityMismatch);
                }
                transaction.execute(
                    "INSERT INTO memory_entities(revision_id, id, kind, source_category, entity_json) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        commit.revision.id.as_str(),
                        entity.id.as_str(),
                        entity.data.kind().as_str(),
                        source_category(&entity.provenance.source_category),
                        serde_json::to_string(entity)?,
                    ],
                )?;
            }
            for relationship in &commit.relationships {
                if relationship.project != commit.revision.project
                    || relationship.memory_revision_id != commit.revision.id
                {
                    return Err(MemoryStoreError::IdentityMismatch);
                }
                transaction.execute(
                    "INSERT INTO memory_relationships( \
                        revision_id, id, kind, source_entity_id, relationship_json \
                     ) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        commit.revision.id.as_str(),
                        relationship.id.as_str(),
                        relationship.kind.as_str(),
                        relationship.source.as_str(),
                        serde_json::to_string(relationship)?,
                    ],
                )?;
            }
        }
        for repository_links in &commit.repository_links {
            insert_repository_link_commit(&transaction, &commit.revision, repository_links)?;
        }
        transaction.execute(
            "DELETE FROM memory_diagnostics WHERE build_id = ?1",
            [commit.revision.completed_by.as_str()],
        )?;
        for (sequence, diagnostic) in commit.diagnostics.iter().enumerate() {
            transaction.execute(
                "INSERT INTO memory_diagnostics( \
                    build_id, revision_id, sequence, severity, code, diagnostic_json \
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    commit.revision.completed_by.as_str(),
                    commit.revision.id.as_str(),
                    sequence as i64,
                    format!("{:?}", diagnostic.severity).to_ascii_lowercase(),
                    diagnostic.code.as_str(),
                    serde_json::to_string(diagnostic)?,
                ],
            )?;
        }
        transaction.execute(
            "INSERT INTO memory_revision_diagnostic_sets(revision_id, build_id) VALUES (?1, ?2) \
             ON CONFLICT(revision_id) DO UPDATE SET build_id = excluded.build_id",
            params![
                commit.revision.id.as_str(),
                commit.revision.completed_by.as_str(),
            ],
        )?;
        for cached in &commit.cache_writes {
            transaction.execute(
                "INSERT INTO memory_fragment_cache(key_json, fragment_json, updated_at) \
                 VALUES (?1, ?2, ?3) \
                 ON CONFLICT(key_json) DO UPDATE SET \
                    fragment_json = excluded.fragment_json, updated_at = excluded.updated_at",
                params![
                    serde_json::to_string(&cached.key)?,
                    serde_json::to_string(&cached.fragment)?,
                    timestamp(),
                ],
            )?;
        }
        let completed = transaction.execute(
            "UPDATE memory_builds SET state = 'complete', finished_at = ?2, \
                metrics_json = ?3 WHERE id = ?1 AND state = 'building'",
            params![
                commit.revision.completed_by.as_str(),
                timestamp(),
                serde_json::to_string(&commit.metrics)?,
            ],
        )?;
        if completed != 1 {
            return Err(MemoryStoreError::InvalidTransition);
        }
        transaction.commit()?;
        Ok(())
    }

    fn publish(
        &mut self,
        request: &MemoryPublishRequest,
    ) -> Result<MemoryPublicationOutcome, Self::Error> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let build =
            load_build(&transaction, &request.build_id)?.ok_or(MemoryStoreError::BuildNotFound)?;
        if build.project != request.project || build.state != MemoryBuildState::Complete {
            return Err(MemoryStoreError::InvalidTransition);
        }
        let revision = load_revision(&transaction, &build.prospective_revision_id)?
            .ok_or(MemoryStoreError::RevisionNotFound)?;
        let current = load_published_view(&transaction, &request.project, &request.view_name)?;
        let actual = current.as_ref().map(|view| MemoryPublicationVersion {
            revision_id: view.revision_id.clone(),
            generation: view.generation,
        });
        if request.expected != actual {
            return Err(MemoryStoreError::PublicationConflict);
        }
        if let Some(current) = current.as_ref()
            && current.revision_id == revision.id
        {
            transaction.execute(
                "UPDATE memory_builds SET state = 'superseded' WHERE id = ?1",
                [build.id.as_str()],
            )?;
            transaction.commit()?;
            return Ok(MemoryPublicationOutcome::Published {
                view: current.clone(),
            });
        }
        let generation = actual.map_or(1, |version| version.generation + 1);
        transaction.execute(
            "INSERT INTO memory_published_views( \
                project_namespace, project_id, view_name, revision_id, build_id, generation, \
                published_at \
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT(project_namespace, project_id, view_name) DO UPDATE SET \
                revision_id = excluded.revision_id, build_id = excluded.build_id, \
                generation = excluded.generation, published_at = excluded.published_at",
            params![
                request.project.namespace.as_str(),
                request.project.project_id.as_str(),
                request.view_name.as_str(),
                revision.id.as_str(),
                build.id.as_str(),
                i64::try_from(generation).map_err(|_| MemoryStoreError::Corrupt)?,
                timestamp(),
            ],
        )?;
        if let Some(current) = current.as_ref() {
            transaction.execute(
                "UPDATE memory_builds SET state = 'superseded' \
                 WHERE id = ?1 AND state = 'published'",
                [current.build_id.as_str()],
            )?;
        }
        transaction.execute(
            "UPDATE memory_builds SET state = 'published' WHERE id = ?1",
            [build.id.as_str()],
        )?;
        let view = PublishedMemoryRevision {
            project: request.project.clone(),
            view_name: request.view_name.clone(),
            revision_id: revision.id,
            build_id: build.id,
            generation,
        };
        transaction.commit()?;
        Ok(MemoryPublicationOutcome::Published { view })
    }

    fn supersede_build(&mut self, build_id: &MemoryBuildId) -> Result<(), Self::Error> {
        let changed = self.connection.execute(
            "UPDATE memory_builds SET state = 'superseded' \
             WHERE id = ?1 AND state IN ('building', 'complete')",
            [build_id.as_str()],
        )?;
        if changed != 1 {
            return Err(MemoryStoreError::InvalidTransition);
        }
        Ok(())
    }

    fn build(&self, build_id: &MemoryBuildId) -> Result<Option<MemoryBuild>, Self::Error> {
        load_build(&self.connection, build_id)
    }

    fn revision(
        &self,
        revision_id: &MemoryRevisionId,
    ) -> Result<Option<MemoryRevision>, Self::Error> {
        load_revision(&self.connection, revision_id)
    }

    fn published_view(
        &self,
        project: &ProjectRef,
        view_name: &MemoryViewName,
    ) -> Result<Option<PublishedMemoryRevision>, Self::Error> {
        load_published_view(&self.connection, project, view_name)
    }

    fn load_cached_fragment(
        &self,
        key: &MemoryFragmentCacheKey,
    ) -> Result<Option<MemoryFragment>, Self::Error> {
        let key = serde_json::to_string(key)?;
        let fragment = self
            .connection
            .query_row(
                "SELECT fragment_json FROM memory_fragment_cache WHERE key_json = ?1",
                [key],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        fragment
            .map(|fragment| serde_json::from_str(&fragment).map_err(Into::into))
            .transpose()
    }

    fn diagnostics_for_build(
        &self,
        build_id: &MemoryBuildId,
    ) -> Result<Vec<MemoryDiagnostic>, Self::Error> {
        let mut statement = self.connection.prepare(
            "SELECT diagnostic_json FROM memory_diagnostics WHERE build_id = ?1 ORDER BY sequence",
        )?;
        let rows = statement.query_map([build_id.as_str()], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }
}

impl MemoryLinkStore for MemorySidecar {
    type Error = MemoryStoreError;

    fn repository_link_set(
        &self,
        link_set_id: &MemoryRepositoryLinkSetId,
    ) -> Result<Option<MemoryRepositoryLinkSet>, Self::Error> {
        self.connection
            .query_row(
                "SELECT link_set_json FROM memory_repository_link_sets WHERE id = ?1",
                [link_set_id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .transpose()
    }

    fn repository_link_set_for_snapshot(
        &self,
        revision_id: &MemoryRevisionId,
        repository: &RepositoryRef,
        snapshot_id: Option<&SnapshotId>,
    ) -> Result<Option<MemoryRepositoryLinkSet>, Self::Error> {
        self.connection
            .query_row(
                "SELECT link_set_json FROM memory_repository_link_sets \
                 WHERE memory_revision_id = ?1 AND repository_namespace = ?2 \
                   AND repository_id = ?3 AND snapshot_id IS ?4 \
                 ORDER BY sequence DESC LIMIT 1",
                params![
                    revision_id.as_str(),
                    repository.namespace.as_str(),
                    repository.repository_id.as_str(),
                    snapshot_id.map(SnapshotId::as_str),
                ],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .transpose()
    }

    fn latest_repository_link_set(
        &self,
        revision_id: &MemoryRevisionId,
        repository: &RepositoryRef,
    ) -> Result<Option<MemoryRepositoryLinkSet>, Self::Error> {
        self.connection
            .query_row(
                "SELECT link_set_json FROM memory_repository_link_sets \
                 WHERE memory_revision_id = ?1 AND repository_namespace = ?2 \
                   AND repository_id = ?3 ORDER BY sequence DESC LIMIT 1",
                params![
                    revision_id.as_str(),
                    repository.namespace.as_str(),
                    repository.repository_id.as_str(),
                ],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .transpose()
    }

    fn latest_compatible_repository_link_set(
        &self,
        revision: &MemoryRevision,
        repository: &RepositoryRef,
    ) -> Result<Option<MemoryRepositoryLinkSet>, Self::Error> {
        self.connection
            .query_row(
                "SELECT sets.link_set_json FROM memory_repository_link_sets sets \
                 JOIN memory_revisions revisions ON revisions.id = sets.memory_revision_id \
                 WHERE sets.memory_revision_id <> ?1 \
                   AND revisions.project_namespace = ?2 AND revisions.project_id = ?3 \
                   AND revisions.policy_algorithm = ?4 AND revisions.policy_digest = ?5 \
                   AND revisions.memory_model_version = ?6 \
                   AND revisions.extractor_set_algorithm = ?7 \
                   AND revisions.extractor_set_digest = ?8 \
                   AND sets.repository_namespace = ?9 AND sets.repository_id = ?10 \
                 ORDER BY sets.sequence DESC LIMIT 1",
                params![
                    revision.id.as_str(),
                    revision.project.namespace.as_str(),
                    revision.project.project_id.as_str(),
                    revision.policy_digest.algorithm(),
                    revision.policy_digest.value(),
                    revision.memory_model_version,
                    revision.extractor_set_digest.algorithm(),
                    revision.extractor_set_digest.value(),
                    repository.namespace.as_str(),
                    repository.repository_id.as_str(),
                ],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .transpose()
    }

    fn repository_links(
        &self,
        link_set_id: &MemoryRepositoryLinkSetId,
    ) -> Result<Vec<MemoryRelationship>, Self::Error> {
        let mut statement = self.connection.prepare(
            "SELECT relationship_json FROM memory_repository_links \
             WHERE link_set_id = ?1 ORDER BY relationship_id",
        )?;
        let rows = statement.query_map([link_set_id.as_str()], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    fn repository_link_diagnostics(
        &self,
        link_set_id: &MemoryRepositoryLinkSetId,
    ) -> Result<Vec<MemoryDiagnostic>, Self::Error> {
        let mut statement = self.connection.prepare(
            "SELECT diagnostic_json FROM memory_repository_link_diagnostics \
             WHERE link_set_id = ?1 ORDER BY sequence",
        )?;
        let rows = statement.query_map([link_set_id.as_str()], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    fn bounded_repository_links(
        &self,
        link_set_id: &MemoryRepositoryLinkSetId,
        max_relationships: u32,
        max_diagnostics: u32,
        max_duration_ms: u64,
    ) -> Result<BoundedMemoryLinks, Self::Error> {
        let started = Instant::now();
        let deadline =
            MemoryReadDeadline::install(&self.connection, started, max_duration_ms.max(1))?;
        let mut relationships = Vec::new();
        let mut duration_exceeded = false;
        {
            let mut statement = self.connection.prepare(
                "SELECT relationship_json FROM memory_repository_links \
                 WHERE link_set_id = ?1 ORDER BY relationship_id LIMIT ?2",
            )?;
            let mut rows = statement.query(params![
                link_set_id.as_str(),
                i64::from(max_relationships.saturating_add(1)),
            ])?;
            loop {
                let row = match rows.next() {
                    Ok(row) => row,
                    Err(error) if sqlite_interrupted(&error) => {
                        duration_exceeded = true;
                        break;
                    }
                    Err(error) => return Err(error.into()),
                };
                let Some(row) = row else {
                    break;
                };
                let encoded = row.get::<_, String>(0)?;
                relationships.push(serde_json::from_str(&encoded)?);
            }
        }
        let relationship_truncated = relationships.len() > max_relationships as usize;
        relationships.truncate(max_relationships as usize);
        let mut diagnostics = Vec::new();
        if !duration_exceeded {
            let mut statement = self.connection.prepare(
                "SELECT diagnostic_json FROM memory_repository_link_diagnostics \
                 WHERE link_set_id = ?1 ORDER BY sequence LIMIT ?2",
            )?;
            let mut rows = statement.query(params![
                link_set_id.as_str(),
                i64::from(max_diagnostics.saturating_add(1)),
            ])?;
            loop {
                let row = match rows.next() {
                    Ok(row) => row,
                    Err(error) if sqlite_interrupted(&error) => {
                        duration_exceeded = true;
                        break;
                    }
                    Err(error) => return Err(error.into()),
                };
                let Some(row) = row else {
                    break;
                };
                let encoded = row.get::<_, String>(0)?;
                diagnostics.push(serde_json::from_str(&encoded)?);
            }
        }
        drop(deadline);
        let diagnostic_truncated = diagnostics.len() > max_diagnostics as usize;
        diagnostics.truncate(max_diagnostics as usize);
        Ok(BoundedMemoryLinks {
            relationships,
            diagnostics,
            truncated: relationship_truncated || diagnostic_truncated,
            duration_exceeded,
        })
    }
}

fn sqlite_interrupted(error: &SqliteError) -> bool {
    matches!(
        error,
        SqliteError::SqliteFailure(failure, _) if failure.code == ErrorCode::OperationInterrupted
    )
}

fn insert_repository_link_commit(
    transaction: &Transaction<'_>,
    revision: &MemoryRevision,
    commit: &MemoryRepositoryLinkCommit,
) -> Result<(), MemoryStoreError> {
    if commit.link_set.project != revision.project
        || commit.link_set.memory_revision_id != revision.id
        || commit.relationships.iter().any(|relationship| {
            relationship.project != revision.project
                || relationship.memory_revision_id != revision.id
        })
    {
        return Err(MemoryStoreError::IdentityMismatch);
    }
    let existing = transaction
        .query_row(
            "SELECT link_set_json FROM memory_repository_link_sets WHERE id = ?1",
            [commit.link_set.id.as_str()],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if let Some(existing) = existing {
        let existing: MemoryRepositoryLinkSet = serde_json::from_str(&existing)?;
        if existing != commit.link_set {
            return Err(MemoryStoreError::IdentityMismatch);
        }
        let mut statement = transaction.prepare(
            "SELECT relationship_json FROM memory_repository_links \
             WHERE link_set_id = ?1 ORDER BY relationship_id",
        )?;
        let rows =
            statement.query_map([commit.link_set.id.as_str()], |row| row.get::<_, String>(0))?;
        let existing_relationships = rows
            .map(|row| Ok(serde_json::from_str::<MemoryRelationship>(&row?)?))
            .collect::<Result<Vec<_>, MemoryStoreError>>()?;
        if existing_relationships.len() != commit.relationships.len()
            || existing_relationships
                .iter()
                .zip(&commit.relationships)
                .any(|(left, right)| {
                    repository_link_semantics(left) != repository_link_semantics(right)
                })
        {
            return Err(MemoryStoreError::IdentityMismatch);
        }
        return Ok(());
    }
    transaction.execute(
        "INSERT INTO memory_repository_link_sets( \
            id, memory_revision_id, repository_namespace, repository_id, snapshot_id, \
            link_set_json, created_at \
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            commit.link_set.id.as_str(),
            revision.id.as_str(),
            commit.link_set.repository.namespace.as_str(),
            commit.link_set.repository.repository_id.as_str(),
            commit
                .link_set
                .repository_snapshot_id
                .as_ref()
                .map(SnapshotId::as_str),
            serde_json::to_string(&commit.link_set)?,
            timestamp(),
        ],
    )?;
    for relationship in &commit.relationships {
        transaction.execute(
            "INSERT INTO memory_repository_links( \
                link_set_id, relationship_id, resolution, relationship_json \
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                commit.link_set.id.as_str(),
                relationship.id.as_str(),
                resolution_state(relationship.provenance.resolution),
                serde_json::to_string(relationship)?,
            ],
        )?;
    }
    for (sequence, diagnostic) in commit.diagnostics.iter().enumerate() {
        transaction.execute(
            "INSERT INTO memory_repository_link_diagnostics( \
                link_set_id, sequence, code, diagnostic_json \
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                commit.link_set.id.as_str(),
                sequence as i64,
                diagnostic.code.as_str(),
                serde_json::to_string(diagnostic)?,
            ],
        )?;
    }
    Ok(())
}

fn repository_link_semantics(relationship: &MemoryRelationship) -> Vec<u8> {
    serde_json::to_vec(&(
        &relationship.project,
        &relationship.memory_revision_id,
        &relationship.id,
        relationship.kind,
        &relationship.source,
        &relationship.target,
        relationship.provenance.source_category,
        &relationship.provenance.source_locator,
        &relationship.provenance.source_fingerprint,
        &relationship.provenance.extractor,
        &relationship.provenance.evidence,
        relationship.provenance.resolution,
        relationship.provenance.confidence,
    ))
    .expect("repository link semantics are serializable")
}

mod schema;
use schema::*;

#[cfg(test)]
#[path = "sqlite_tests.rs"]
mod tests;
