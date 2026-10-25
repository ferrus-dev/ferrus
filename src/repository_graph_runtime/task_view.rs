//! Task repository bindings, baseline resolution, and review freezes.

use super::*;

#[derive(Debug)]
pub(crate) enum RepositoryViewFreeze {
    NotAttempted,
    Frozen(project::RepositoryViewReference),
    Failed,
}

pub(super) fn frozen_tree_matches_snapshot(
    sidecar_path: &Path,
    workspace_root: &Path,
    repository: RepositoryRef,
    config: &RepositoryGraphConfig,
    source_tree: Digest,
    snapshot_id: &repository_graph::domain::SnapshotId,
) -> Result<bool> {
    let identities = active_extractor_identities(config)?;
    let discovery = SourceDiscoveryContext::from_config(repository, config, &identities)?;
    let source = TaskBaselineSource::discover(workspace_root, discovery, source_tree)?;
    let sidecar = match open_for_query_at(sidecar_path)? {
        OpenQuerySidecarResult::Ready(sidecar) => sidecar,
        OpenQuerySidecarResult::Absent => {
            anyhow::bail!("repository graph sidecar is unavailable")
        }
        OpenQuerySidecarResult::NeedsMigration {
            found_schema_version,
        } => anyhow::bail!(
            "repository graph sidecar schema {found_schema_version} requires migration"
        ),
        OpenQuerySidecarResult::RequiresRebuild(reason) => anyhow::bail!(
            "repository graph sidecar schema {} is incompatible with {}",
            reason.found_schema_version,
            reason.supported_schema_version
        ),
    };
    let snapshot = sidecar
        .snapshot(snapshot_id)?
        .context("submitted repository graph snapshot is no longer retained")?;
    let snapshot_files = all_snapshot_file_descriptors(&sidecar, snapshot_id)?;
    Ok(snapshot.repository == source.manifest().revision.repository
        && snapshot.analysis_config_digest == source.manifest().revision.analysis_config_digest
        && snapshot.extractor_set_digest == source.manifest().extractor_set_digest
        && snapshot_files == source.manifest().files)
}

pub(crate) fn capture_matching_submitted_tree(
    sidecar_path: &Path,
    workspace_root: &Path,
    task_id: &str,
    repository: RepositoryRef,
    config: &RepositoryGraphConfig,
    snapshot_id: &repository_graph::domain::SnapshotId,
) -> Result<Digest> {
    let source_tree = capture_worktree_tree(workspace_root)?;
    pin_submitted_tree(workspace_root, task_id, &source_tree)?;
    let matches = frozen_tree_matches_snapshot(
        sidecar_path,
        workspace_root,
        repository,
        config,
        source_tree.clone(),
        snapshot_id,
    );
    match matches {
        Ok(true) => Ok(source_tree),
        result => {
            release_submitted_tree_pin(workspace_root, task_id)?;
            match result {
                Ok(false) => {
                    anyhow::bail!("submitted source tree does not match the indexed task view")
                }
                Ok(true) => unreachable!("matching submitted tree returned above"),
                Err(error) => Err(error),
            }
        }
    }
}

/// Prepares the immutable graph/source identity persisted by the submit
/// transaction. Git writes only content-addressed objects through an isolated
/// temporary index; it never changes the task's real index or lifecycle row.
pub(crate) async fn prepare_submitted_repository_view(
    runtime: &project::RuntimeTaskContext,
) -> RepositoryViewFreeze {
    let Some(workspace_root) = runtime.workspace_path.as_deref() else {
        return RepositoryViewFreeze::NotAttempted;
    };
    let Some(baseline_tree) = std::env::var(ENV_BASELINE_TREE)
        .ok()
        .filter(|value| !value.trim().is_empty())
    else {
        return RepositoryViewFreeze::NotAttempted;
    };
    let graph_context = match LocalGraphContext::load(false).await {
        Ok(context) if context.config.enabled => context,
        Ok(_) => return RepositoryViewFreeze::NotAttempted,
        Err(error) => {
            tracing::warn!(
                task_id = runtime.task_id,
                error = ?error,
                "failed to prepare submitted repository view; submit will continue"
            );
            return RepositoryViewFreeze::Failed;
        }
    };
    let sidecar_path = match sidecar_path().await {
        Ok(path) => path,
        Err(error) => {
            tracing::warn!(
                task_id = runtime.task_id,
                error = ?error,
                "failed to resolve repository graph sidecar; submit will continue"
            );
            return RepositoryViewFreeze::Failed;
        }
    };

    let mutable_view = match refresh_task_overlay(
        &runtime.task_id,
        Path::new(workspace_root),
        baseline_tree.trim(),
    )
    .await
    {
        Ok(view) => view,
        Err(error) => {
            tracing::warn!(
                task_id = runtime.task_id,
                error = ?error,
                "failed to refresh submitted repository view; submit will continue"
            );
            return RepositoryViewFreeze::Failed;
        }
    };
    let Some(view_snapshot_id) = mutable_view.view_snapshot_id.clone() else {
        tracing::warn!(
            task_id = runtime.task_id,
            "submitted repository view was not materialized; submit will continue"
        );
        return RepositoryViewFreeze::Failed;
    };
    let workspace_root = std::path::PathBuf::from(workspace_root);
    let task_id = runtime.task_id.clone();
    let repository = graph_context.repository;
    let config = graph_context.config;
    let source_tree = match tokio::task::spawn_blocking(move || {
        capture_matching_submitted_tree(
            &sidecar_path,
            &workspace_root,
            &task_id,
            repository,
            &config,
            &view_snapshot_id,
        )
    })
    .await
    {
        Ok(Ok(tree)) => tree,
        Ok(Err(error)) => {
            tracing::warn!(
                task_id = runtime.task_id,
                error = ?error,
                "failed to capture submitted source tree; submit will continue"
            );
            return RepositoryViewFreeze::Failed;
        }
        Err(error) => {
            tracing::warn!(
                task_id = runtime.task_id,
                error = ?error,
                "submitted source capture task failed; submit will continue"
            );
            return RepositoryViewFreeze::Failed;
        }
    };
    match mutable_view.frozen(source_tree) {
        Ok(view) => RepositoryViewFreeze::Frozen(view),
        Err(error) => {
            release_submitted_tree_pin_best_effort(runtime).await;
            tracing::warn!(
                task_id = runtime.task_id,
                error = ?error,
                "submitted repository view was not materialized; submit will continue"
            );
            RepositoryViewFreeze::Failed
        }
    }
}

pub(crate) async fn release_submitted_tree_pin_best_effort(runtime: &project::RuntimeTaskContext) {
    release_submitted_tree_pin_for_task_best_effort(&runtime.task_id).await;
}

pub(crate) async fn release_submitted_tree_pin_for_task_best_effort(task_id: &str) {
    let project_root = match project::canonical_project_root().await {
        Ok(root) => root,
        Err(error) => {
            tracing::warn!(
                task_id,
                error = ?error,
                "failed to resolve project root while releasing submitted tree pin"
            );
            return;
        }
    };
    let owned_task_id = task_id.to_string();
    match tokio::task::spawn_blocking(move || {
        release_submitted_tree_pin(&project_root, &owned_task_id)
    })
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(
            task_id,
            error = ?error,
            "failed to release submitted tree pin"
        ),
        Err(error) => tracing::warn!(
            task_id,
            error = ?error,
            "submitted tree pin release task failed"
        ),
    }
}

pub(super) fn resolved_repository_view(
    task_id: &str,
    existing: Option<project::RepositoryViewReference>,
    resolved: Result<project::RepositoryViewReference>,
) -> project::RepositoryViewReference {
    match resolved {
        Ok(repository_view) => repository_view,
        Err(error) => {
            if error.downcast_ref::<RefreshAlreadyInProgress>().is_some() {
                return existing.unwrap_or_default();
            }

            tracing::warn!(
                task_id,
                error = ?error,
                "failed to pin task repository graph baseline; dispatch will continue"
            );
            match existing {
                Some(view) if view.baseline_snapshot_id.is_some() => {
                    let mut view = view.mutable_successor();
                    view.status = project::RepositoryViewStatus::Stale;
                    view
                }
                None => project::RepositoryViewReference {
                    status: project::RepositoryViewStatus::Failed,
                    ..Default::default()
                },
                Some(_) => project::RepositoryViewReference {
                    status: project::RepositoryViewStatus::Failed,
                    ..Default::default()
                },
            }
        }
    }
}

pub(super) async fn resolve_task_baseline(
    context: LocalGraphContext,
    sidecar_path: std::path::PathBuf,
    task_id: &str,
    workspace_root: &Path,
    baseline_tree: Option<&str>,
    existing: Option<&project::RepositoryViewReference>,
) -> Result<project::RepositoryViewReference> {
    if let Some(existing) = existing
        && let Some(snapshot_id) = existing.baseline_snapshot_id.as_ref()
    {
        let retained = match open_for_query_at(&sidecar_path) {
            Ok(OpenQuerySidecarResult::Ready(sidecar)) => sidecar
                .snapshot(snapshot_id)
                .context("Failed to inspect the pinned baseline snapshot")?
                .is_some(),
            _ => false,
        };
        if retained {
            let mut retained_view = existing.clone().mutable_successor();
            retained_view.status = existing.status;
            return Ok(retained_view);
        }
    }

    if !context.config.enabled || baseline_tree.is_none() {
        return project::RepositoryViewReference::new(
            None,
            None,
            project::RepositoryViewStatus::Unavailable,
        );
    }

    let baseline_tree =
        parse_git_tree_digest(baseline_tree.context("Missing task baseline tree")?)?;
    let workspace_root = workspace_root.to_path_buf();
    let config = context.config.clone();
    let repository = context.repository.clone();
    let task_id = task_id.to_string();
    let snapshot = tokio::task::spawn_blocking(move || -> Result<_> {
        let identities = active_extractor_identities(&config)?;
        let discovery =
            SourceDiscoveryContext::from_config(repository.clone(), &config, &identities)?;
        let source = TaskBaselineSource::discover(&workspace_root, discovery, baseline_tree)?;
        let mut sidecar = match open_for_build_at(&sidecar_path)? {
            OpenSidecarResult::Ready(sidecar) => sidecar,
            OpenSidecarResult::RequiresRebuild(reason) => anyhow::bail!(
                "repository graph sidecar schema {} is incompatible with {}",
                reason.found_schema_version,
                reason.supported_schema_version
            ),
        };
        let build_id = next_task_baseline_build_id(&task_id)?;
        let view_name = PublishedViewName::new(format!("task-baseline:{task_id}"))?;
        if sidecar.acquire_refresh_lease(
            &repository,
            &view_name,
            build_id.as_str(),
            REFRESH_LEASE_TTL,
        )? == RefreshLeaseOutcome::Busy
        {
            return Err(RefreshAlreadyInProgress.into());
        }

        let heartbeat = sidecar.start_refresh_lease_heartbeat(
            &repository,
            &view_name,
            build_id.as_str(),
            REFRESH_LEASE_TTL,
        )?;
        let indexed = IndexCoordinator::new(&mut sidecar).index(
            &source,
            &config,
            IndexRequest {
                build_id: build_id.clone(),
                view_name: view_name.clone(),
                force_full: false,
            },
        );
        let lease_healthy = heartbeat.finish();
        let released = sidecar.release_refresh_lease(&repository, &view_name, build_id.as_str());
        let outcome = indexed?;
        if !lease_healthy || !released? {
            anyhow::bail!("task baseline repository graph refresh lease was lost");
        }

        Ok(outcome.snapshot.id)
    })
    .await??;

    project::RepositoryViewReference::materialized(
        snapshot.clone(),
        None,
        snapshot,
        project::RepositoryViewStatus::Available,
    )
}
