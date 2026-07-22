//! Recovery, refresh coordination, and conservative retention for the local sidecar.
//!
//! Runtime ownership remains in `ferrus.db`; callers pass the immutable snapshot
//! and task-view identities that are still authoritative. The sidecar never
//! infers task lifecycle from local paths or process identifiers.

use std::{
    collections::BTreeSet,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use rusqlite::{TransactionBehavior, params};
use serde::Serialize;

use super::{
    config::RetentionConfig,
    domain::{PublishedViewName, RepositoryRef, SnapshotId},
    sqlite::Sidecar,
};

const INTERRUPTED_BUILD_CODE: &str = "build.interrupted";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetentionProtection {
    pub snapshot_ids: BTreeSet<SnapshotId>,
    pub published_views: BTreeSet<PublishedViewName>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct GraphMaintenanceReport {
    pub interrupted_builds: u64,
    pub expired_refresh_leases: u64,
    pub removed_views: u64,
    pub removed_snapshots: u64,
    pub removed_builds: u64,
    pub removed_fragments: u64,
}

impl GraphMaintenanceReport {
    pub fn pending_recovery(self) -> u64 {
        self.interrupted_builds + self.expired_refresh_leases
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshLeaseOutcome {
    Acquired,
    Busy,
}

impl Sidecar {
    /// Acquires a cross-process lease for one repository view. Expired leases
    /// are reclaimed transactionally, so a crashed indexer cannot block future
    /// refreshes indefinitely.
    pub fn acquire_refresh_lease(
        &mut self,
        repository: &RepositoryRef,
        view_name: &PublishedViewName,
        owner_token: &str,
        ttl: Duration,
    ) -> Result<RefreshLeaseOutcome> {
        let now = unix_millis()?;
        let ttl_ms = i64::try_from(ttl.as_millis()).context("refresh lease TTL is too large")?;
        let expires_at = now
            .checked_add(ttl_ms)
            .context("refresh lease expiration overflow")?;
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "DELETE FROM graph_refresh_leases WHERE expires_at_ms <= ?1",
            [now],
        )?;
        let inserted = transaction.execute(
            "INSERT INTO graph_refresh_leases(\
                 repository_namespace, repository_id, view_name, owner_token, \
                 acquired_at_ms, expires_at_ms\
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(repository_namespace, repository_id, view_name) DO NOTHING",
            params![
                repository.namespace.as_str(),
                repository.repository_id.as_str(),
                view_name.as_str(),
                owner_token,
                now,
                expires_at,
            ],
        )?;
        transaction.commit()?;
        Ok(if inserted == 1 {
            RefreshLeaseOutcome::Acquired
        } else {
            RefreshLeaseOutcome::Busy
        })
    }

    pub fn release_refresh_lease(
        &mut self,
        repository: &RepositoryRef,
        view_name: &PublishedViewName,
        owner_token: &str,
    ) -> Result<bool> {
        Ok(self.connection_mut().execute(
            "DELETE FROM graph_refresh_leases \
             WHERE repository_namespace = ?1 AND repository_id = ?2 \
               AND view_name = ?3 AND owner_token = ?4",
            params![
                repository.namespace.as_str(),
                repository.repository_id.as_str(),
                view_name.as_str(),
                owner_token,
            ],
        )? == 1)
    }

    pub fn preview_recovery(&self) -> Result<GraphMaintenanceReport> {
        let now = unix_millis()?;
        let interrupted_builds = self.connection().query_row(
            "SELECT COUNT(*) FROM index_builds \
             WHERE state = 'building' AND finished_at IS NULL \
               AND NOT EXISTS (\
                   SELECT 1 FROM graph_refresh_leases AS leases \
                   WHERE leases.owner_token = index_builds.id AND leases.expires_at_ms > ?1\
               )",
            [now],
            |row| row.get::<_, i64>(0),
        )?;
        let expired_refresh_leases = self.connection().query_row(
            "SELECT COUNT(*) FROM graph_refresh_leases WHERE expires_at_ms <= ?1",
            [now],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(GraphMaintenanceReport {
            interrupted_builds: u64::try_from(interrupted_builds)
                .context("interrupted build count is negative")?,
            expired_refresh_leases: u64::try_from(expired_refresh_leases)
                .context("expired refresh lease count is negative")?,
            ..GraphMaintenanceReport::default()
        })
    }

    /// Marks only genuinely unfinished builds as failed. A build whose complete
    /// snapshot transaction committed has `finished_at` and remains reusable.
    pub fn recover_interrupted_builds(&mut self) -> Result<GraphMaintenanceReport> {
        let now_ms = unix_millis()?;
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let interrupted_builds = transaction.execute(
            "UPDATE index_builds \
             SET state = 'failed', finished_at = ?1, failure_code = ?2, failure_message = NULL \
             WHERE state = 'building' AND finished_at IS NULL \
               AND NOT EXISTS (\
                   SELECT 1 FROM graph_refresh_leases AS leases \
                   WHERE leases.owner_token = index_builds.id AND leases.expires_at_ms > ?3\
               )",
            params![timestamp(), INTERRUPTED_BUILD_CODE, now_ms],
        )? as u64;
        let expired_refresh_leases = transaction.execute(
            "DELETE FROM graph_refresh_leases WHERE expires_at_ms <= ?1",
            [now_ms],
        )? as u64;
        transaction.commit()?;
        Ok(GraphMaintenanceReport {
            interrupted_builds,
            expired_refresh_leases,
            ..GraphMaintenanceReport::default()
        })
    }

    /// Removes task publications and immutable snapshots only after the caller
    /// has excluded every active/frozen runtime reference. Canonical and other
    /// still-published snapshots are always retained independently.
    pub fn collect_garbage(
        &mut self,
        repository: &RepositoryRef,
        config: &RetentionConfig,
        protection: &RetentionProtection,
    ) -> Result<GraphMaintenanceReport> {
        let transaction = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        let task_views = {
            let mut statement = transaction.prepare(
                "SELECT view_name FROM published_views \
                 WHERE repository_namespace = ?1 AND repository_id = ?2 \
                   AND (view_name LIKE 'task-overlay:%' OR view_name LIKE 'task-baseline:%')",
            )?;
            statement
                .query_map(
                    params![
                        repository.namespace.as_str(),
                        repository.repository_id.as_str()
                    ],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let protected_views = protection
            .published_views
            .iter()
            .map(PublishedViewName::as_str)
            .collect::<BTreeSet<_>>();
        let mut removed_views = 0u64;
        for view_name in task_views {
            if !protected_views.contains(view_name.as_str()) {
                removed_views += transaction.execute(
                    "DELETE FROM published_views \
                     WHERE repository_namespace = ?1 AND repository_id = ?2 AND view_name = ?3",
                    params![
                        repository.namespace.as_str(),
                        repository.repository_id.as_str(),
                        view_name,
                    ],
                )? as u64;
            }
        }

        let published_snapshots = {
            let mut statement = transaction.prepare(
                "SELECT snapshot_id FROM published_views \
                 WHERE repository_namespace = ?1 AND repository_id = ?2",
            )?;
            statement
                .query_map(
                    params![
                        repository.namespace.as_str(),
                        repository.repository_id.as_str()
                    ],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<rusqlite::Result<BTreeSet<_>>>()?
        };
        let snapshots = {
            let mut statement = transaction.prepare(
                "SELECT id FROM snapshots \
                 WHERE repository_namespace = ?1 AND repository_id = ?2 \
                 ORDER BY created_at DESC, id DESC",
            )?;
            statement
                .query_map(
                    params![
                        repository.namespace.as_str(),
                        repository.repository_id.as_str()
                    ],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let protected_snapshots = protection
            .snapshot_ids
            .iter()
            .map(SnapshotId::as_str)
            .collect::<BTreeSet<_>>();
        let mut ordinary_retained = 0u32;
        let mut removed_snapshots = 0u64;
        for snapshot_id in snapshots {
            if published_snapshots.contains(&snapshot_id)
                || protected_snapshots.contains(snapshot_id.as_str())
            {
                continue;
            }
            if ordinary_retained < config.max_snapshots {
                ordinary_retained += 1;
                continue;
            }
            removed_snapshots +=
                transaction.execute("DELETE FROM snapshots WHERE id = ?1", [&snapshot_id])? as u64;
        }

        let removed_fragments = transaction.execute(
            "DELETE FROM fragment_cache AS cache \
             WHERE cache.repository_namespace = ?1 AND cache.repository_id = ?2 \
               AND NOT EXISTS (\
                   SELECT 1 FROM files \
                   WHERE files.path = cache.path \
                     AND files.content_algorithm = cache.content_algorithm \
                     AND files.content_digest = cache.content_digest \
                     AND files.byte_length = cache.byte_length\
               )",
            params![
                repository.namespace.as_str(),
                repository.repository_id.as_str()
            ],
        )? as u64;

        let retained_failed_builds = {
            let mut statement = transaction.prepare(
                "SELECT id FROM index_builds \
                 WHERE repository_namespace = ?1 AND repository_id = ?2 AND state = 'failed' \
                 ORDER BY started_at DESC, id DESC LIMIT ?3",
            )?;
            statement
                .query_map(
                    params![
                        repository.namespace.as_str(),
                        repository.repository_id.as_str(),
                        config.max_failed_builds,
                    ],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<rusqlite::Result<BTreeSet<_>>>()?
        };
        let orphaned_builds = {
            let mut statement = transaction.prepare(
                "SELECT builds.id, builds.state FROM index_builds AS builds \
                 WHERE builds.repository_namespace = ?1 AND builds.repository_id = ?2 \
                   AND NOT (builds.state = 'building' AND builds.finished_at IS NULL) \
                   AND NOT EXISTS (SELECT 1 FROM snapshots WHERE completed_by_build_id = builds.id) \
                   AND NOT EXISTS (SELECT 1 FROM published_views WHERE build_id = builds.id) \
                   AND NOT EXISTS (SELECT 1 FROM snapshot_diagnostic_sets WHERE build_id = builds.id)",
            )?;
            statement
                .query_map(
                    params![
                        repository.namespace.as_str(),
                        repository.repository_id.as_str()
                    ],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut removed_builds = 0u64;
        for (build_id, state) in orphaned_builds {
            if state == "failed" && retained_failed_builds.contains(&build_id) {
                continue;
            }
            removed_builds +=
                transaction.execute("DELETE FROM index_builds WHERE id = ?1", [&build_id])? as u64;
        }

        transaction.commit()?;
        Ok(GraphMaintenanceReport {
            removed_views,
            removed_snapshots,
            removed_builds,
            removed_fragments,
            ..GraphMaintenanceReport::default()
        })
    }
}

fn unix_millis() -> Result<i64> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_millis();
    i64::try_from(millis).context("Unix timestamp does not fit SQLite integer")
}

fn timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository_graph::{
        domain::{RepositoryId, RepositoryNamespace},
        sqlite::{OpenSidecarResult, open_for_build_at},
    };

    fn repository() -> RepositoryRef {
        RepositoryRef {
            namespace: RepositoryNamespace::new("local:test").unwrap(),
            repository_id: RepositoryId::new("root").unwrap(),
        }
    }

    #[test]
    fn refresh_leases_isolate_views_and_recover_after_expiration() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("repo-graph.db");
        let OpenSidecarResult::Ready(mut sidecar) = open_for_build_at(&path).unwrap() else {
            panic!("sidecar should be ready");
        };
        let repository = repository();
        let first = PublishedViewName::new("task-overlay:t-001").unwrap();
        let second = PublishedViewName::new("task-overlay:t-002").unwrap();

        assert_eq!(
            sidecar
                .acquire_refresh_lease(&repository, &first, "owner-1", Duration::from_secs(30))
                .unwrap(),
            RefreshLeaseOutcome::Acquired
        );
        assert_eq!(
            sidecar
                .acquire_refresh_lease(&repository, &first, "owner-2", Duration::from_secs(30))
                .unwrap(),
            RefreshLeaseOutcome::Busy
        );
        assert_eq!(
            sidecar
                .acquire_refresh_lease(&repository, &second, "owner-2", Duration::from_secs(30))
                .unwrap(),
            RefreshLeaseOutcome::Acquired
        );
        assert!(
            sidecar
                .release_refresh_lease(&repository, &first, "owner-1")
                .unwrap()
        );
        assert_eq!(
            sidecar
                .acquire_refresh_lease(&repository, &first, "owner-2", Duration::from_secs(30))
                .unwrap(),
            RefreshLeaseOutcome::Acquired
        );
    }

    #[test]
    fn recovery_marks_only_unfinished_builds_and_clears_expired_leases() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("repo-graph.db");
        let OpenSidecarResult::Ready(mut sidecar) = open_for_build_at(&path).unwrap() else {
            panic!("sidecar should be ready");
        };
        sidecar
            .connection_mut()
            .execute_batch(
                r#"
                INSERT INTO index_builds(
                    id, repository_namespace, repository_id, source_revision_id,
                    prospective_snapshot_id, state, started_at, finished_at
                ) VALUES
                    ('unfinished', 'local:test', 'root', 'revision-1', 'snapshot-1',
                     'building', '2026-01-01T00:00:00Z', NULL),
                    ('complete', 'local:test', 'root', 'revision-2', 'snapshot-2',
                     'building', '2026-01-01T00:00:00Z', '2026-01-01T00:00:01Z'),
                    ('active', 'local:test', 'root', 'revision-3', 'snapshot-3',
                     'building', '2026-01-01T00:00:00Z', NULL);
                INSERT INTO graph_refresh_leases(
                    repository_namespace, repository_id, view_name, owner_token,
                    acquired_at_ms, expires_at_ms
                ) VALUES
                    ('local:test', 'root', 'task-overlay:t-001', 'owner', 0, 0),
                    ('local:test', 'root', 'task-overlay:t-active', 'active', 0, 9223372036854775807);
                "#,
            )
            .unwrap();

        let preview = sidecar.preview_recovery().unwrap();
        assert_eq!(preview.interrupted_builds, 1);
        assert_eq!(preview.expired_refresh_leases, 1);
        let recovered = sidecar.recover_interrupted_builds().unwrap();
        assert_eq!(recovered.interrupted_builds, 1);
        assert_eq!(recovered.expired_refresh_leases, 1);
        assert_eq!(sidecar.preview_recovery().unwrap().pending_recovery(), 0);
        let states = sidecar
            .connection()
            .prepare("SELECT id, state FROM index_builds ORDER BY id")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            states,
            vec![
                ("active".to_string(), "building".to_string()),
                ("complete".to_string(), "building".to_string()),
                ("unfinished".to_string(), "failed".to_string()),
            ]
        );
    }

    #[test]
    fn retention_preserves_canonical_and_active_task_snapshots_only() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("repo-graph.db");
        let OpenSidecarResult::Ready(mut sidecar) = open_for_build_at(&path).unwrap() else {
            panic!("sidecar should be ready");
        };
        sidecar
            .connection_mut()
            .execute_batch(
                r#"
                INSERT INTO index_builds(
                    id, repository_namespace, repository_id, source_revision_id,
                    prospective_snapshot_id, state, started_at, finished_at
                ) VALUES
                    ('build-canonical', 'local:test', 'root', 'revision-canonical',
                     'snapshot-canonical', 'published', '2026-01-01T00:00:00Z', 'now'),
                    ('build-active', 'local:test', 'root', 'revision-active',
                     'snapshot-active', 'published', '2026-01-02T00:00:00Z', 'now'),
                    ('build-complete', 'local:test', 'root', 'revision-complete',
                     'snapshot-complete', 'published', '2026-01-03T00:00:00Z', 'now'),
                    ('failed-old', 'local:test', 'root', 'revision-failed-old',
                     'snapshot-failed-old', 'failed', '2026-01-01T00:00:00Z', 'now'),
                    ('failed-new', 'local:test', 'root', 'revision-failed-new',
                     'snapshot-failed-new', 'failed', '2026-01-02T00:00:00Z', 'now');
                INSERT INTO snapshots(
                    id, repository_namespace, repository_id, source_revision_id,
                    source_manifest_algorithm, source_manifest_digest, graph_model_version,
                    analysis_config_algorithm, analysis_config_digest,
                    extractor_set_algorithm, extractor_set_digest,
                    completed_by_build_id, created_at
                ) VALUES
                    ('snapshot-canonical', 'local:test', 'root', 'revision-canonical',
                     'sha256', '00', 1, 'sha256', '00', 'sha256', '00',
                     'build-canonical', '2026-01-01T00:00:00Z'),
                    ('snapshot-active', 'local:test', 'root', 'revision-active',
                     'sha256', '11', 1, 'sha256', '00', 'sha256', '00',
                     'build-active', '2026-01-02T00:00:00Z'),
                    ('snapshot-complete', 'local:test', 'root', 'revision-complete',
                     'sha256', '22', 1, 'sha256', '00', 'sha256', '00',
                     'build-complete', '2026-01-03T00:00:00Z');
                INSERT INTO published_views(
                    repository_namespace, repository_id, view_name, snapshot_id,
                    build_id, generation, published_at
                ) VALUES
                    ('local:test', 'root', 'canonical', 'snapshot-canonical',
                     'build-canonical', 1, 'now'),
                    ('local:test', 'root', 'task-overlay:t-active', 'snapshot-active',
                     'build-active', 1, 'now'),
                    ('local:test', 'root', 'task-overlay:t-complete', 'snapshot-complete',
                     'build-complete', 1, 'now');
                "#,
            )
            .unwrap();
        let protection = RetentionProtection {
            snapshot_ids: BTreeSet::from([SnapshotId::new("snapshot-active").unwrap()]),
            published_views: BTreeSet::from([
                PublishedViewName::new("task-overlay:t-active").unwrap()
            ]),
        };
        let report = sidecar
            .collect_garbage(
                &repository(),
                &RetentionConfig {
                    max_snapshots: 0,
                    max_failed_builds: 1,
                },
                &protection,
            )
            .unwrap();

        assert_eq!(report.removed_views, 1);
        assert_eq!(report.removed_snapshots, 1);
        let snapshots = sidecar
            .connection()
            .prepare("SELECT id FROM snapshots ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            snapshots,
            vec![
                "snapshot-active".to_string(),
                "snapshot-canonical".to_string(),
            ]
        );
        let failed = sidecar
            .connection()
            .prepare("SELECT id FROM index_builds WHERE state = 'failed' ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(failed, vec!["failed-new".to_string()]);
    }
}
