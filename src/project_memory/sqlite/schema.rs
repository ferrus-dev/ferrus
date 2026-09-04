//! Initialize and migrate the memory sidecar schema and encode revision/publication rows.

use super::*;

pub(super) fn initialize_schema(connection: &mut Connection) -> Result<(), MemoryStoreError> {
    let application_id: u32 =
        connection.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if application_id != MEMORY_APPLICATION_ID && (version > 0 || application_id != 0) {
        return Err(MemoryStoreError::RequiresRebuild);
    }
    if version > MEMORY_SIDECAR_SCHEMA_VERSION {
        return Err(MemoryStoreError::RequiresRebuild);
    }
    if version == MEMORY_SIDECAR_SCHEMA_VERSION {
        return Ok(());
    }
    if version == 0 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(
        r#"
        CREATE TABLE memory_builds (
            sequence INTEGER PRIMARY KEY AUTOINCREMENT,
            id TEXT NOT NULL UNIQUE,
            project_namespace TEXT NOT NULL,
            project_id TEXT NOT NULL,
            prospective_revision_id TEXT NOT NULL,
            state TEXT NOT NULL CHECK (state IN ('building', 'complete', 'published', 'failed', 'superseded')),
            started_at TEXT NOT NULL,
            finished_at TEXT,
            failure_code TEXT,
            metrics_json TEXT
        ) STRICT;

        CREATE TABLE memory_revisions (
            id TEXT PRIMARY KEY,
            project_namespace TEXT NOT NULL,
            project_id TEXT NOT NULL,
            source_set_algorithm TEXT NOT NULL,
            source_set_digest TEXT NOT NULL,
            policy_algorithm TEXT NOT NULL,
            policy_digest TEXT NOT NULL,
            memory_model_version INTEGER NOT NULL,
            extractor_set_algorithm TEXT NOT NULL,
            extractor_set_digest TEXT NOT NULL,
            completed_by_build_id TEXT NOT NULL REFERENCES memory_builds(id),
            completed_at TEXT NOT NULL
        ) STRICT;

        CREATE TABLE memory_entities (
            revision_id TEXT NOT NULL REFERENCES memory_revisions(id) ON DELETE CASCADE,
            id TEXT NOT NULL,
            kind TEXT NOT NULL,
            source_category TEXT NOT NULL,
            entity_json TEXT NOT NULL,
            PRIMARY KEY(revision_id, id)
        ) STRICT;

        CREATE TABLE memory_relationships (
            revision_id TEXT NOT NULL REFERENCES memory_revisions(id) ON DELETE CASCADE,
            id TEXT NOT NULL,
            kind TEXT NOT NULL,
            source_entity_id TEXT NOT NULL,
            relationship_json TEXT NOT NULL,
            PRIMARY KEY(revision_id, id)
        ) STRICT;

        CREATE TABLE memory_published_views (
            project_namespace TEXT NOT NULL,
            project_id TEXT NOT NULL,
            view_name TEXT NOT NULL,
            revision_id TEXT NOT NULL REFERENCES memory_revisions(id),
            build_id TEXT NOT NULL REFERENCES memory_builds(id),
            generation INTEGER NOT NULL CHECK (generation > 0),
            published_at TEXT NOT NULL,
            PRIMARY KEY(project_namespace, project_id, view_name)
        ) STRICT;

        CREATE TABLE memory_fragment_cache (
            key_json TEXT PRIMARY KEY,
            fragment_json TEXT NOT NULL,
            updated_at TEXT NOT NULL
        ) STRICT;

        CREATE TABLE memory_diagnostics (
            build_id TEXT NOT NULL REFERENCES memory_builds(id) ON DELETE CASCADE,
            revision_id TEXT NOT NULL REFERENCES memory_revisions(id) ON DELETE CASCADE,
            sequence INTEGER NOT NULL,
            severity TEXT NOT NULL,
            code TEXT NOT NULL,
            diagnostic_json TEXT NOT NULL,
            PRIMARY KEY(build_id, sequence)
        ) STRICT;

        CREATE TABLE memory_revision_diagnostic_sets (
            revision_id TEXT PRIMARY KEY REFERENCES memory_revisions(id) ON DELETE CASCADE,
            build_id TEXT NOT NULL REFERENCES memory_builds(id) ON DELETE CASCADE
        ) STRICT;

        CREATE INDEX memory_entities_kind_idx ON memory_entities(revision_id, kind);
        CREATE INDEX memory_relationships_source_idx
            ON memory_relationships(revision_id, source_entity_id, kind);
        CREATE INDEX memory_diagnostics_revision_idx
            ON memory_diagnostics(revision_id, sequence);
        "#,
    )?;
        transaction.pragma_update(None, "application_id", MEMORY_APPLICATION_ID)?;
        transaction.pragma_update(None, "user_version", 1)?;
        transaction.commit()?;
    }
    migrate_repository_link_schema(connection)?;
    Ok(())
}

pub(super) fn migrate_repository_link_schema(
    connection: &mut Connection,
) -> Result<(), MemoryStoreError> {
    let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version >= 2 {
        return Ok(());
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        r#"
        CREATE TABLE memory_repository_link_sets (
            sequence INTEGER PRIMARY KEY AUTOINCREMENT,
            id TEXT NOT NULL UNIQUE,
            memory_revision_id TEXT NOT NULL REFERENCES memory_revisions(id) ON DELETE CASCADE,
            repository_namespace TEXT NOT NULL,
            repository_id TEXT NOT NULL,
            snapshot_id TEXT,
            link_set_json TEXT NOT NULL,
            created_at TEXT NOT NULL
        ) STRICT;

        CREATE TABLE memory_repository_links (
            link_set_id TEXT NOT NULL REFERENCES memory_repository_link_sets(id) ON DELETE CASCADE,
            relationship_id TEXT NOT NULL,
            resolution TEXT NOT NULL CHECK (resolution IN ('resolved', 'unresolved', 'stale')),
            relationship_json TEXT NOT NULL,
            PRIMARY KEY(link_set_id, relationship_id)
        ) STRICT;

        CREATE TABLE memory_repository_link_diagnostics (
            link_set_id TEXT NOT NULL REFERENCES memory_repository_link_sets(id) ON DELETE CASCADE,
            sequence INTEGER NOT NULL,
            code TEXT NOT NULL,
            diagnostic_json TEXT NOT NULL,
            PRIMARY KEY(link_set_id, sequence)
        ) STRICT;

        CREATE INDEX memory_repository_link_sets_revision_idx
            ON memory_repository_link_sets(
                memory_revision_id, repository_namespace, repository_id, sequence
            );
        CREATE INDEX memory_repository_links_resolution_idx
            ON memory_repository_links(link_set_id, resolution, relationship_id);
        "#,
    )?;
    transaction.pragma_update(None, "user_version", 2)?;
    transaction.commit()?;
    Ok(())
}

pub(super) fn insert_revision(
    transaction: &Transaction<'_>,
    revision: &MemoryRevision,
) -> Result<(), MemoryStoreError> {
    transaction.execute(
        "INSERT INTO memory_revisions( \
            id, project_namespace, project_id, source_set_algorithm, source_set_digest, \
            policy_algorithm, policy_digest, memory_model_version, extractor_set_algorithm, \
            extractor_set_digest, completed_by_build_id, completed_at \
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            revision.id.as_str(),
            revision.project.namespace.as_str(),
            revision.project.project_id.as_str(),
            revision.source_set_digest.algorithm(),
            revision.source_set_digest.value(),
            revision.policy_digest.algorithm(),
            revision.policy_digest.value(),
            revision.memory_model_version,
            revision.extractor_set_digest.algorithm(),
            revision.extractor_set_digest.value(),
            revision.completed_by.as_str(),
            timestamp(),
        ],
    )?;
    Ok(())
}

pub(super) fn load_build(
    connection: &Connection,
    build_id: &MemoryBuildId,
) -> Result<Option<MemoryBuild>, MemoryStoreError> {
    connection
        .query_row(
            "SELECT project_namespace, project_id, prospective_revision_id, state \
             FROM memory_builds WHERE id = ?1",
            [build_id.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?
        .map(|(namespace, project_id, revision_id, state)| {
            Ok(MemoryBuild {
                id: build_id.clone(),
                project: project_ref(namespace, project_id)?,
                prospective_revision_id: MemoryRevisionId::new(revision_id)?,
                state: parse_build_state(&state)?,
            })
        })
        .transpose()
}

pub(super) fn load_revision(
    connection: &Connection,
    revision_id: &MemoryRevisionId,
) -> Result<Option<MemoryRevision>, MemoryStoreError> {
    connection
        .query_row(
            "SELECT project_namespace, project_id, source_set_algorithm, source_set_digest, \
                policy_algorithm, policy_digest, memory_model_version, extractor_set_algorithm, \
                extractor_set_digest, completed_by_build_id \
             FROM memory_revisions WHERE id = ?1",
            [revision_id.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, u32>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                ))
            },
        )
        .optional()?
        .map(|row| {
            Ok(MemoryRevision {
                id: revision_id.clone(),
                project: project_ref(row.0, row.1)?,
                source_set_digest: Digest::new(row.2, row.3)?,
                policy_digest: Digest::new(row.4, row.5)?,
                memory_model_version: row.6,
                extractor_set_digest: Digest::new(row.7, row.8)?,
                completed_by: MemoryBuildId::new(row.9)?,
            })
        })
        .transpose()
}

pub(super) fn load_published_view(
    connection: &Connection,
    project: &ProjectRef,
    view_name: &MemoryViewName,
) -> Result<Option<PublishedMemoryRevision>, MemoryStoreError> {
    connection
        .query_row(
            "SELECT revision_id, build_id, generation FROM memory_published_views \
             WHERE project_namespace = ?1 AND project_id = ?2 AND view_name = ?3",
            params![
                project.namespace.as_str(),
                project.project_id.as_str(),
                view_name.as_str(),
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?
        .map(|(revision_id, build_id, generation)| {
            Ok(PublishedMemoryRevision {
                project: project.clone(),
                view_name: view_name.clone(),
                revision_id: MemoryRevisionId::new(revision_id)?,
                build_id: MemoryBuildId::new(build_id)?,
                generation: u64::try_from(generation).map_err(|_| MemoryStoreError::Corrupt)?,
            })
        })
        .transpose()
}

pub(super) fn project_ref(
    namespace: String,
    project_id: String,
) -> Result<ProjectRef, MemoryStoreError> {
    Ok(ProjectRef {
        namespace: ProjectNamespace::new(namespace)?,
        project_id: ProjectId::new(project_id)?,
    })
}

pub(super) fn parse_build_state(value: &str) -> Result<MemoryBuildState, MemoryStoreError> {
    match value {
        "building" => Ok(MemoryBuildState::Building),
        "complete" => Ok(MemoryBuildState::Complete),
        "published" => Ok(MemoryBuildState::Published),
        "failed" => Ok(MemoryBuildState::Failed),
        "superseded" => Ok(MemoryBuildState::Superseded),
        _ => Err(MemoryStoreError::Corrupt),
    }
}

pub(in crate::project_memory) fn source_category(
    category: &super::super::domain::MemorySourceCategory,
) -> &'static str {
    match category {
        super::super::domain::MemorySourceCategory::SpecificationStructure => {
            "specification_structure"
        }
        super::super::domain::MemorySourceCategory::ApprovedOutcome => "approved_outcome",
        super::super::domain::MemorySourceCategory::ArchiveManifest => "archive_manifest",
        super::super::domain::MemorySourceCategory::RuntimeProvenance => "runtime_provenance",
        super::super::domain::MemorySourceCategory::TaskBody => "task_body",
        super::super::domain::MemorySourceCategory::SubmissionBody => "submission_body",
        super::super::domain::MemorySourceCategory::ReviewBody => "review_body",
        super::super::domain::MemorySourceCategory::PatchBody => "patch_body",
        super::super::domain::MemorySourceCategory::CheckLogBody => "check_log_body",
        super::super::domain::MemorySourceCategory::QuestionBody => "question_body",
        super::super::domain::MemorySourceCategory::AnswerBody => "answer_body",
        super::super::domain::MemorySourceCategory::ConsultationBody => "consultation_body",
        super::super::domain::MemorySourceCategory::IntegrationErrorBody => {
            "integration_error_body"
        }
    }
}

pub(super) fn resolution_state(state: MemoryResolutionState) -> &'static str {
    match state {
        MemoryResolutionState::Resolved => "resolved",
        MemoryResolutionState::Unresolved => "unresolved",
        MemoryResolutionState::Stale => "stale",
    }
}

pub(super) fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}
