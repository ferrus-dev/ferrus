//! Read-only terminal runtime provenance discovery.

use super::*;

pub(super) fn discover_runtime(
    data_dir: &Path,
    project: &ProjectRef,
    materials: &mut Vec<SourceMaterial>,
) -> Result<()> {
    let database_path = data_dir.join("ferrus.db");
    if !database_path.is_file() {
        return Ok(());
    }

    let connection = Connection::open_with_flags(&database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut tasks = Vec::new();
    let mut task_ids = BTreeSet::new();
    {
        let mut statement = connection.prepare(
            "SELECT id, milestone_id, status, baseline_snapshot_id, \
                    repository_view_snapshot_id FROM tasks \
             WHERE spec_path IS NOT NULL AND status IN ('complete', 'failed') ORDER BY id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?;
        for row in rows {
            let (id, milestone_id, status, baseline_snapshot_id, repository_snapshot_id) = row?;
            let task = RuntimeTaskDocument {
                id,
                milestone_id,
                status,
                baseline_snapshot_id: baseline_snapshot_id
                    .map(crate::repository_graph::domain::SnapshotId::new)
                    .transpose()?,
                repository_snapshot_id: repository_snapshot_id
                    .map(crate::repository_graph::domain::SnapshotId::new)
                    .transpose()?,
            };
            task_ids.insert(task.id.clone());
            tasks.push(task);
            if tasks.len() > MAX_RUNTIME_RECORDS {
                anyhow::bail!("runtime task provenance exceeds the record budget");
            }
        }
    }
    if tasks.is_empty() {
        return Ok(());
    }

    let mut runs = Vec::new();
    {
        let mut statement = connection.prepare(
            "SELECT runs.id, runs.task_id, runs.status, runs.baseline_snapshot_id, \
                    runs.repository_view_snapshot_id FROM runs \
             JOIN tasks ON tasks.id = runs.task_id \
             WHERE tasks.spec_path IS NOT NULL AND tasks.status IN ('complete', 'failed') \
               AND runs.status IN ('completed', 'failed', 'interrupted') \
             ORDER BY runs.id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?;
        for row in rows {
            let (id, task_id, status, baseline_snapshot_id, repository_snapshot_id) = row?;
            runs.push(RuntimeRunDocument {
                id,
                task_id,
                status,
                check_ids: Vec::new(),
                baseline_snapshot_id: baseline_snapshot_id
                    .map(crate::repository_graph::domain::SnapshotId::new)
                    .transpose()?,
                repository_snapshot_id: repository_snapshot_id
                    .map(crate::repository_graph::domain::SnapshotId::new)
                    .transpose()?,
            });
            if runs.len() > MAX_RUNTIME_RECORDS {
                anyhow::bail!("runtime run provenance exceeds the record budget");
            }
        }
    }
    let run_tasks = runs
        .iter()
        .map(|run| (run.id.clone(), run.task_id.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut checks = Vec::new();
    {
        #[derive(Deserialize)]
        struct SafeEventPayload {
            pub(super) task_id: Option<String>,
        }
        let mut statement = connection.prepare(
            "SELECT id, run_id, type, payload_json FROM events \
             WHERE type IN ('task_check_passed', 'task_check_failed', \
                 'task_check_limit_exceeded', 'check_passed', 'check_failed', \
                 'submit_check_failed') ORDER BY id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (id, run_id, event_type, payload) = row?;
            if payload.len() > 64 * 1024 {
                continue;
            }

            let payload = serde_json::from_str::<SafeEventPayload>(&payload).ok();
            let task_id = match run_id.as_ref() {
                Some(run_id) => run_tasks.get(run_id).cloned(),
                None => payload.and_then(|payload| payload.task_id),
            };
            let Some(task_id) = task_id.filter(|task_id| task_ids.contains(task_id)) else {
                continue;
            };
            checks.push(RuntimeCheckDocument {
                id: format!("event:{id}"),
                task_id,
                run_id,
                status: if event_type.contains("passed") {
                    "passed".to_string()
                } else {
                    "failed".to_string()
                },
            });
            if checks.len() > MAX_RUNTIME_RECORDS {
                anyhow::bail!("runtime check provenance exceeds the record budget");
            }
        }
    }
    let checks_by_run = checks
        .iter()
        .filter_map(|check| {
            check
                .run_id
                .as_ref()
                .map(|run_id| (run_id.clone(), check.id.clone()))
        })
        .fold(
            BTreeMap::<String, Vec<String>>::new(),
            |mut map, (run, check)| {
                map.entry(run).or_default().push(check);
                map
            },
        );
    for run in &mut runs {
        run.check_ids = checks_by_run.get(&run.id).cloned().unwrap_or_default();
    }

    let document = RuntimeSourceDocument {
        tasks,
        runs,
        checks,
    };
    let bytes = serde_json::to_vec(&document)?;
    if bytes.len() > MAX_RUNTIME_SOURCE_BYTES {
        anyhow::bail!("runtime provenance exceeds the source byte budget");
    }

    materials.push(SourceMaterial {
        descriptor: AuthorizedSourceDescriptor {
            project: project.clone(),
            category: MemorySourceCategory::RuntimeProvenance,
            locator: MemorySourceLocator::RuntimeRecords {
                record_type: MemoryStatusToken::new("task-run-check-metadata")?,
            },
            fingerprint: canonical_digest(&bytes)?,
            byte_len: bytes.len() as u64,
        },
        content: MaterialContent::Sanitized(bytes),
    });
    Ok(())
}

pub(super) fn count_regular_files(path: &Path) -> Result<u64> {
    if !fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir()) {
        return Ok(0);
    }

    let Ok(entries) = fs::read_dir(path) else {
        return Ok(0);
    };
    let mut count = 0u64;
    for entry in entries {
        if entry?.file_type()?.is_file() {
            count += 1;
            if count > MAX_RUNTIME_RECORDS as u64 {
                anyhow::bail!("archive task count exceeds the record budget");
            }
        }
    }

    Ok(count)
}

pub(super) fn count_directories(path: &Path) -> Result<u64> {
    if !fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir()) {
        return Ok(0);
    }

    let Ok(entries) = fs::read_dir(path) else {
        return Ok(0);
    };
    let mut count = 0u64;
    for entry in entries {
        if entry?.file_type()?.is_dir() {
            count += 1;
            if count > MAX_RUNTIME_RECORDS as u64 {
                anyhow::bail!("archive run count exceeds the record budget");
            }
        }
    }

    Ok(count)
}
