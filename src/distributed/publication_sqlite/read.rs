//! Load scoped publication records and encrypted facts, and initialize their SQLite schema.

use super::*;

pub(super) fn load_graph_view(
    connection: &Connection,
    repository: &RemoteRepositoryRef,
    view_name: &PublishedViewName,
) -> Result<Option<PublishedRemoteGraphView>, RemoteStoreError> {
    connection
        .query_row(
            "SELECT snapshot_id, job_id, generation FROM remote_graph_views
             WHERE tenant_id = ?1 AND project_id = ?2 AND repository_id = ?3 AND view_name = ?4",
            params![
                repository.project.tenant_id.as_str(),
                repository.project.project_id.as_str(),
                repository.repository_id.as_str(),
                view_name.as_str()
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
        .map(|(snapshot_id, job_id, generation)| {
            Ok(PublishedRemoteGraphView {
                repository: repository.clone(),
                view_name: view_name.clone(),
                snapshot_id: SnapshotId::new(snapshot_id)
                    .map_err(|_| RemoteStoreError::IntegrityFailure)?,
                job: IndexJobRef {
                    project: repository.project.clone(),
                    job_id: IndexJobId::new(job_id)
                        .map_err(|_| RemoteStoreError::IntegrityFailure)?,
                    kind: IndexJobKind::RepositoryGraph,
                },
                generation: NonZeroU64::new(
                    u64::try_from(generation).map_err(|_| RemoteStoreError::IntegrityFailure)?,
                )
                .ok_or(RemoteStoreError::IntegrityFailure)?,
            })
        })
        .transpose()
}

pub(super) fn load_memory_view(
    connection: &Connection,
    project: &RemoteProjectRef,
    view_name: &MemoryViewName,
) -> Result<Option<PublishedRemoteMemoryView>, RemoteStoreError> {
    connection
        .query_row(
            "SELECT revision_id, job_id, generation FROM remote_memory_views
             WHERE tenant_id = ?1 AND project_id = ?2 AND view_name = ?3",
            params![
                project.tenant_id.as_str(),
                project.project_id.as_str(),
                view_name.as_str()
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
        .map(|(revision_id, job_id, generation)| {
            Ok(PublishedRemoteMemoryView {
                project: project.clone(),
                view_name: view_name.clone(),
                revision_id: MemoryRevisionId::new(revision_id)
                    .map_err(|_| RemoteStoreError::IntegrityFailure)?,
                job: IndexJobRef {
                    project: project.clone(),
                    job_id: IndexJobId::new(job_id)
                        .map_err(|_| RemoteStoreError::IntegrityFailure)?,
                    kind: IndexJobKind::ProjectMemory,
                },
                generation: NonZeroU64::new(
                    u64::try_from(generation).map_err(|_| RemoteStoreError::IntegrityFailure)?,
                )
                .ok_or(RemoteStoreError::IntegrityFailure)?,
            })
        })
        .transpose()
}

pub(super) fn load_graph_record(
    connection: &Connection,
    snapshot: &RemoteGraphSnapshotRef,
) -> Result<Option<RemoteGraphSnapshotRecord>, RemoteStoreError> {
    let revision = load_revision_row(
        connection,
        &snapshot.repository.project,
        "repository_graph",
        snapshot.repository.repository_id.as_str(),
        snapshot.snapshot_id.as_str(),
    )?;
    revision
        .map(|row| {
            let repository_identity = connection.query_row(
                "SELECT repository_identity_json FROM remote_graph_snapshot_metadata
             WHERE tenant_id = ?1 AND project_id = ?2 AND repository_id = ?3
               AND snapshot_id = ?4",
                params![
                    snapshot.repository.project.tenant_id.as_str(),
                    snapshot.repository.project.project_id.as_str(),
                    snapshot.repository.repository_id.as_str(),
                    snapshot.snapshot_id.as_str()
                ],
                |row| row.get::<_, Vec<u8>>(0),
            )?;
            Ok(RemoteGraphSnapshotRecord {
                snapshot: snapshot.clone(),
                repository_identity: decode(&repository_identity)?,
                job: IndexJobRef {
                    project: snapshot.repository.project.clone(),
                    job_id: row.job_id,
                    kind: IndexJobKind::RepositoryGraph,
                },
                build_id: BuildId::new(row.build_id)
                    .map_err(|_| RemoteStoreError::IntegrityFailure)?,
                extractor_set_digest: row.extractor_set_digest,
                fact_set_digest: row.fact_set_digest,
                counts: row.counts,
                completed_at: row.completed_at,
            })
        })
        .transpose()
}

pub(super) fn load_memory_record(
    connection: &Connection,
    revision: &RemoteMemoryRevisionRef,
) -> Result<Option<RemoteMemoryRevisionRecord>, RemoteStoreError> {
    load_revision_row(
        connection,
        &revision.project,
        "project_memory",
        "",
        revision.revision_id.as_str(),
    )?
    .map(|row| {
        Ok(RemoteMemoryRevisionRecord {
            revision: revision.clone(),
            job: IndexJobRef {
                project: revision.project.clone(),
                job_id: row.job_id,
                kind: IndexJobKind::ProjectMemory,
            },
            build_id: MemoryBuildId::new(row.build_id)
                .map_err(|_| RemoteStoreError::IntegrityFailure)?,
            extractor_set_digest: row.extractor_set_digest,
            fact_set_digest: row.fact_set_digest,
            counts: row.counts,
            completed_at: row.completed_at,
        })
    })
    .transpose()
}

pub(super) fn target_was_published(
    connection: &Connection,
    project: &RemoteProjectRef,
    domain: &str,
    repository_id: &str,
    target_id: &str,
) -> Result<bool, RemoteStoreError> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM remote_published_targets
             WHERE tenant_id = ?1 AND project_id = ?2 AND domain = ?3
               AND repository_id = ?4 AND target_id = ?5",
            params![
                project.tenant_id.as_str(),
                project.project_id.as_str(),
                domain,
                repository_id,
                target_id
            ],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

pub(super) struct RevisionRow {
    pub(super) job_id: IndexJobId,
    pub(super) build_id: String,
    pub(super) extractor_set_digest: Digest,
    pub(super) fact_set_digest: Digest,
    pub(super) counts: RemoteFactCounts,
    pub(super) completed_at: DateTime<Utc>,
}

pub(super) fn load_revision_row(
    connection: &Connection,
    project: &RemoteProjectRef,
    domain: &str,
    repository_id: &str,
    target_id: &str,
) -> Result<Option<RevisionRow>, RemoteStoreError> {
    connection
        .query_row(
            "SELECT job_id, build_id, extractor_digest_algorithm, extractor_digest_value,
                    fact_digest_algorithm, fact_digest_value, primary_count, relationship_count,
                    diagnostic_count, completed_at_ms
             FROM remote_immutable_revisions
             WHERE tenant_id = ?1 AND project_id = ?2 AND domain = ?3
               AND repository_id = ?4 AND target_id = ?5",
            params![
                project.tenant_id.as_str(),
                project.project_id.as_str(),
                domain,
                repository_id,
                target_id
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, i64>(9)?,
                ))
            },
        )
        .optional()?
        .map(|row| {
            Ok(RevisionRow {
                job_id: IndexJobId::new(row.0).map_err(|_| RemoteStoreError::IntegrityFailure)?,
                build_id: row.1,
                extractor_set_digest: Digest::new(row.2, row.3)
                    .map_err(|_| RemoteStoreError::IntegrityFailure)?,
                fact_set_digest: Digest::new(row.4, row.5)
                    .map_err(|_| RemoteStoreError::IntegrityFailure)?,
                counts: RemoteFactCounts {
                    primary: u64::try_from(row.6)
                        .map_err(|_| RemoteStoreError::IntegrityFailure)?,
                    relationships: u64::try_from(row.7)
                        .map_err(|_| RemoteStoreError::IntegrityFailure)?,
                    diagnostics: u64::try_from(row.8)
                        .map_err(|_| RemoteStoreError::IntegrityFailure)?,
                },
                completed_at: DateTime::from_timestamp_millis(row.9)
                    .ok_or(RemoteStoreError::IntegrityFailure)?,
            })
        })
        .transpose()
}

pub(super) fn load_facts(
    connection: &Connection,
    key: &LessSafeKey,
    job: &IndexJobRef,
    domain: &str,
    target_id: &str,
    repository_id: &str,
    deadline: Option<(Instant, Duration)>,
) -> Result<Vec<(String, Vec<u8>)>, RemoteStoreError> {
    let mut statement = connection.prepare(
        "SELECT fact_kind, fact_id, byte_len, nonce, ciphertext
         FROM remote_encrypted_facts
         WHERE tenant_id = ?1 AND project_id = ?2 AND domain = ?3
           AND repository_id = ?4 AND target_id = ?5
         ORDER BY fact_kind, fact_id",
    )?;
    let rows = statement.query_map(
        params![
            job.project.tenant_id.as_str(),
            job.project.project_id.as_str(),
            domain,
            repository_id,
            target_id
        ],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, Vec<u8>>(4)?,
            ))
        },
    )?;
    let mut facts = Vec::new();
    for row in rows {
        ensure_read_budget(deadline)?;
        let (kind, id, byte_len, nonce, mut ciphertext) = match row {
            Ok(row) => row,
            Err(_) if read_budget_exceeded(deadline) => {
                return Err(RemoteStoreError::ReadBudgetExceeded);
            }
            Err(error) => return Err(RemoteStoreError::Database(error)),
        };
        let nonce = Nonce::try_assume_unique_for_key(&nonce)
            .map_err(|_| RemoteStoreError::IntegrityFailure)?;
        let plaintext = key
            .open_in_place(
                nonce,
                Aad::from(fact_aad(job, domain, target_id, &kind, &id)),
                &mut ciphertext,
            )
            .map_err(|_| RemoteStoreError::IntegrityFailure)?;
        if u64::try_from(plaintext.len()).ok() != u64::try_from(byte_len).ok() {
            return Err(RemoteStoreError::IntegrityFailure);
        }
        facts.push((kind, plaintext.to_vec()));
    }
    Ok(facts)
}

pub(super) fn read_budget_exceeded(deadline: Option<(Instant, Duration)>) -> bool {
    deadline.is_some_and(|(started, duration)| started.elapsed() >= duration)
}

pub(super) fn ensure_read_budget(
    deadline: Option<(Instant, Duration)>,
) -> Result<(), RemoteStoreError> {
    if read_budget_exceeded(deadline) {
        Err(RemoteStoreError::ReadBudgetExceeded)
    } else {
        Ok(())
    }
}

pub(super) fn finish_bounded_read<T>(
    result: Result<T, RemoteStoreError>,
    started: Instant,
    duration: Duration,
) -> Result<T, RemoteStoreError> {
    if started.elapsed() >= duration {
        Err(RemoteStoreError::ReadBudgetExceeded)
    } else {
        result
    }
}

pub(super) fn decode<T: serde::de::DeserializeOwned>(
    encoded: &[u8],
) -> Result<T, RemoteStoreError> {
    serde_json::from_slice(encoded).map_err(|_| RemoteStoreError::IntegrityFailure)
}

pub(super) fn parse_job_kind(value: &str) -> Result<IndexJobKind, RemoteStoreError> {
    match value {
        "repository_graph" => Ok(IndexJobKind::RepositoryGraph),
        "project_memory" => Ok(IndexJobKind::ProjectMemory),
        _ => Err(RemoteStoreError::IntegrityFailure),
    }
}

pub(super) fn job_kind(value: IndexJobKind) -> &'static str {
    match value {
        IndexJobKind::RepositoryGraph => "repository_graph",
        IndexJobKind::ProjectMemory => "project_memory",
    }
}

pub(super) fn i64_from_u64(value: u64) -> Result<i64, RemoteStoreError> {
    i64::try_from(value).map_err(|_| RemoteStoreError::QuotaExceeded)
}

pub(super) fn initialize_schema(connection: &Connection) -> Result<(), RemoteStoreError> {
    connection.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS remote_storage_metadata (
             singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
             schema_version INTEGER NOT NULL,
             protocol_version INTEGER NOT NULL
         );",
    )?;
    let coordinator_exists = connection
        .query_row(
            "SELECT 1 FROM sqlite_master
             WHERE type = 'table' AND name = 'distributed_coordinator_metadata'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !coordinator_exists {
        return Err(RemoteStoreError::MissingCoordinatorSchema);
    }
    let coordinator_version = connection
        .query_row(
            "SELECT schema_version FROM distributed_coordinator_metadata WHERE singleton = 1",
            [],
            |row| row.get::<_, u32>(0),
        )
        .optional()?;
    if coordinator_version != Some(COORDINATOR_SCHEMA_VERSION) {
        return Err(RemoteStoreError::IncompatibleSchema);
    }
    let jobs_exist = connection
        .query_row(
            "SELECT 1 FROM sqlite_master
             WHERE type = 'table' AND name = 'distributed_index_jobs'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !jobs_exist {
        return Err(RemoteStoreError::MissingCoordinatorSchema);
    }
    let version = connection
        .query_row(
            "SELECT schema_version, protocol_version FROM remote_storage_metadata WHERE singleton = 1",
            [],
            |row| Ok((row.get::<_, u32>(0)?, row.get::<_, u32>(1)?)),
        )
        .optional()?;
    match version {
        None => {
            connection.execute(
                "INSERT OR IGNORE INTO remote_storage_metadata
                 (singleton, schema_version, protocol_version) VALUES (1, ?1, ?2)",
                params![STORAGE_SCHEMA_VERSION, DISTRIBUTED_STORAGE_PROTOCOL_VERSION],
            )?;
        }
        Some((schema, protocol))
            if schema == STORAGE_SCHEMA_VERSION
                && protocol == DISTRIBUTED_STORAGE_PROTOCOL_VERSION => {}
        Some(_) => return Err(RemoteStoreError::IncompatibleSchema),
    }
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS project_deletion_tombstones (
             tenant_id TEXT NOT NULL,
             project_id TEXT NOT NULL,
             deletion_id TEXT NOT NULL,
             created_at_ms INTEGER NOT NULL,
             PRIMARY KEY (tenant_id, project_id)
         );
         CREATE TABLE IF NOT EXISTS remote_immutable_revisions (
             tenant_id TEXT NOT NULL,
             project_id TEXT NOT NULL,
             domain TEXT NOT NULL CHECK (
                 domain IN ('repository_graph', 'project_memory', 'memory_repository_links')
             ),
             repository_id TEXT NOT NULL,
             target_id TEXT NOT NULL,
             job_id TEXT NOT NULL,
             job_kind TEXT NOT NULL,
             build_id TEXT NOT NULL,
             extractor_digest_algorithm TEXT NOT NULL,
             extractor_digest_value TEXT NOT NULL,
             fact_digest_algorithm TEXT NOT NULL,
             fact_digest_value TEXT NOT NULL,
             primary_count INTEGER NOT NULL CHECK (primary_count >= 0),
             relationship_count INTEGER NOT NULL CHECK (relationship_count >= 0),
             diagnostic_count INTEGER NOT NULL CHECK (diagnostic_count >= 0),
             completed_at_ms INTEGER NOT NULL,
             PRIMARY KEY (tenant_id, project_id, domain, repository_id, target_id)
         );
         CREATE TABLE IF NOT EXISTS remote_encrypted_facts (
             tenant_id TEXT NOT NULL,
             project_id TEXT NOT NULL,
             domain TEXT NOT NULL,
             repository_id TEXT NOT NULL,
             target_id TEXT NOT NULL,
             fact_kind TEXT NOT NULL,
             fact_id TEXT NOT NULL,
             byte_len INTEGER NOT NULL CHECK (byte_len >= 0),
             nonce BLOB NOT NULL,
             ciphertext BLOB NOT NULL,
             PRIMARY KEY (
                 tenant_id, project_id, domain, repository_id, target_id, fact_kind, fact_id
             ),
             FOREIGN KEY (tenant_id, project_id, domain, repository_id, target_id)
                 REFERENCES remote_immutable_revisions (
                     tenant_id, project_id, domain, repository_id, target_id
                 ) ON DELETE CASCADE
         );
         CREATE TABLE IF NOT EXISTS remote_published_targets (
             tenant_id TEXT NOT NULL,
             project_id TEXT NOT NULL,
             domain TEXT NOT NULL CHECK (domain IN ('repository_graph', 'project_memory')),
             repository_id TEXT NOT NULL,
             target_id TEXT NOT NULL,
             first_published_at_ms INTEGER NOT NULL,
             PRIMARY KEY (tenant_id, project_id, domain, repository_id, target_id),
             FOREIGN KEY (tenant_id, project_id, domain, repository_id, target_id)
                 REFERENCES remote_immutable_revisions (
                     tenant_id, project_id, domain, repository_id, target_id
                 ) ON DELETE CASCADE
         );
         CREATE TABLE IF NOT EXISTS remote_publication_receipts (
             tenant_id TEXT NOT NULL,
             project_id TEXT NOT NULL,
             job_id TEXT NOT NULL,
             domain TEXT NOT NULL CHECK (domain IN ('repository_graph', 'project_memory')),
             repository_id TEXT NOT NULL,
             target_id TEXT NOT NULL,
             request_digest_algorithm TEXT NOT NULL,
             request_digest_value TEXT NOT NULL,
             outcome_json BLOB NOT NULL,
             completed_at_ms INTEGER NOT NULL,
             PRIMARY KEY (tenant_id, project_id, job_id),
             FOREIGN KEY (tenant_id, project_id, job_id)
                 REFERENCES distributed_index_jobs (tenant_id, project_id, job_id)
                 ON DELETE CASCADE,
             FOREIGN KEY (tenant_id, project_id, domain, repository_id, target_id)
                 REFERENCES remote_immutable_revisions (
                     tenant_id, project_id, domain, repository_id, target_id
                 ) ON DELETE CASCADE
         );
         CREATE TABLE IF NOT EXISTS remote_graph_views (
             tenant_id TEXT NOT NULL,
             project_id TEXT NOT NULL,
             domain TEXT NOT NULL CHECK (domain = 'repository_graph'),
             repository_id TEXT NOT NULL,
             view_name TEXT NOT NULL,
             snapshot_id TEXT NOT NULL,
             job_id TEXT NOT NULL,
             generation INTEGER NOT NULL CHECK (generation > 0),
             PRIMARY KEY (tenant_id, project_id, repository_id, view_name),
             FOREIGN KEY (tenant_id, project_id, domain, repository_id, snapshot_id)
                 REFERENCES remote_immutable_revisions (
                     tenant_id, project_id, domain, repository_id, target_id
                 ) ON DELETE RESTRICT
         );
         CREATE TABLE IF NOT EXISTS remote_graph_snapshot_metadata (
             tenant_id TEXT NOT NULL,
             project_id TEXT NOT NULL,
             domain TEXT NOT NULL DEFAULT 'repository_graph'
                 CHECK (domain = 'repository_graph'),
             repository_id TEXT NOT NULL,
             snapshot_id TEXT NOT NULL,
             repository_identity_json BLOB NOT NULL,
             PRIMARY KEY (tenant_id, project_id, repository_id, snapshot_id),
             FOREIGN KEY (tenant_id, project_id, domain, repository_id, snapshot_id)
                 REFERENCES remote_immutable_revisions (
                     tenant_id, project_id, domain, repository_id, target_id
                 ) ON DELETE CASCADE
         );
         CREATE TABLE IF NOT EXISTS remote_memory_views (
             tenant_id TEXT NOT NULL,
             project_id TEXT NOT NULL,
             domain TEXT NOT NULL CHECK (domain = 'project_memory'),
             repository_id TEXT NOT NULL CHECK (repository_id = ''),
             view_name TEXT NOT NULL,
             revision_id TEXT NOT NULL,
             job_id TEXT NOT NULL,
             generation INTEGER NOT NULL CHECK (generation > 0),
             PRIMARY KEY (tenant_id, project_id, view_name),
             FOREIGN KEY (tenant_id, project_id, domain, repository_id, revision_id)
                 REFERENCES remote_immutable_revisions (
                     tenant_id, project_id, domain, repository_id, target_id
                 ) ON DELETE RESTRICT
         );
         CREATE TABLE IF NOT EXISTS remote_memory_repository_link_sets (
             tenant_id TEXT NOT NULL,
             project_id TEXT NOT NULL,
             repository_id TEXT NOT NULL,
             memory_revision_id TEXT NOT NULL,
             snapshot_id TEXT NOT NULL,
             link_set_id TEXT NOT NULL,
             job_id TEXT NOT NULL,
             link_set_json BLOB NOT NULL,
             PRIMARY KEY (
                 tenant_id, project_id, repository_id, memory_revision_id, snapshot_id
             ),
             UNIQUE (tenant_id, project_id, repository_id, link_set_id)
         );",
    )?;
    Ok(())
}
