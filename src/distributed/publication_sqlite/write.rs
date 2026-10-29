//! Validate and encrypt publication facts; enforce lease authority, receipts, and pointer updates.

use super::*;

pub(super) fn canonical_digest(value: &impl Serialize) -> Result<Digest, RemoteStoreError> {
    let encoded = serde_json::to_vec(value).map_err(|_| RemoteStoreError::Serialization)?;
    Digest::new(
        "sha256",
        sha256_value(b"ferrus.remote.fact-set.v1\0", &encoded),
    )
    .map_err(|_| RemoteStoreError::Serialization)
}

pub(super) fn graph_publication_digest(
    request: &PublishGraphRequest,
) -> Result<Digest, RemoteStoreError> {
    canonical_digest(&(
        "ferrus.remote.graph-publication.v1",
        request.protocol_version,
        &request.job,
        &request.worker_id,
        request.lease_generation,
        &request.repository,
        &request.view_name,
        &request.snapshot_id,
        &request.expected,
    ))
}

pub(super) fn memory_publication_digest(
    request: &PublishMemoryRequest,
) -> Result<Digest, RemoteStoreError> {
    canonical_digest(&(
        "ferrus.remote.memory-publication.v1",
        request.protocol_version,
        &request.job,
        &request.worker_id,
        request.lease_generation,
        &request.project,
        &request.view_name,
        &request.revision_id,
        &request.expected,
    ))
}

pub(super) fn sha256_value(domain: &[u8], bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(domain);
    digest.update(bytes);
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(super) fn encrypt_facts(
    key: &LessSafeKey,
    job: &IndexJobRef,
    domain: &str,
    target_id: &str,
    facts: Vec<PlainFact>,
    max_fact_bytes: NonZeroU64,
) -> Result<Vec<EncryptedFact>, RemoteStoreError> {
    facts
        .into_iter()
        .map(|fact| {
            let byte_len =
                u64::try_from(fact.encoded.len()).map_err(|_| RemoteStoreError::QuotaExceeded)?;
            if byte_len > max_fact_bytes.get() {
                return Err(RemoteStoreError::QuotaExceeded);
            }

            let mut nonce_bytes = [0u8; NONCE_BYTES];
            SystemRandom::new()
                .fill(&mut nonce_bytes)
                .map_err(|_| RemoteStoreError::Encryption)?;
            let nonce = Nonce::assume_unique_for_key(nonce_bytes);
            let mut ciphertext = fact.encoded;
            key.seal_in_place_append_tag(
                nonce,
                Aad::from(fact_aad(job, domain, target_id, fact.kind, &fact.id)),
                &mut ciphertext,
            )
            .map_err(|_| RemoteStoreError::Encryption)?;
            Ok(EncryptedFact {
                kind: fact.kind,
                id: fact.id,
                byte_len,
                nonce: nonce_bytes,
                ciphertext,
            })
        })
        .collect()
}

pub(super) fn fact_aad(
    job: &IndexJobRef,
    domain: &str,
    target_id: &str,
    fact_kind: &str,
    fact_id: &str,
) -> Vec<u8> {
    format!(
        "{}\0{}\0{}\0{}\0{}\0{}\0{}",
        job.project.tenant_id,
        job.project.project_id,
        job.job_id,
        domain,
        target_id,
        fact_kind,
        fact_id
    )
    .into_bytes()
}

pub(super) struct JobAuthority {
    pub(super) spec: IndexJobSpec,
}

fn ensure_project_not_deleted(
    connection: &Connection,
    project: &RemoteProjectRef,
) -> Result<(), RemoteStoreError> {
    if connection
        .query_row(
            "SELECT 1 FROM project_deletion_tombstones
             WHERE tenant_id = ?1 AND project_id = ?2",
            params![project.tenant_id.as_str(), project.project_id.as_str()],
            |_| Ok(()),
        )
        .optional()?
        .is_some()
    {
        return Err(RemoteStoreError::ProjectDeleted);
    }

    Ok(())
}

fn replay_publication<T>(
    connection: &Connection,
    job: &IndexJobRef,
    domain: &str,
    repository_id: &str,
    target_id: &str,
    publication_digest: &Digest,
) -> Result<Option<T>, RemoteStoreError>
where
    T: serde::de::DeserializeOwned,
{
    ensure_project_not_deleted(connection, &job.project)?;
    let encoded = connection
        .query_row(
            "SELECT receipt.outcome_json
             FROM remote_publication_receipts AS receipt
             JOIN distributed_index_jobs AS job
               ON job.tenant_id = receipt.tenant_id
              AND job.project_id = receipt.project_id
              AND job.job_id = receipt.job_id
             WHERE receipt.tenant_id = ?1 AND receipt.project_id = ?2
               AND receipt.job_id = ?3 AND receipt.domain = ?4
               AND receipt.repository_id = ?5 AND receipt.target_id = ?6
               AND receipt.request_digest_algorithm = ?7
               AND receipt.request_digest_value = ?8
               AND job.state = 'complete'",
            params![
                job.project.tenant_id.as_str(),
                job.project.project_id.as_str(),
                job.job_id.as_str(),
                domain,
                repository_id,
                target_id,
                publication_digest.algorithm(),
                publication_digest.value()
            ],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?;
    encoded
        .map(|value| serde_json::from_slice(&value).map_err(|_| RemoteStoreError::IntegrityFailure))
        .transpose()
}

pub(super) fn replay_graph_publication(
    connection: &Connection,
    request: &PublishGraphRequest,
    publication_digest: &Digest,
) -> Result<Option<GraphPublicationOutcome>, RemoteStoreError> {
    replay_publication::<GraphPublicationOutcome>(
        connection,
        &request.job,
        "repository_graph",
        request.repository.repository_id.as_str(),
        request.snapshot_id.as_str(),
        publication_digest,
    )
}

pub(super) fn replay_memory_publication(
    connection: &Connection,
    request: &PublishMemoryRequest,
    publication_digest: &Digest,
) -> Result<Option<MemoryPublicationOutcome>, RemoteStoreError> {
    replay_publication::<MemoryPublicationOutcome>(
        connection,
        &request.job,
        "project_memory",
        "",
        request.revision_id.as_str(),
        publication_digest,
    )
}

struct PublicationReceiptTarget<'a> {
    domain: &'a str,
    repository_id: &'a str,
    target_id: &'a str,
}

fn record_publication<T: Serialize>(
    transaction: &Transaction<'_>,
    job: &IndexJobRef,
    target: PublicationReceiptTarget<'_>,
    publication_digest: &Digest,
    outcome: &T,
    now: DateTime<Utc>,
) -> Result<(), RemoteStoreError> {
    let encoded = serde_json::to_vec(outcome).map_err(|_| RemoteStoreError::Serialization)?;
    transaction.execute(
        "INSERT INTO remote_publication_receipts (
             tenant_id, project_id, job_id, domain, repository_id, target_id,
             request_digest_algorithm, request_digest_value, outcome_json, completed_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            job.project.tenant_id.as_str(),
            job.project.project_id.as_str(),
            job.job_id.as_str(),
            target.domain,
            target.repository_id,
            target.target_id,
            publication_digest.algorithm(),
            publication_digest.value(),
            encoded,
            now.timestamp_millis()
        ],
    )?;
    Ok(())
}

pub(super) fn record_graph_publication(
    transaction: &Transaction<'_>,
    request: &PublishGraphRequest,
    publication_digest: &Digest,
    outcome: &GraphPublicationOutcome,
    now: DateTime<Utc>,
) -> Result<(), RemoteStoreError> {
    record_publication(
        transaction,
        &request.job,
        PublicationReceiptTarget {
            domain: "repository_graph",
            repository_id: request.repository.repository_id.as_str(),
            target_id: request.snapshot_id.as_str(),
        },
        publication_digest,
        outcome,
        now,
    )
}

pub(super) fn record_memory_publication(
    transaction: &Transaction<'_>,
    request: &PublishMemoryRequest,
    publication_digest: &Digest,
    outcome: &MemoryPublicationOutcome,
    now: DateTime<Utc>,
) -> Result<(), RemoteStoreError> {
    record_publication(
        transaction,
        &request.job,
        PublicationReceiptTarget {
            domain: "project_memory",
            repository_id: "",
            target_id: request.revision_id.as_str(),
        },
        publication_digest,
        outcome,
        now,
    )
}

pub(super) fn require_publication_authority(
    transaction: &Transaction<'_>,
    job: &IndexJobRef,
    worker_id: &WorkerId,
    lease_generation: NonZeroU64,
    now: DateTime<Utc>,
) -> Result<JobAuthority, RemoteStoreError> {
    ensure_project_not_deleted(transaction, &job.project)?;
    let record = transaction
        .query_row(
            "SELECT kind, spec_json, state, cancellation_requested, lease_worker_id,
                    lease_generation, lease_until_ms, deadline_at_ms
             FROM distributed_index_jobs
             WHERE tenant_id = ?1 AND project_id = ?2 AND job_id = ?3",
            params![
                job.project.tenant_id.as_str(),
                job.project.project_id.as_str(),
                job.job_id.as_str()
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, bool>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            },
        )
        .optional()?
        .ok_or(RemoteStoreError::AuthorityLost)?;
    let kind = parse_job_kind(&record.0)?;
    let spec: IndexJobSpec =
        serde_json::from_slice(&record.1).map_err(|_| RemoteStoreError::IntegrityFailure)?;
    spec.validate()
        .map_err(|_| RemoteStoreError::IntegrityFailure)?;
    let live = kind == job.kind
        && spec.project() == &job.project
        && record.2 == "publishing"
        && !record.3
        && record.4.as_deref() == Some(worker_id.as_str())
        && u64::try_from(record.5).ok() == Some(lease_generation.get())
        && record
            .6
            .is_some_and(|expires| expires > now.timestamp_millis())
        && record.7 > now.timestamp_millis();
    if !live {
        return Err(RemoteStoreError::AuthorityLost);
    }

    Ok(JobAuthority { spec })
}

pub(super) trait PublicationLease {
    fn job(&self) -> &IndexJobRef;

    fn worker_id(&self) -> &WorkerId;

    fn lease_generation(&self) -> NonZeroU64;
}

impl PublicationLease for PublishGraphRequest {
    fn job(&self) -> &IndexJobRef {
        &self.job
    }

    fn worker_id(&self) -> &WorkerId {
        &self.worker_id
    }

    fn lease_generation(&self) -> NonZeroU64 {
        self.lease_generation
    }
}

impl PublicationLease for PublishMemoryRequest {
    fn job(&self) -> &IndexJobRef {
        &self.job
    }

    fn worker_id(&self) -> &WorkerId {
        &self.worker_id
    }

    fn lease_generation(&self) -> NonZeroU64 {
        self.lease_generation
    }
}

pub(super) fn complete_job(
    transaction: &Transaction<'_>,
    request: &impl PublicationLease,
    now: DateTime<Utc>,
) -> Result<(), RemoteStoreError> {
    let changed = transaction.execute(
        "UPDATE distributed_index_jobs
         SET state = 'complete', lease_worker_id = NULL, lease_until_ms = NULL,
             failure_code = NULL, updated_at_ms = ?1
         WHERE tenant_id = ?2 AND project_id = ?3 AND job_id = ?4
           AND state = 'publishing' AND cancellation_requested = 0
           AND lease_worker_id = ?5 AND lease_generation = ?6
           AND lease_until_ms > ?1 AND deadline_at_ms > ?1",
        params![
            now.timestamp_millis(),
            request.job().project.tenant_id.as_str(),
            request.job().project.project_id.as_str(),
            request.job().job_id.as_str(),
            request.worker_id().as_str(),
            i64::try_from(request.lease_generation().get())
                .map_err(|_| RemoteStoreError::InvalidInput)?
        ],
    )?;
    if changed != 1 {
        return Err(RemoteStoreError::AuthorityLost);
    }

    Ok(())
}

pub(super) fn insert_graph_snapshot(
    transaction: &Transaction<'_>,
    record: &RemoteGraphSnapshotRecord,
    facts: &[EncryptedFact],
    limits: RemoteStoreLimits,
) -> Result<bool, RemoteStoreError> {
    let reused = insert_revision(
        transaction,
        &record.snapshot.repository.project,
        "repository_graph",
        record.snapshot.repository.repository_id.as_str(),
        record.snapshot.snapshot_id.as_str(),
        &record.job,
        record.build_id.as_str(),
        &record.extractor_set_digest,
        &record.fact_set_digest,
        record.counts,
        record.completed_at,
        facts,
        limits,
    )?;
    let encoded = serde_json::to_vec(&record.repository_identity)
        .map_err(|_| RemoteStoreError::Serialization)?;
    let existing = transaction
        .query_row(
            "SELECT repository_identity_json FROM remote_graph_snapshot_metadata
             WHERE tenant_id = ?1 AND project_id = ?2 AND repository_id = ?3
               AND snapshot_id = ?4",
            params![
                record.snapshot.repository.project.tenant_id.as_str(),
                record.snapshot.repository.project.project_id.as_str(),
                record.snapshot.repository.repository_id.as_str(),
                record.snapshot.snapshot_id.as_str()
            ],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?;
    if let Some(existing) = existing {
        if existing != encoded {
            return Err(RemoteStoreError::ImmutableConflict);
        }
    } else {
        transaction.execute(
            "INSERT INTO remote_graph_snapshot_metadata (
                 tenant_id, project_id, repository_id, snapshot_id, repository_identity_json
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                record.snapshot.repository.project.tenant_id.as_str(),
                record.snapshot.repository.project.project_id.as_str(),
                record.snapshot.repository.repository_id.as_str(),
                record.snapshot.snapshot_id.as_str(),
                encoded
            ],
        )?;
    }

    Ok(reused)
}

pub(super) fn insert_memory_revision(
    transaction: &Transaction<'_>,
    record: &RemoteMemoryRevisionRecord,
    facts: &[EncryptedFact],
    limits: RemoteStoreLimits,
) -> Result<bool, RemoteStoreError> {
    insert_revision(
        transaction,
        &record.revision.project,
        "project_memory",
        "",
        record.revision.revision_id.as_str(),
        &record.job,
        record.build_id.as_str(),
        &record.extractor_set_digest,
        &record.fact_set_digest,
        record.counts,
        record.completed_at,
        facts,
        limits,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn insert_memory_repository_links(
    transaction: &Transaction<'_>,
    project: &RemoteProjectRef,
    target: &RemoteMemoryLinkSetTarget,
    job: &IndexJobRef,
    build_id: &MemoryBuildId,
    extractor_set_digest: &Digest,
    fact_set_digest: &Digest,
    counts: RemoteFactCounts,
    completed_at: DateTime<Utc>,
    facts: &[EncryptedFact],
    limits: RemoteStoreLimits,
) -> Result<(), RemoteStoreError> {
    insert_revision(
        transaction,
        project,
        "memory_repository_links",
        target.graph.repository.repository_id.as_str(),
        target.link_set.id.as_str(),
        job,
        build_id.as_str(),
        extractor_set_digest,
        fact_set_digest,
        counts,
        completed_at,
        facts,
        limits,
    )?;
    let stored_job = load_revision_row(
        transaction,
        project,
        "memory_repository_links",
        target.graph.repository.repository_id.as_str(),
        target.link_set.id.as_str(),
    )?
    .ok_or(RemoteStoreError::IntegrityFailure)?
    .job_id;
    let encoded = serde_json::to_vec(target).map_err(|_| RemoteStoreError::Serialization)?;
    let existing = transaction
        .query_row(
            "SELECT link_set_id, link_set_json
             FROM remote_memory_repository_link_sets
             WHERE tenant_id = ?1 AND project_id = ?2 AND repository_id = ?3
               AND memory_revision_id = ?4 AND snapshot_id = ?5",
            params![
                project.tenant_id.as_str(),
                project.project_id.as_str(),
                target.graph.repository.repository_id.as_str(),
                target.link_set.memory_revision_id.as_str(),
                target.graph.snapshot_id.as_str()
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?;
    if let Some((link_set_id, link_set_json)) = existing {
        let stored_target = decode_memory_link_target(&link_set_json, &target.graph)?;
        if link_set_id != target.link_set.id.as_str() || stored_target != *target {
            return Err(RemoteStoreError::ImmutableConflict);
        }

        return Ok(());
    }

    transaction.execute(
        "INSERT INTO remote_memory_repository_link_sets (
             tenant_id, project_id, repository_id, memory_revision_id, snapshot_id,
             link_set_id, job_id, link_set_json
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            project.tenant_id.as_str(),
            project.project_id.as_str(),
            target.graph.repository.repository_id.as_str(),
            target.link_set.memory_revision_id.as_str(),
            target.graph.snapshot_id.as_str(),
            target.link_set.id.as_str(),
            stored_job.as_str(),
            encoded
        ],
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn insert_revision(
    transaction: &Transaction<'_>,
    project: &RemoteProjectRef,
    domain: &str,
    repository_id: &str,
    target_id: &str,
    job: &IndexJobRef,
    build_id: &str,
    extractor_set_digest: &Digest,
    fact_set_digest: &Digest,
    counts: RemoteFactCounts,
    completed_at: DateTime<Utc>,
    facts: &[EncryptedFact],
    limits: RemoteStoreLimits,
) -> Result<bool, RemoteStoreError> {
    let existing = transaction
        .query_row(
            "SELECT fact_digest_algorithm, fact_digest_value, extractor_digest_algorithm,
                    extractor_digest_value, primary_count, relationship_count, diagnostic_count
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
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            },
        )
        .optional()?;
    if let Some(existing) = existing {
        let same = existing.0 == fact_set_digest.algorithm()
            && existing.1 == fact_set_digest.value()
            && existing.2 == extractor_set_digest.algorithm()
            && existing.3 == extractor_set_digest.value()
            && u64::try_from(existing.4).ok() == Some(counts.primary)
            && u64::try_from(existing.5).ok() == Some(counts.relationships)
            && u64::try_from(existing.6).ok() == Some(counts.diagnostics);
        return same
            .then_some(true)
            .ok_or(RemoteStoreError::ImmutableConflict);
    }

    enforce_project_quota(transaction, project, facts, limits)?;
    transaction.execute(
        "INSERT INTO remote_immutable_revisions (
             tenant_id, project_id, domain, repository_id, target_id, job_id, job_kind,
             build_id, extractor_digest_algorithm, extractor_digest_value,
             fact_digest_algorithm, fact_digest_value, primary_count, relationship_count,
             diagnostic_count, completed_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        params![
            project.tenant_id.as_str(),
            project.project_id.as_str(),
            domain,
            repository_id,
            target_id,
            job.job_id.as_str(),
            job_kind(job.kind),
            build_id,
            extractor_set_digest.algorithm(),
            extractor_set_digest.value(),
            fact_set_digest.algorithm(),
            fact_set_digest.value(),
            i64_from_u64(counts.primary)?,
            i64_from_u64(counts.relationships)?,
            i64_from_u64(counts.diagnostics)?,
            completed_at.timestamp_millis()
        ],
    )?;
    for fact in facts {
        transaction.execute(
            "INSERT INTO remote_encrypted_facts (
                 tenant_id, project_id, domain, repository_id, target_id, fact_kind,
                 fact_id, byte_len, nonce, ciphertext
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                project.tenant_id.as_str(),
                project.project_id.as_str(),
                domain,
                repository_id,
                target_id,
                fact.kind,
                fact.id,
                i64_from_u64(fact.byte_len)?,
                fact.nonce.as_slice(),
                fact.ciphertext
            ],
        )?;
    }

    Ok(false)
}

pub(super) fn enforce_project_quota(
    transaction: &Transaction<'_>,
    project: &RemoteProjectRef,
    facts: &[EncryptedFact],
    limits: RemoteStoreLimits,
) -> Result<(), RemoteStoreError> {
    let (snapshots, stored_facts, stored_bytes): (i64, i64, i64) = transaction.query_row(
        "SELECT
             (SELECT COUNT(*) FROM remote_immutable_revisions
              WHERE tenant_id = ?1 AND project_id = ?2),
             (SELECT COUNT(*) FROM remote_encrypted_facts
              WHERE tenant_id = ?1 AND project_id = ?2),
             (SELECT COALESCE(SUM(length(ciphertext)), 0) FROM remote_encrypted_facts
              WHERE tenant_id = ?1 AND project_id = ?2)",
        params![project.tenant_id.as_str(), project.project_id.as_str()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let incoming_facts = u64::try_from(facts.len()).map_err(|_| RemoteStoreError::QuotaExceeded)?;
    let incoming_bytes = facts
        .iter()
        .try_fold(0u64, |total, fact| {
            total.checked_add(u64::try_from(fact.ciphertext.len()).ok()?)
        })
        .ok_or(RemoteStoreError::QuotaExceeded)?;
    if u64::try_from(snapshots)
        .ok()
        .is_none_or(|value| value >= limits.max_snapshots_per_project.get())
        || u64::try_from(stored_facts).ok().is_none_or(|value| {
            value.saturating_add(incoming_facts) > limits.max_facts_per_project.get()
        })
        || u64::try_from(stored_bytes).ok().is_none_or(|value| {
            value.saturating_add(incoming_bytes) > limits.max_bytes_per_project.get()
        })
    {
        return Err(RemoteStoreError::QuotaExceeded);
    }

    Ok(())
}

pub(super) fn graph_expected_matches(
    expected: Option<&GraphPublicationVersion>,
    actual: Option<&PublishedRemoteGraphView>,
) -> bool {
    match (expected, actual) {
        (None, None) => true,
        (Some(expected), Some(actual)) => {
            expected.snapshot_id == actual.snapshot_id && expected.generation == actual.generation
        }
        _ => false,
    }
}

pub(super) fn memory_expected_matches(
    expected: Option<&MemoryPublicationVersion>,
    actual: Option<&PublishedRemoteMemoryView>,
) -> bool {
    match (expected, actual) {
        (None, None) => true,
        (Some(expected), Some(actual)) => {
            expected.revision_id == actual.revision_id && expected.generation == actual.generation
        }
        _ => false,
    }
}

pub(super) fn next_generation(actual: Option<NonZeroU64>) -> Result<NonZeroU64, RemoteStoreError> {
    let generation = actual
        .map(NonZeroU64::get)
        .unwrap_or(0)
        .checked_add(1)
        .ok_or(RemoteStoreError::IntegrityFailure)?;
    NonZeroU64::new(generation).ok_or(RemoteStoreError::IntegrityFailure)
}

pub(super) fn mark_published_target(
    transaction: &Transaction<'_>,
    project: &RemoteProjectRef,
    domain: &str,
    repository_id: &str,
    target_id: &str,
    published_at: DateTime<Utc>,
) -> Result<(), RemoteStoreError> {
    transaction.execute(
        "INSERT INTO remote_published_targets (
             tenant_id, project_id, domain, repository_id, target_id, first_published_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (tenant_id, project_id, domain, repository_id, target_id) DO NOTHING",
        params![
            project.tenant_id.as_str(),
            project.project_id.as_str(),
            domain,
            repository_id,
            target_id,
            published_at.timestamp_millis()
        ],
    )?;
    Ok(())
}

pub(super) fn upsert_graph_view(
    transaction: &Transaction<'_>,
    view: &PublishedRemoteGraphView,
) -> Result<(), RemoteStoreError> {
    transaction.execute(
        "INSERT INTO remote_graph_views (
             tenant_id, project_id, domain, repository_id, view_name, snapshot_id, job_id,
             generation
         ) VALUES (?1, ?2, 'repository_graph', ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (tenant_id, project_id, repository_id, view_name) DO UPDATE SET
             snapshot_id = excluded.snapshot_id,
             job_id = excluded.job_id,
             generation = excluded.generation",
        params![
            view.repository.project.tenant_id.as_str(),
            view.repository.project.project_id.as_str(),
            view.repository.repository_id.as_str(),
            view.view_name.as_str(),
            view.snapshot_id.as_str(),
            view.job.job_id.as_str(),
            i64_from_u64(view.generation.get())?
        ],
    )?;
    Ok(())
}

pub(super) fn upsert_memory_view(
    transaction: &Transaction<'_>,
    view: &PublishedRemoteMemoryView,
) -> Result<(), RemoteStoreError> {
    transaction.execute(
        "INSERT INTO remote_memory_views (
             tenant_id, project_id, domain, repository_id, view_name, revision_id, job_id,
             generation
         ) VALUES (?1, ?2, 'project_memory', '', ?3, ?4, ?5, ?6)
         ON CONFLICT (tenant_id, project_id, view_name) DO UPDATE SET
             revision_id = excluded.revision_id,
             job_id = excluded.job_id,
             generation = excluded.generation",
        params![
            view.project.tenant_id.as_str(),
            view.project.project_id.as_str(),
            view.view_name.as_str(),
            view.revision_id.as_str(),
            view.job.job_id.as_str(),
            i64_from_u64(view.generation.get())?
        ],
    )?;
    Ok(())
}

mod prepare;
pub(super) use prepare::*;
