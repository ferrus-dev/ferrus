use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GraphDoctorObservation {
    pub(crate) healthy: bool,
    pub(crate) message: String,
}

pub(crate) async fn maintain_graph_best_effort() {
    if let Err(error) = maintain_graph().await {
        tracing::warn!(
            error = ?error,
            "repository graph maintenance failed; orchestration lifecycle is unchanged"
        );
    }
}

pub(crate) async fn maintain_graph() -> Result<GraphMaintenanceReport> {
    let (config, repository, path) = graph_maintenance_context().await?;
    if !path.exists() {
        return Ok(GraphMaintenanceReport::default());
    }
    let references = project::repository_graph_retention_references().await?;
    let protection = RetentionProtection {
        snapshot_ids: references.snapshot_ids,
        published_views: references.view_names,
    };
    let telemetry_enabled = config.telemetry.enabled;
    let retention = config.retention.clone();
    let metric_repository = repository.clone();
    let report = tokio::task::spawn_blocking(move || -> Result<_> {
        let mut sidecar = match open_for_build_at(&path)? {
            OpenSidecarResult::Ready(sidecar) => sidecar,
            OpenSidecarResult::RequiresRebuild(reason) => anyhow::bail!(
                "repository graph sidecar schema {} is incompatible with {}",
                reason.found_schema_version,
                reason.supported_schema_version
            ),
        };
        let recovery = sidecar.recover_interrupted_builds()?;
        let retention = sidecar.collect_garbage(&repository, &retention, &protection)?;
        Ok(GraphMaintenanceReport {
            interrupted_builds: recovery.interrupted_builds,
            expired_refresh_leases: recovery.expired_refresh_leases,
            ..retention
        })
    })
    .await??;
    if telemetry_enabled {
        let encoded = serde_json::to_string(&report)
            .expect("privacy-safe graph maintenance metrics are always serializable");
        tracing::info!(
            target: "ferrus::repository_graph::maintenance",
            repository_namespace = metric_repository.namespace.as_str(),
            repository_id = metric_repository.repository_id.as_str(),
            metric = %encoded,
            "repository graph maintenance"
        );
    }
    Ok(report)
}

pub(crate) async fn preview_graph_recovery() -> Result<GraphMaintenanceReport> {
    let (_, _, path) = graph_maintenance_context().await?;
    tokio::task::spawn_blocking(move || -> Result<_> {
        match open_for_query_at(&path)? {
            OpenQuerySidecarResult::Ready(sidecar) => sidecar.preview_recovery(),
            OpenQuerySidecarResult::Absent
            | OpenQuerySidecarResult::NeedsMigration { .. }
            | OpenQuerySidecarResult::RequiresRebuild(_) => Ok(GraphMaintenanceReport::default()),
        }
    })
    .await?
}

pub(crate) async fn recover_graph_state() -> Result<GraphMaintenanceReport> {
    maintain_graph().await
}

pub(crate) async fn graph_doctor_observations() -> Vec<GraphDoctorObservation> {
    match graph_maintenance_context().await {
        Ok((config, _, _)) if !config.enabled => {
            return vec![GraphDoctorObservation {
                healthy: true,
                message: "optional repository graph is disabled".to_string(),
            }];
        }
        Ok(_) => {}
        Err(error) => {
            return vec![GraphDoctorObservation {
                healthy: false,
                message: format!("repository graph configuration is unavailable ({error})"),
            }];
        }
    }
    let mut observations = Vec::new();
    match project::canonical_graph_reference().await {
        Ok(reference) => observations.push(GraphDoctorObservation {
            healthy: reference.status != project::CanonicalGraphStatus::Stale,
            message: match reference.status {
                project::CanonicalGraphStatus::Unknown => {
                    "canonical repository graph freshness has not been recorded".to_string()
                }
                project::CanonicalGraphStatus::Stale => {
                    "canonical repository graph is stale; run `ferrus graph index`".to_string()
                }
                project::CanonicalGraphStatus::Fresh => {
                    "canonical repository graph has a recorded fresh snapshot".to_string()
                }
            },
        }),
        Err(error) => observations.push(GraphDoctorObservation {
            healthy: false,
            message: format!("canonical repository graph state is unreadable ({error})"),
        }),
    }
    match preview_graph_recovery().await {
        Ok(report) => observations.push(GraphDoctorObservation {
            healthy: report.pending_recovery() == 0,
            message: format!(
                "repository graph recovery pending: {} interrupted builds, {} expired refresh leases{}",
                report.interrupted_builds,
                report.expired_refresh_leases,
                if report.pending_recovery() == 0 {
                    ""
                } else {
                    "; run `ferrus recover`"
                }
            ),
        }),
        Err(error) => observations.push(GraphDoctorObservation {
            healthy: false,
            message: format!("repository graph recovery state is unreadable ({error})"),
        }),
    }
    observations
}

async fn graph_maintenance_context()
-> Result<(RepositoryGraphConfig, RepositoryRef, std::path::PathBuf)> {
    let root = project::canonical_project_root().await?;
    let contents = tokio::fs::read_to_string(root.join("ferrus.toml"))
        .await
        .context("ferrus.toml not found while maintaining repository graph")?;
    let config = RepositoryGraphConfig::from_ferrus_toml(&contents)
        .context("Invalid [repository_graph] configuration")?;
    let project_id = project::current_project_id().await?;
    let repository = RepositoryRef {
        namespace: RepositoryNamespace::new(format!("local:{project_id}"))?,
        repository_id: RepositoryId::new("root")?,
    };
    Ok((config, repository, sidecar_path().await?))
}

async fn canonical_source_at(
    root: &Path,
) -> Result<Option<(RepositoryGraphConfig, RepositoryRef, LocalRepositorySource)>> {
    let contents = tokio::fs::read_to_string(root.join("ferrus.toml"))
        .await
        .context("ferrus.toml not found while observing canonical source")?;
    let config = RepositoryGraphConfig::from_ferrus_toml(&contents)
        .context("Invalid [repository_graph] configuration")?;
    if !config.enabled {
        return Ok(None);
    }
    let project_id = project::current_project_id().await?;
    let repository = RepositoryRef {
        namespace: RepositoryNamespace::new(format!("local:{project_id}"))?,
        repository_id: RepositoryId::new("root")?,
    };
    let root = root.to_path_buf();
    let discovery_config = config.clone();
    let discovery_repository = repository.clone();
    let source = tokio::task::spawn_blocking(move || -> Result<LocalRepositorySource> {
        let identities = active_extractor_identities(&discovery_config)?;
        let context = SourceDiscoveryContext::from_config(
            discovery_repository,
            &discovery_config,
            &identities,
        )?;
        Ok(LocalRepositorySource::discover(root, context)?)
    })
    .await??;
    Ok(Some((config, repository, source)))
}

pub(crate) async fn canonical_source_identity_at(
    root: &Path,
) -> Result<Option<project::CanonicalSourceIdentity>> {
    Ok(canonical_source_at(root)
        .await?
        .map(|(_, _, source)| project::CanonicalSourceIdentity {
            source_revision_id: source.manifest().revision.id.clone(),
            manifest_digest: source.manifest().revision.manifest_digest.clone(),
        }))
}

pub(crate) async fn refresh_canonical_graph_after_approval(
    project_root: std::path::PathBuf,
    task_id: String,
    run_id: Option<String>,
) {
    loop {
        let guard = match project::canonical_graph_refresh_guard().await {
            Ok(guard) => guard,
            Err(error) => {
                tracing::warn!(
                    task_id,
                    error = ?error,
                    "failed to capture canonical graph refresh generation"
                );
                return;
            }
        };
        match refresh_canonical_graph_at(&project_root).await {
            Ok(None) => return,
            Ok(Some((source, snapshot_id, build_id))) => {
                match project::record_canonical_graph_refresh(
                    Some(&task_id),
                    run_id.as_deref(),
                    guard,
                    &source,
                    &snapshot_id,
                    &build_id,
                )
                .await
                {
                    Ok(project::CanonicalGraphRefreshOutcome::Recorded) => {}
                    Ok(project::CanonicalGraphRefreshOutcome::Superseded) => tracing::debug!(
                        task_id,
                        "canonical graph refresh was superseded by a newer invalidation"
                    ),
                    Err(error) => tracing::warn!(
                        task_id,
                        error = ?error,
                        "canonical graph refreshed but durable freshness state was not updated"
                    ),
                }
                maintain_graph_best_effort().await;
                return;
            }
            Err(error) if error.downcast_ref::<RefreshAlreadyInProgress>().is_some() => {
                tracing::debug!(
                    task_id,
                    "waiting for the active canonical repository graph refresh"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(error) => {
                tracing::warn!(
                    task_id,
                    error = ?error,
                    "best-effort canonical graph refresh failed after approval"
                );
                project::record_canonical_graph_refresh_failed_best_effort(
                    &task_id,
                    run_id.as_deref(),
                    guard,
                )
                .await;
                return;
            }
        }
    }
}

async fn refresh_canonical_graph_at(
    project_root: &Path,
) -> Result<
    Option<(
        project::CanonicalSourceIdentity,
        repository_graph::domain::SnapshotId,
        BuildId,
    )>,
> {
    let contents = tokio::fs::read_to_string(project_root.join("ferrus.toml"))
        .await
        .context("ferrus.toml not found while refreshing canonical source")?;
    let config = RepositoryGraphConfig::from_ferrus_toml(&contents)
        .context("Invalid [repository_graph] configuration")?;
    if !config.enabled {
        return Ok(None);
    }
    let project_id = project::current_project_id().await?;
    let repository = RepositoryRef {
        namespace: RepositoryNamespace::new(format!("local:{project_id}"))?,
        repository_id: RepositoryId::new("root")?,
    };
    let sidecar_path = sidecar_path().await?;
    let indexed_repository = repository.clone();
    let project_root = project_root.to_path_buf();
    let (source_identity, outcome) = tokio::task::spawn_blocking(move || -> Result<_> {
        let mut sidecar = match open_for_build_at(&sidecar_path)? {
            OpenSidecarResult::Ready(sidecar) => sidecar,
            OpenSidecarResult::RequiresRebuild(reason) => anyhow::bail!(
                "repository graph sidecar schema {} is incompatible with {}",
                reason.found_schema_version,
                reason.supported_schema_version
            ),
        };
        let build_id = next_canonical_refresh_build_id()?;
        let view_name = PublishedViewName::new(CANONICAL_VIEW)?;
        if sidecar.acquire_refresh_lease(
            &indexed_repository,
            &view_name,
            build_id.as_str(),
            REFRESH_LEASE_TTL,
        )? == RefreshLeaseOutcome::Busy
        {
            return Err(RefreshAlreadyInProgress.into());
        }
        let heartbeat = sidecar.start_refresh_lease_heartbeat(
            &indexed_repository,
            &view_name,
            build_id.as_str(),
            REFRESH_LEASE_TTL,
        )?;
        let indexed = (|| -> Result<_> {
            let identities = active_extractor_identities(&config)?;
            let context = SourceDiscoveryContext::from_config(
                indexed_repository.clone(),
                &config,
                &identities,
            )?;
            let source = LocalRepositorySource::discover(project_root, context)?;
            let source_identity = project::CanonicalSourceIdentity {
                source_revision_id: source.manifest().revision.id.clone(),
                manifest_digest: source.manifest().revision.manifest_digest.clone(),
            };
            let outcome = IndexCoordinator::new(&mut sidecar).index(
                &source,
                &config,
                IndexRequest {
                    build_id: build_id.clone(),
                    view_name: view_name.clone(),
                    force_full: false,
                },
            )?;
            Ok((source_identity, outcome))
        })();
        let lease_healthy = heartbeat.finish();
        let released =
            sidecar.release_refresh_lease(&indexed_repository, &view_name, build_id.as_str());
        let indexed = indexed?;
        if !lease_healthy || !released? {
            anyhow::bail!("canonical repository graph refresh lease was lost");
        }
        Ok(indexed)
    })
    .await??;
    debug_assert_eq!(outcome.snapshot.repository, repository);
    Ok(Some((
        source_identity,
        outcome.snapshot.id,
        outcome.build_id,
    )))
}

fn next_canonical_refresh_build_id() -> Result<BuildId> {
    let sequence = CANONICAL_REFRESH_BUILD_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Ok(BuildId::new(format!(
        "canonical-approval:{nanos:x}:{sequence:x}"
    ))?)
}

pub(crate) async fn schedule_task_baseline_pin(
    task_id: &str,
    workspace_root: &Path,
    baseline_tree: Option<&str>,
) -> Option<tokio::task::JoinHandle<()>> {
    let existing = project::task_repository_view(task_id).await.ok().flatten();
    let prepared = match (
        LocalGraphContext::load(false).await,
        project::current_project_data_dir().await,
    ) {
        (Ok(context), Ok(data_dir)) => Some((
            context,
            data_dir.join(SIDECAR_FILE_NAME),
            data_dir.join("ferrus.db"),
        )),
        (Err(error), _) | (_, Err(error)) => {
            let repository_view = resolved_repository_view(task_id, existing.clone(), Err(error));
            if let Err(error) =
                project::record_task_repository_view(task_id, &repository_view).await
            {
                tracing::warn!(
                    task_id,
                    error = ?error,
                    "failed to persist unavailable task repository graph baseline"
                );
            }
            None
        }
    };
    let (context, sidecar_path, database_path) = prepared?;
    let task_id = task_id.to_string();
    let workspace_root = workspace_root.to_path_buf();
    let baseline_tree = baseline_tree.map(str::to_string);
    Some(tokio::spawn(async move {
        let resolved = resolve_task_baseline(
            context,
            sidecar_path,
            &task_id,
            &workspace_root,
            baseline_tree.as_deref(),
            existing.as_ref(),
        )
        .await;
        let repository_view = resolved_repository_view(&task_id, existing.clone(), resolved);
        match compare_and_record_task_baseline_at(
            &database_path,
            &task_id,
            existing.as_ref(),
            &repository_view,
        )
        .await
        {
            Ok(true) => maintain_graph_best_effort().await,
            Ok(false) => tracing::debug!(
                task_id,
                "task repository graph baseline was superseded by a newer task view"
            ),
            Err(error) => tracing::warn!(
                task_id,
                error = ?error,
                "failed to persist task repository graph baseline; dispatch already continued"
            ),
        }
    }))
}
