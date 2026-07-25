//! Machine-local repository graph runtime adapter shared by CLI and MCP reads.

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU64,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{
    agent_id::{ENV_AGENT_ID, ENV_BASELINE_TREE, ENV_TASK_ID},
    project, repository_graph,
};
use anyhow::{Context, Result};
use repository_graph::{
    QUERY_WIRE_VERSION,
    config::RepositoryGraphConfig,
    domain::{
        Availability, BuildId, DiagnosticCode, DiagnosticLocation, DiagnosticSeverity, Freshness,
        PublishedViewName, QueryBudget, RepositoryId, RepositoryNamespace, RepositoryRef,
        TaskViewId, TaskViewLifecycle, WorkspaceRef,
    },
    index::{IndexCoordinator, IndexRequest, active_extractor_identities},
    maintenance::{GraphMaintenanceReport, RefreshLeaseOutcome, RetentionProtection},
    ports::{GraphQuery, SnapshotContent},
    query::{
        ContentRequest, ContextRequest, ContextResponse, ContextSnippet, DiagnosticSummary,
        DiagnosticsEnvelope, FreshnessEnvelope, PageInfo, QueryDiagnostic, QueryError,
        QueryErrorCode, QueryResponse, RetrievalAction, RetrievalFallback, SearchRequest,
        SearchResponse, SnapshotSelector, StatusData, StatusRequest, StatusResponse,
        TaskViewEnvelope, TaskViewStatus,
    },
    query_sqlite::{
        FreshnessComparison, SqliteGraphQuery, all_snapshot_file_descriptors, default_budget,
        snapshot_file_descriptors,
    },
    source::{
        GitTreeSnapshotContent, LocalRepositorySource, LocalSnapshotContent,
        SourceDiscoveryContext, TaskBaselineSource, TaskOverlaySource, capture_worktree_tree,
        parse_git_tree_digest,
    },
    sqlite::{
        OpenQuerySidecarResult, OpenSidecarResult, SIDECAR_FILE_NAME, open_for_build_at,
        open_for_query_at,
    },
};

pub(crate) const CANONICAL_VIEW: &str = "canonical";
static TASK_BASELINE_BUILD_COUNTER: AtomicU64 = AtomicU64::new(0);
static TASK_OVERLAY_BUILD_COUNTER: AtomicU64 = AtomicU64::new(0);
static CANONICAL_REFRESH_BUILD_COUNTER: AtomicU64 = AtomicU64::new(0);
const REFRESH_LEASE_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, thiserror::Error)]
#[error("repository graph refresh is already in progress for this view")]
struct RefreshAlreadyInProgress;

pub(crate) struct LocalGraphContext {
    pub(crate) project_root: std::path::PathBuf,
    pub(crate) root: std::path::PathBuf,
    pub(crate) repository: RepositoryRef,
    pub(crate) config: RepositoryGraphConfig,
    pub(crate) repository_view: Option<project::RepositoryViewReference>,
    pub(crate) task_view_id: Option<TaskViewId>,
    pub(crate) run_id: Option<String>,
}

impl LocalGraphContext {
    pub(crate) async fn load(require_enabled: bool) -> Result<Self> {
        let agent_id = std::env::var(ENV_AGENT_ID)
            .ok()
            .filter(|value| !value.trim().is_empty());
        Self::load_with_agent(require_enabled, agent_id.as_deref()).await
    }

    pub(crate) async fn load_for_agent(require_enabled: bool, agent_id: &str) -> Result<Self> {
        Self::load_with_agent(require_enabled, Some(agent_id)).await
    }

    async fn load_with_agent(require_enabled: bool, agent_id: Option<&str>) -> Result<Self> {
        let root = project::canonical_project_root().await?;
        let contents = tokio::fs::read_to_string(root.join("ferrus.toml"))
            .await
            .context("ferrus.toml not found — run ferrus init first")?;
        let config = RepositoryGraphConfig::from_ferrus_toml(&contents)
            .context("Invalid [repository_graph] configuration")?;
        if require_enabled && !config.enabled {
            anyhow::bail!(
                "repository graph is disabled; set repository_graph.enabled = true in ferrus.toml"
            );
        }
        let project_id = project::current_project_id().await?;
        let requested_task_id = std::env::var(ENV_TASK_ID)
            .ok()
            .filter(|value| !value.trim().is_empty());
        let runtime_context = match agent_id.map(str::trim).filter(|value| !value.is_empty()) {
            Some(agent_id) => project::runtime_task_context_for_agent(agent_id).await?,
            None => None,
        };
        if let Some(requested_task_id) = requested_task_id.as_deref() {
            let Some(runtime) = runtime_context.as_ref() else {
                anyhow::bail!(
                    "repository graph task binding {requested_task_id:?} is not attached to the current runtime"
                );
            };
            if runtime.task_id != requested_task_id {
                anyhow::bail!(
                    "repository graph task binding does not match the current runtime task"
                );
            }
        }
        if config.enabled
            && runtime_context.as_ref().is_some_and(|runtime| {
                runtime.run_role.as_deref() == Some("supervisor")
                    && runtime.status == project::TaskStatus::Reviewing.as_str()
                    && runtime.repository_view.lifecycle != TaskViewLifecycle::FrozenSubmitted
            })
        {
            anyhow::bail!(
                "the submitted repository view is unavailable for this reviewer; inspect the task source directly"
            );
        }
        let repository_view = runtime_context
            .as_ref()
            .map(|context| context.repository_view.clone());
        let task_view_id = runtime_context
            .as_ref()
            .map(|context| TaskViewId::new(&context.task_id))
            .transpose()?;
        let run_id = runtime_context
            .as_ref()
            .and_then(|context| context.run_id.clone());
        let source_root = runtime_context
            .as_ref()
            .and_then(|context| context.repository_workspace_path.as_ref())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| root.clone());
        Ok(Self {
            project_root: root,
            root: source_root,
            repository: RepositoryRef {
                namespace: RepositoryNamespace::new(format!("local:{project_id}"))?,
                repository_id: RepositoryId::new("root")?,
            },
            config,
            repository_view,
            task_view_id,
            run_id,
        })
    }

    pub(crate) fn discover(&self) -> Result<LocalRepositorySource> {
        self.discover_at(&self.root)
            .context("Failed to discover the repository source")
    }

    pub(crate) fn discover_canonical(&self) -> Result<LocalRepositorySource> {
        self.discover_at(&self.project_root)
            .context("Failed to discover the canonical repository source")
    }

    fn discover_at(&self, root: &Path) -> Result<LocalRepositorySource> {
        let identities = active_extractor_identities(&self.config)?;
        let context = SourceDiscoveryContext::from_config(
            self.repository.clone(),
            &self.config,
            &identities,
        )?;
        LocalRepositorySource::discover(root, context).map_err(Into::into)
    }

    pub(crate) fn freshness_comparison(&self) -> Result<Option<FreshnessComparison>> {
        if !self.config.enabled {
            return Ok(None);
        }
        let source = self.discover()?;
        Ok(Some(FreshnessComparison::from_manifest(source.manifest())))
    }

    pub(crate) fn scope(&self, budget: QueryBudget) -> Result<repository_graph::query::QueryScope> {
        let snapshot = match (&self.repository_view, &self.task_view_id) {
            (Some(view), _) if view.view_snapshot_id.is_some() => SnapshotSelector::Snapshot(
                view.view_snapshot_id
                    .clone()
                    .expect("checked materialized snapshot identity"),
            ),
            (Some(view), Some(task_view_id)) if view.overlay_revision_id.is_some() => {
                SnapshotSelector::Published(task_overlay_view_name(task_view_id)?)
            }
            (Some(view), _) if view.baseline_snapshot_id.is_some() => SnapshotSelector::Snapshot(
                view.baseline_snapshot_id
                    .clone()
                    .expect("checked baseline snapshot identity"),
            ),
            _ => SnapshotSelector::Published(
                PublishedViewName::new(CANONICAL_VIEW)
                    .expect("canonical published view name is non-empty"),
            ),
        };
        Ok(repository_graph::query::QueryScope::current(
            self.repository.clone(),
            snapshot,
            budget,
        ))
    }

    pub(crate) async fn status(&self) -> Result<StatusResponse> {
        if let Some(status) = self.unavailable_task_view_status() {
            let mut response = unavailable_status(
                self.repository.clone(),
                Availability::NotBuilt,
                status.as_str(),
                RetrievalAction::Index,
            )?;
            self.attach_task_view_to_status(&mut response);
            return Ok(response);
        }
        let path = sidecar_path().await?;
        // Discovering the current manifest can walk and hash the repository.
        // MCP retrieval must stay latency-bounded, so without a reliable source
        // mutation token it reports freshness as unknown rather than stale data
        // as fresh. The local CLI uses freshness_comparison() for exact checks.
        let mut response = status_response_at(self, &path, None)?;
        if let Some(reference) = self.canonical_invalidation().await {
            response.freshness = canonical_stale_freshness(reference);
            response.data.recommended_action = Some(RetrievalAction::RefreshIndex);
        }
        self.attach_task_view_to_status(&mut response);
        Ok(response)
    }

    pub(crate) async fn search(
        &self,
        request: &SearchRequest,
    ) -> Result<Result<SearchResponse, QueryError>> {
        if !self.config.enabled {
            return Ok(Err(query_error(
                QueryErrorCode::InvalidRequest,
                "repository graph is disabled; enable it before searching",
                false,
                None,
            )));
        }
        if let Some(status) = self.unavailable_task_view_status() {
            return Ok(Err(unavailable_task_view_error(status)));
        }
        let path = sidecar_path().await?;
        let mut response = search_response_at(self, &path, None, request);
        if let Ok(response) = response.as_mut() {
            // The durable stale marker is conservative and avoids a full
            // source scan on latency-bounded MCP reads.
            self.apply_canonical_invalidation(response).await;
            self.attach_task_view(response);
        }
        Ok(response)
    }

    pub(crate) async fn context(
        &self,
        request: &ContextRequest,
    ) -> Result<Result<ContextResponse, QueryError>> {
        if !self.config.enabled {
            return Ok(Err(query_error(
                QueryErrorCode::InvalidRequest,
                "repository graph is disabled; enable it before assembling context",
                false,
                None,
            )));
        }
        if let Some(status) = self.unavailable_task_view_status() {
            return Ok(Err(unavailable_task_view_error(status)));
        }
        let path = sidecar_path().await?;
        let mut response = context_response_at(self, &path, None, request);
        if let Ok(response) = response.as_mut() {
            self.apply_canonical_invalidation(response).await;
            self.attach_task_view(response);
        }
        Ok(response)
    }

    pub(crate) async fn context_with_snippets(
        &self,
        request: &ContextRequest,
        requested_snippet_bytes: NonZeroU64,
    ) -> Result<Result<ContextResponse, QueryError>> {
        let response = match self.context(request).await? {
            Ok(response) => response,
            Err(error) => return Ok(Err(error)),
        };
        let path = sidecar_path().await?;
        Ok(attach_snippets_at(
            self,
            &path,
            request,
            response,
            requested_snippet_bytes,
        ))
    }

    fn unavailable_task_view_status(&self) -> Option<project::RepositoryViewStatus> {
        self.repository_view
            .as_ref()
            .and_then(|view| view.baseline_snapshot_id.is_none().then_some(view.status))
    }

    pub(crate) fn task_view_envelope(&self) -> Option<TaskViewEnvelope> {
        let view = self.repository_view.as_ref()?;
        Some(TaskViewEnvelope {
            task_view_id: self.task_view_id.clone()?,
            baseline_snapshot_id: view.baseline_snapshot_id.clone()?,
            overlay_revision_id: view.overlay_revision_id.clone(),
            lifecycle: view.lifecycle,
        })
    }

    fn attach_task_view<T>(&self, response: &mut QueryResponse<T>) {
        response.task_view = self.task_view_envelope();
    }

    fn attach_task_view_to_status(&self, response: &mut StatusResponse) {
        response.task_view = self.task_view_envelope();
        if let Some(view) = self.repository_view.as_ref() {
            response.data.task_view_status = Some(task_view_status(view.status));
            response.data.fallback = (view.status != project::RepositoryViewStatus::Available)
                .then_some(RetrievalFallback::DirectSourceInspection);
        }
    }

    async fn apply_canonical_invalidation<T>(&self, response: &mut QueryResponse<T>) {
        let Some(reference) = self.canonical_invalidation().await else {
            return;
        };
        response.freshness = canonical_stale_freshness(reference);
    }

    async fn canonical_invalidation(&self) -> Option<project::CanonicalGraphReference> {
        if self.repository_view.is_some() {
            return None;
        }
        let reference = match project::canonical_graph_reference().await {
            Ok(reference) => reference,
            Err(error) => {
                tracing::warn!(error = ?error, "failed to read canonical graph invalidation state");
                return None;
            }
        };
        if reference.status != project::CanonicalGraphStatus::Stale {
            return None;
        }
        Some(reference)
    }
}

fn task_view_status(status: project::RepositoryViewStatus) -> TaskViewStatus {
    match status {
        project::RepositoryViewStatus::NotBuilt => TaskViewStatus::NotBuilt,
        project::RepositoryViewStatus::Available => TaskViewStatus::Available,
        project::RepositoryViewStatus::Stale => TaskViewStatus::Stale,
        project::RepositoryViewStatus::Unavailable => TaskViewStatus::Unavailable,
        project::RepositoryViewStatus::Failed => TaskViewStatus::Failed,
    }
}

fn canonical_stale_freshness(reference: project::CanonicalGraphReference) -> FreshnessEnvelope {
    FreshnessEnvelope {
        freshness: Freshness::Stale,
        compared_manifest: reference.source.map(|source| source.manifest_digest),
        reason_codes: vec!["canonical_invalidation".to_string()],
    }
}

fn task_overlay_view_name(task_view_id: &TaskViewId) -> Result<PublishedViewName> {
    Ok(PublishedViewName::new(format!(
        "task-overlay:{}",
        task_view_id.as_str()
    ))?)
}

pub(crate) async fn sidecar_path() -> Result<std::path::PathBuf> {
    Ok(project::current_project_data_dir()
        .await?
        .join(SIDECAR_FILE_NAME))
}

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

pub(crate) fn schedule_canonical_refresh_after_approval(
    project_root: std::path::PathBuf,
    task_id: String,
    run_id: Option<String>,
) {
    tokio::spawn(async move {
        match refresh_canonical_graph_at(&project_root).await {
            Ok(None) => {}
            Ok(Some((guard, source, snapshot_id, build_id))) => {
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
            }
            Err(error) if error.downcast_ref::<RefreshAlreadyInProgress>().is_some() => {
                tracing::debug!(
                    task_id,
                    "canonical repository graph refresh was deduplicated"
                );
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
                )
                .await;
            }
        }
    });
}

async fn refresh_canonical_graph_at(
    project_root: &Path,
) -> Result<
    Option<(
        project::CanonicalGraphRefreshGuard,
        project::CanonicalSourceIdentity,
        repository_graph::domain::SnapshotId,
        BuildId,
    )>,
> {
    // Observe the invalidation generation before source discovery. A later
    // approval may invalidate canonical content while this build is running;
    // its durable stale marker must win over this older publication.
    let refresh_guard = project::canonical_graph_refresh_guard().await?;
    let Some((config, repository, source)) = canonical_source_at(project_root).await? else {
        return Ok(None);
    };
    let source_identity = project::CanonicalSourceIdentity {
        source_revision_id: source.manifest().revision.id.clone(),
        manifest_digest: source.manifest().revision.manifest_digest.clone(),
    };
    let sidecar_path = sidecar_path().await?;
    let indexed_repository = repository.clone();
    let outcome = tokio::task::spawn_blocking(move || -> Result<_> {
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
        let indexed = IndexCoordinator::new(&mut sidecar).index(
            &source,
            &config,
            IndexRequest {
                build_id: build_id.clone(),
                view_name: view_name.clone(),
                force_full: false,
            },
        );
        let released =
            sidecar.release_refresh_lease(&indexed_repository, &view_name, build_id.as_str());
        let outcome = indexed?;
        if !released? {
            anyhow::bail!("canonical repository graph refresh lease was lost");
        }
        Ok(outcome)
    })
    .await??;
    debug_assert_eq!(outcome.snapshot.repository, repository);
    Ok(Some((
        refresh_guard,
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
) {
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
    let Some((context, sidecar_path, database_path)) = prepared else {
        return;
    };
    let task_id = task_id.to_string();
    let workspace_root = workspace_root.to_path_buf();
    let baseline_tree = baseline_tree.map(str::to_string);
    tokio::spawn(async move {
        let resolved = resolve_task_baseline(
            context,
            sidecar_path,
            &task_id,
            &workspace_root,
            baseline_tree.as_deref(),
            existing.as_ref(),
        )
        .await;
        let repository_view = resolved_repository_view(&task_id, existing, resolved);
        if let Err(error) =
            project::record_task_repository_view_at(&database_path, &task_id, &repository_view)
                .await
        {
            tracing::warn!(
                task_id,
                error = ?error,
                "failed to persist task repository graph baseline; dispatch already continued"
            );
        } else {
            maintain_graph_best_effort().await;
        }
    });
}

/// Explicitly refreshes the mutable task overlay without coupling graph
/// failures to the orchestration state machine. `/check` and the final submit
/// gate call this after source-changing work; retrieval tools remain read-only.
pub(crate) async fn refresh_task_overlay_best_effort_for_context(
    runtime: &project::RuntimeTaskContext,
) {
    let Some(workspace_root) = runtime.workspace_path.as_deref() else {
        return;
    };
    let Some(baseline_tree) = std::env::var(ENV_BASELINE_TREE)
        .ok()
        .filter(|value| !value.trim().is_empty())
    else {
        return;
    };
    if let Err(error) = refresh_task_overlay(
        &runtime.task_id,
        Path::new(workspace_root),
        baseline_tree.trim(),
    )
    .await
    {
        tracing::warn!(
            task_id = runtime.task_id,
            error = ?error,
            "failed to refresh task repository graph overlay; task lifecycle is unchanged"
        );
    }
}

#[derive(Debug)]
pub(crate) enum RepositoryViewFreeze {
    NotAttempted,
    Frozen(project::RepositoryViewReference),
    Failed,
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
    drop(graph_context);

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
    let workspace_root = std::path::PathBuf::from(workspace_root);
    let source_tree =
        match tokio::task::spawn_blocking(move || capture_worktree_tree(&workspace_root)).await {
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
            tracing::warn!(
                task_id = runtime.task_id,
                error = ?error,
                "submitted repository view was not materialized; submit will continue"
            );
            RepositoryViewFreeze::Failed
        }
    }
}

pub(crate) async fn refresh_task_overlay(
    task_id: &str,
    workspace_root: &Path,
    baseline_tree: &str,
) -> Result<project::RepositoryViewReference> {
    let existing = project::task_repository_view(task_id)
        .await?
        .context("task repository view has not been initialized")?;
    let baseline_snapshot_id = existing
        .baseline_snapshot_id
        .clone()
        .context("task repository graph baseline is unavailable")?;
    let context = LocalGraphContext::load(false).await?;
    if !context.config.enabled {
        return Ok(existing);
    }
    let sidecar_path = sidecar_path().await?;
    let database_path = project::current_project_data_dir().await?.join("ferrus.db");
    let repository = context.repository.clone();
    let config = context.config.clone();
    let workspace_root = workspace_root.to_path_buf();
    let baseline_revision = parse_git_tree_digest(baseline_tree)?;
    let task_view_id = TaskViewId::new(task_id)?;
    let task_id = task_id.to_string();
    let indexed_baseline_snapshot_id = baseline_snapshot_id.clone();

    let refreshed = tokio::task::spawn_blocking(move || -> Result<_> {
        let identities = active_extractor_identities(&config)?;
        let discovery =
            SourceDiscoveryContext::from_config(repository.clone(), &config, &identities)?;
        let mut sidecar = match open_for_build_at(&sidecar_path)? {
            OpenSidecarResult::Ready(sidecar) => sidecar,
            OpenSidecarResult::RequiresRebuild(reason) => anyhow::bail!(
                "repository graph sidecar schema {} is incompatible with {}",
                reason.found_schema_version,
                reason.supported_schema_version
            ),
        };
        if sidecar.snapshot(&indexed_baseline_snapshot_id)?.is_none() {
            anyhow::bail!("task repository graph baseline snapshot is no longer retained");
        }
        let baseline_files =
            all_snapshot_file_descriptors(&sidecar, &indexed_baseline_snapshot_id)?;
        let view_name = task_overlay_view_name(&task_view_id)?;
        let build_id = next_task_overlay_build_id(&task_view_id)?;
        if sidecar.acquire_refresh_lease(
            &repository,
            &view_name,
            build_id.as_str(),
            REFRESH_LEASE_TTL,
        )? == RefreshLeaseOutcome::Busy
        {
            return Err(RefreshAlreadyInProgress.into());
        }
        let refreshed = (|| -> Result<_> {
            let source = TaskOverlaySource::discover(
                &workspace_root,
                WorkspaceRef {
                    repository: repository.clone(),
                    task_view_id: task_view_id.clone(),
                    baseline_revision,
                },
                discovery,
                baseline_files,
            )?;
            if source.overlay_manifest().changes.is_empty() {
                return Ok(None);
            }
            let overlay_revision_id = source.overlay_manifest().revision_id.clone();
            let outcome = IndexCoordinator::new(&mut sidecar).index(
                &source,
                &config,
                IndexRequest {
                    build_id: build_id.clone(),
                    view_name: view_name.clone(),
                    force_full: false,
                },
            )?;
            Ok(Some((overlay_revision_id, outcome.snapshot.id)))
        })();
        let released = sidecar.release_refresh_lease(&repository, &view_name, build_id.as_str());
        let refreshed = refreshed?;
        if !released? {
            anyhow::bail!("task repository graph refresh lease was lost");
        }
        Ok(refreshed)
    })
    .await;

    let repository_view = match refreshed {
        Ok(Ok(Some((overlay_revision_id, view_snapshot_id)))) => {
            project::RepositoryViewReference::materialized(
                baseline_snapshot_id,
                Some(overlay_revision_id),
                view_snapshot_id,
                project::RepositoryViewStatus::Available,
            )?
        }
        Ok(Ok(None)) => project::RepositoryViewReference::materialized(
            baseline_snapshot_id.clone(),
            None,
            baseline_snapshot_id,
            project::RepositoryViewStatus::Available,
        )?,
        Ok(Err(error)) if error.downcast_ref::<RefreshAlreadyInProgress>().is_some() => {
            return Err(error);
        }
        Ok(Err(error)) => {
            let mut stale = existing.clone().mutable_successor();
            stale.status = project::RepositoryViewStatus::Stale;
            if !project::compare_and_record_task_repository_view_at(
                &database_path,
                &task_id,
                &existing,
                &stale,
            )
            .await?
            {
                return current_task_repository_view(&task_id).await;
            }
            return Err(error);
        }
        Err(error) => {
            let mut stale = existing.clone().mutable_successor();
            stale.status = project::RepositoryViewStatus::Stale;
            if !project::compare_and_record_task_repository_view_at(
                &database_path,
                &task_id,
                &existing,
                &stale,
            )
            .await?
            {
                return current_task_repository_view(&task_id).await;
            }
            return Err(error.into());
        }
    };
    if !project::compare_and_record_task_repository_view_at(
        &database_path,
        &task_id,
        &existing,
        &repository_view,
    )
    .await?
    {
        return current_task_repository_view(&task_id).await;
    }
    maintain_graph_best_effort().await;
    Ok(repository_view)
}

async fn current_task_repository_view(task_id: &str) -> Result<project::RepositoryViewReference> {
    project::task_repository_view(task_id)
        .await?
        .context("task repository view disappeared during refresh")
}

fn resolved_repository_view(
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
                None => project::RepositoryViewReference::new(
                    None,
                    None,
                    project::RepositoryViewStatus::Failed,
                )
                .expect("a failed unavailable view has no overlay"),
                Some(_) => project::RepositoryViewReference::new(
                    None,
                    None,
                    project::RepositoryViewStatus::Failed,
                )
                .expect("a failed unavailable view has no overlay"),
            }
        }
    }
}

async fn resolve_task_baseline(
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
        let mut retained_view = existing.clone().mutable_successor();
        retained_view.status = if retained {
            existing.status
        } else {
            project::RepositoryViewStatus::Stale
        };
        return Ok(retained_view);
    }
    if !context.config.enabled || baseline_tree.is_none() {
        return project::RepositoryViewReference::new(
            None,
            None,
            project::RepositoryViewStatus::Unavailable,
        );
    }

    let baseline_tree = parse_git_tree_digest(baseline_tree.expect("checked above"))?;
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
        let indexed = IndexCoordinator::new(&mut sidecar).index(
            &source,
            &config,
            IndexRequest {
                build_id: build_id.clone(),
                view_name: view_name.clone(),
                force_full: false,
            },
        );
        let released = sidecar.release_refresh_lease(&repository, &view_name, build_id.as_str());
        let outcome = indexed?;
        if !released? {
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

fn next_task_baseline_build_id(task_id: &str) -> Result<BuildId> {
    let sequence = TASK_BASELINE_BUILD_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Ok(BuildId::new(format!(
        "task-baseline:{task_id}:{nanos:x}:{sequence:x}"
    ))?)
}

fn next_task_overlay_build_id(task_view_id: &TaskViewId) -> Result<BuildId> {
    let sequence = TASK_OVERLAY_BUILD_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Ok(BuildId::new(format!(
        "task-overlay:{}:{nanos:x}:{sequence:x}",
        task_view_id.as_str()
    ))?)
}

pub(crate) fn status_response_at(
    context: &LocalGraphContext,
    sidecar_path: &Path,
    freshness_comparison: Option<FreshnessComparison>,
) -> Result<StatusResponse> {
    match open_for_query_at(sidecar_path) {
        Ok(OpenQuerySidecarResult::Ready(sidecar)) => {
            let query = SqliteGraphQuery::new(
                &sidecar,
                context.config.query_limits.clone(),
                freshness_comparison,
            );
            Ok(query.status(&StatusRequest {
                scope: context.scope(default_budget(&context.config.query_limits)?)?,
            })?)
        }
        Ok(OpenQuerySidecarResult::Absent) => unavailable_status(
            context.repository.clone(),
            Availability::NotBuilt,
            "not_built",
            RetrievalAction::Index,
        ),
        Ok(OpenQuerySidecarResult::NeedsMigration {
            found_schema_version,
        }) => unavailable_status(
            context.repository.clone(),
            Availability::Incompatible,
            &format!("schema_{found_schema_version}_needs_migration"),
            RetrievalAction::Index,
        ),
        Ok(OpenQuerySidecarResult::RequiresRebuild(_)) => unavailable_status(
            context.repository.clone(),
            Availability::Incompatible,
            "incompatible_schema",
            RetrievalAction::Rebuild,
        ),
        Err(_) => unavailable_status(
            context.repository.clone(),
            Availability::Incompatible,
            "sidecar_unreadable",
            RetrievalAction::Rebuild,
        ),
    }
}

fn search_response_at(
    context: &LocalGraphContext,
    sidecar_path: &Path,
    freshness_comparison: Option<FreshnessComparison>,
    request: &SearchRequest,
) -> Result<SearchResponse, QueryError> {
    match open_for_query_at(sidecar_path) {
        Ok(OpenQuerySidecarResult::Ready(sidecar)) => SqliteGraphQuery::new(
            &sidecar,
            context.config.query_limits.clone(),
            freshness_comparison,
        )
        .search(request),
        Ok(OpenQuerySidecarResult::Absent) => Err(query_error(
            QueryErrorCode::NotBuilt,
            "repository graph is not built; run `ferrus graph index`",
            false,
            Some(RetrievalAction::Index),
        )),
        Ok(OpenQuerySidecarResult::NeedsMigration { .. }) => Err(query_error(
            QueryErrorCode::Incompatible,
            "repository graph storage needs migration; run `ferrus graph index`",
            false,
            Some(RetrievalAction::Index),
        )),
        Ok(OpenQuerySidecarResult::RequiresRebuild(_)) => Err(query_error(
            QueryErrorCode::Incompatible,
            "repository graph storage is incompatible; rebuild the derived index",
            false,
            Some(RetrievalAction::Rebuild),
        )),
        Err(_) => Err(query_error(
            QueryErrorCode::BackendUnavailable,
            "repository graph storage is unavailable or inconsistent",
            true,
            Some(RetrievalAction::Rebuild),
        )),
    }
}

fn context_response_at(
    context: &LocalGraphContext,
    sidecar_path: &Path,
    freshness_comparison: Option<FreshnessComparison>,
    request: &ContextRequest,
) -> Result<ContextResponse, QueryError> {
    match open_for_query_at(sidecar_path) {
        Ok(OpenQuerySidecarResult::Ready(sidecar)) => SqliteGraphQuery::new(
            &sidecar,
            context.config.query_limits.clone(),
            freshness_comparison,
        )
        .context(request),
        Ok(OpenQuerySidecarResult::Absent) => Err(query_error(
            QueryErrorCode::NotBuilt,
            "repository graph is not built; run `ferrus graph index`",
            false,
            Some(RetrievalAction::Index),
        )),
        Ok(OpenQuerySidecarResult::NeedsMigration { .. }) => Err(query_error(
            QueryErrorCode::Incompatible,
            "repository graph storage needs migration; run `ferrus graph index`",
            false,
            Some(RetrievalAction::Index),
        )),
        Ok(OpenQuerySidecarResult::RequiresRebuild(_)) => Err(query_error(
            QueryErrorCode::Incompatible,
            "repository graph storage is incompatible; rebuild the derived index",
            false,
            Some(RetrievalAction::Rebuild),
        )),
        Err(_) => Err(query_error(
            QueryErrorCode::BackendUnavailable,
            "repository graph storage is unavailable or inconsistent",
            true,
            Some(RetrievalAction::Rebuild),
        )),
    }
}

fn attach_snippets_at(
    context: &LocalGraphContext,
    sidecar_path: &Path,
    request: &ContextRequest,
    mut response: ContextResponse,
    requested_snippet_bytes: NonZeroU64,
) -> Result<ContextResponse, QueryError> {
    let hard_limit =
        NonZeroU64::new(context.config.query_limits.max_snippet_bytes).ok_or_else(|| {
            query_error(
                QueryErrorCode::InvalidRequest,
                "repository_graph.query_limits.max_snippet_bytes must be greater than zero",
                false,
                None,
            )
        })?;
    let total_limit = requested_snippet_bytes.get().min(hard_limit.get());
    let max_diagnostics = request
        .scope
        .budget
        .max_diagnostics
        .get()
        .min(context.config.query_limits.max_diagnostics) as usize;
    let sidecar = match open_for_query_at(sidecar_path) {
        Ok(OpenQuerySidecarResult::Ready(sidecar)) => sidecar,
        _ => {
            return Err(query_error(
                QueryErrorCode::ContentUnavailable,
                "repository content metadata became unavailable after context assembly",
                true,
                None,
            ));
        }
    };

    let paths = response
        .data
        .items
        .iter()
        .map(|item| item.path.clone())
        .collect::<BTreeSet<_>>();
    let files = snapshot_file_descriptors(&sidecar, &response.snapshot_id, &paths)?;

    let content: Box<dyn SnapshotContent> = match context.repository_view.as_ref() {
        Some(view) if view.lifecycle == TaskViewLifecycle::FrozenSubmitted => {
            let tree = view.frozen_source_tree.clone().ok_or_else(|| {
                query_error(
                    QueryErrorCode::ContentUnavailable,
                    "frozen repository view is missing its source tree identity",
                    false,
                    None,
                )
            })?;
            Box::new(
                GitTreeSnapshotContent::new(
                    &context.project_root,
                    context.repository.clone(),
                    response.snapshot_id.clone(),
                    tree,
                    &context.config.source,
                    files,
                    hard_limit,
                )
                .map_err(|_| {
                    query_error(
                        QueryErrorCode::ContentUnavailable,
                        "frozen repository content boundary could not be initialized",
                        true,
                        None,
                    )
                })?,
            )
        }
        _ => Box::new(
            LocalSnapshotContent::new(
                &context.root,
                context.repository.clone(),
                response.snapshot_id.clone(),
                &context.config.source,
                files,
                hard_limit,
            )
            .map_err(|_| {
                query_error(
                    QueryErrorCode::ContentUnavailable,
                    "repository content boundary could not be initialized",
                    true,
                    None,
                )
            })?,
        ),
    };

    let evidence = response
        .data
        .items
        .iter()
        .map(|item| {
            (
                item.path.clone(),
                item.span.clone(),
                item.content_identity.clone(),
            )
        })
        .collect::<Vec<_>>();
    let mut seen = BTreeSet::new();
    let mut remaining = total_limit;
    let mut omitted_for_budget = false;
    for (path, span, content_identity) in evidence {
        let key = serde_json::to_string(&(path.clone(), span.clone(), content_identity.clone()))
            .expect("context evidence is always serializable");
        if !seen.insert(key) {
            continue;
        }
        let Some(max_bytes) = NonZeroU64::new(remaining) else {
            omitted_for_budget = true;
            break;
        };
        match content.read_verified(&ContentRequest {
            wire_version: QUERY_WIRE_VERSION,
            repository: response.repository.clone(),
            snapshot_id: response.snapshot_id.clone(),
            path: path.clone(),
            expected_content_identity: content_identity,
            span: span.clone(),
            max_bytes,
        }) {
            Ok(snippet) => match String::from_utf8(snippet.bytes) {
                Ok(text) => {
                    remaining = remaining.saturating_sub(text.len() as u64);
                    response.data.snippets.push(ContextSnippet {
                        path,
                        span,
                        verified_content_identity: snippet.verified_content_identity,
                        text,
                        truncated: snippet.truncated,
                    });
                    if snippet.truncated {
                        omitted_for_budget = true;
                    }
                }
                Err(_) => add_content_diagnostic(
                    &mut response,
                    max_diagnostics,
                    "content.non_utf8",
                    path,
                    span,
                ),
            },
            Err(error) => add_content_diagnostic(
                &mut response,
                max_diagnostics,
                match error.code {
                    QueryErrorCode::ContentChanged => "content.changed",
                    _ => "content.unavailable",
                },
                path,
                span,
            ),
        }
    }
    if omitted_for_budget {
        add_content_diagnostic_without_location(
            &mut response,
            max_diagnostics,
            "content.snippets_truncated",
        );
    }
    Ok(response)
}

fn add_content_diagnostic(
    response: &mut ContextResponse,
    max_diagnostics: usize,
    code: &str,
    path: repository_graph::domain::RepoPath,
    span: Option<repository_graph::domain::SourceSpan>,
) {
    add_bounded_content_diagnostic(
        response,
        max_diagnostics,
        code,
        Some(DiagnosticLocation { path, span }),
    );
}

fn add_content_diagnostic_without_location(
    response: &mut ContextResponse,
    max_diagnostics: usize,
    code: &str,
) {
    add_bounded_content_diagnostic(response, max_diagnostics, code, None);
}

fn add_bounded_content_diagnostic(
    response: &mut ContextResponse,
    max_diagnostics: usize,
    code: &str,
    location: Option<DiagnosticLocation>,
) {
    response.diagnostics.summary.warning += 1;
    if response.diagnostics.items.len() < max_diagnostics {
        response.diagnostics.items.push(QueryDiagnostic {
            severity: DiagnosticSeverity::Warning,
            code: DiagnosticCode::new(code).expect("static content diagnostic code is canonical"),
            location,
        });
    } else {
        response.diagnostics.truncated = true;
    }
}

fn unavailable_status(
    repository: RepositoryRef,
    availability: Availability,
    reason: &str,
    action: RetrievalAction,
) -> Result<StatusResponse> {
    Ok(StatusResponse {
        wire_version: QUERY_WIRE_VERSION,
        repository,
        snapshot_id: None,
        source_revision: None,
        task_view: None,
        freshness: FreshnessEnvelope {
            freshness: Freshness::NotApplicable,
            compared_manifest: None,
            reason_codes: vec![reason.to_string()],
        },
        diagnostics: DiagnosticsEnvelope {
            summary: DiagnosticSummary::default(),
            items: vec![],
            truncated: false,
        },
        page: PageInfo {
            next_cursor: None,
            truncation: None,
        },
        data: StatusData {
            availability,
            build_state: None,
            build_id: None,
            published_view: Some(PublishedViewName::new(CANONICAL_VIEW)?),
            graph_model_version: None,
            statistics: None,
            recommended_action: Some(action),
            task_view_status: None,
            fallback: None,
        },
    })
}

fn query_error(
    code: QueryErrorCode,
    message: &str,
    retryable: bool,
    recommended_action: Option<RetrievalAction>,
) -> QueryError {
    QueryError {
        wire_version: QUERY_WIRE_VERSION,
        code,
        message: message.to_string(),
        retryable,
        recommended_action,
        details: BTreeMap::new(),
    }
}

fn unavailable_task_view_error(status: project::RepositoryViewStatus) -> QueryError {
    let mut error = query_error(
        QueryErrorCode::NotBuilt,
        "repository graph is unavailable for the current task baseline; inspect source directly",
        matches!(status, project::RepositoryViewStatus::Stale),
        Some(RetrievalAction::Index),
    );
    error
        .details
        .insert("task_view_status".to_string(), status.as_str().to_string());
    error.details.insert(
        "fallback".to_string(),
        "direct_source_inspection".to_string(),
    );
    error
}

#[cfg(test)]
mod tests {
    use super::*;

    fn indexed_context(root: &Path, sidecar_path: &Path) -> (LocalGraphContext, ContextRequest) {
        let mut context = context(root);
        context.config.enabled = true;
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), b"pub struct RuntimeTaskContext;\n").unwrap();
        let source = context.discover().unwrap();
        let mut sidecar = match repository_graph::sqlite::open_for_build_at(sidecar_path).unwrap() {
            repository_graph::sqlite::OpenSidecarResult::Ready(sidecar) => sidecar,
            repository_graph::sqlite::OpenSidecarResult::RequiresRebuild(_) => {
                panic!("new sidecar unexpectedly requires rebuild")
            }
        };
        repository_graph::index::IndexCoordinator::new(&mut sidecar)
            .index(
                &source,
                &context.config,
                repository_graph::index::IndexRequest {
                    build_id: repository_graph::domain::BuildId::new("build-1").unwrap(),
                    view_name: PublishedViewName::new(CANONICAL_VIEW).unwrap(),
                    force_full: false,
                },
            )
            .unwrap();
        drop(sidecar);
        let request = ContextRequest {
            scope: context
                .scope(default_budget(&context.config.query_limits).unwrap())
                .unwrap(),
            seeds: vec![repository_graph::query::ContextSeed::Path(
                repository_graph::domain::RepoPath::new("src/lib.rs").unwrap(),
            )],
            policy: repository_graph::query::ContextPolicy {
                direction: repository_graph::query::EdgeDirection::Both,
                edge_kinds: vec![],
                include_unresolved: false,
                include_external: false,
            },
            page: repository_graph::query::PageRequest { cursor: None },
        };
        (context, request)
    }

    fn context(root: &Path) -> LocalGraphContext {
        LocalGraphContext {
            project_root: root.to_path_buf(),
            root: root.to_path_buf(),
            repository: RepositoryRef {
                namespace: RepositoryNamespace::new("local:test").unwrap(),
                repository_id: RepositoryId::new("root").unwrap(),
            },
            config: RepositoryGraphConfig::default(),
            repository_view: None,
            task_view_id: None,
            run_id: None,
        }
    }

    #[test]
    fn canonical_discovery_ignores_a_task_worktree_root() {
        let canonical = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(canonical.path().join("src")).unwrap();
        std::fs::create_dir_all(worktree.path().join("src")).unwrap();
        std::fs::write(
            canonical.path().join("src/lib.rs"),
            b"pub struct CanonicalSymbol;\n",
        )
        .unwrap();
        std::fs::write(
            worktree.path().join("src/lib.rs"),
            b"pub struct UnapprovedTaskSymbol;\n",
        )
        .unwrap();
        let mut graph_context = context(canonical.path());
        graph_context.root = worktree.path().to_path_buf();

        let task_source = graph_context.discover().unwrap();
        let canonical_source = graph_context.discover_canonical().unwrap();

        assert_ne!(
            task_source.manifest().revision.manifest_digest,
            canonical_source.manifest().revision.manifest_digest
        );
        assert_eq!(
            canonical_source.manifest().revision.manifest_digest,
            context(canonical.path())
                .discover()
                .unwrap()
                .manifest()
                .revision
                .manifest_digest
        );
    }

    #[test]
    fn absent_status_search_and_context_are_read_only_and_actionable() {
        let directory = tempfile::tempdir().unwrap();
        let sidecar = directory.path().join(SIDECAR_FILE_NAME);
        let context = context(directory.path());

        let status = status_response_at(&context, &sidecar, None).unwrap();
        let search = search_response_at(
            &context,
            &sidecar,
            None,
            &SearchRequest {
                scope: context
                    .scope(default_budget(&context.config.query_limits).unwrap())
                    .unwrap(),
                text: "RuntimeTaskContext".to_string(),
                node_kinds: vec![],
                paths: vec![],
                page: repository_graph::query::PageRequest { cursor: None },
            },
        )
        .unwrap_err();
        let context_response = context_response_at(
            &context,
            &sidecar,
            None,
            &ContextRequest {
                scope: context
                    .scope(default_budget(&context.config.query_limits).unwrap())
                    .unwrap(),
                seeds: vec![repository_graph::query::ContextSeed::Path(
                    repository_graph::domain::RepoPath::new("src/lib.rs").unwrap(),
                )],
                policy: repository_graph::query::ContextPolicy {
                    direction: repository_graph::query::EdgeDirection::Both,
                    edge_kinds: vec![],
                    include_unresolved: false,
                    include_external: false,
                },
                page: repository_graph::query::PageRequest { cursor: None },
            },
        )
        .unwrap_err();

        assert_eq!(status.data.availability, Availability::NotBuilt);
        assert_eq!(status.data.recommended_action, Some(RetrievalAction::Index));
        assert_eq!(search.code, QueryErrorCode::NotBuilt);
        assert_eq!(search.recommended_action, Some(RetrievalAction::Index));
        assert_eq!(context_response.code, QueryErrorCode::NotBuilt);
        assert_eq!(
            context_response.recommended_action,
            Some(RetrievalAction::Index)
        );
        assert!(!sidecar.exists());
    }

    #[test]
    fn unavailable_task_status_exposes_binding_and_direct_source_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let mut context = context(directory.path());
        context.repository_view = Some(
            project::RepositoryViewReference::new(
                None,
                None,
                project::RepositoryViewStatus::Unavailable,
            )
            .unwrap(),
        );
        context.task_view_id = Some(TaskViewId::new("t-001").unwrap());
        let mut response = unavailable_status(
            context.repository.clone(),
            Availability::NotBuilt,
            "unavailable",
            RetrievalAction::Index,
        )
        .unwrap();

        context.attach_task_view_to_status(&mut response);

        assert_eq!(response.task_view, None);
        assert_eq!(
            response.data.task_view_status,
            Some(TaskViewStatus::Unavailable)
        );
        assert_eq!(
            response.data.fallback,
            Some(RetrievalFallback::DirectSourceInspection)
        );
    }

    #[test]
    fn unreadable_sidecar_is_distinct_from_a_missing_index() {
        let directory = tempfile::tempdir().unwrap();
        let sidecar = directory.path().join(SIDECAR_FILE_NAME);
        std::fs::write(&sidecar, b"not a sqlite database").unwrap();
        let context = context(directory.path());

        let status = status_response_at(&context, &sidecar, None).unwrap();

        assert_eq!(status.data.availability, Availability::Incompatible);
        assert_eq!(status.freshness.reason_codes, ["sidecar_unreadable"]);
        assert_eq!(
            status.data.recommended_action,
            Some(RetrievalAction::Rebuild)
        );
    }

    #[test]
    fn mcp_runtime_does_not_label_changed_source_as_fresh_without_revalidation() {
        let directory = tempfile::tempdir().unwrap();
        let sidecar = directory.path().join(SIDECAR_FILE_NAME);
        let (context, request) = indexed_context(directory.path(), &sidecar);
        std::fs::write(
            directory.path().join("src/lib.rs"),
            b"pub struct ChangedAfterIndex;\n",
        )
        .unwrap();

        let response = context_response_at(&context, &sidecar, None, &request).unwrap();

        assert_eq!(response.freshness.freshness, Freshness::Unknown);
        assert_eq!(response.freshness.reason_codes, ["source_not_compared"]);
    }

    #[test]
    fn task_scope_remains_pinned_when_canonical_publication_advances() {
        let directory = tempfile::tempdir().unwrap();
        let sidecar_path = directory.path().join(SIDECAR_FILE_NAME);
        let source_root = directory.path().join("repository");
        std::fs::create_dir(&source_root).unwrap();
        let (mut context, _) = indexed_context(&source_root, &sidecar_path);
        let sidecar = match open_for_query_at(&sidecar_path).unwrap() {
            OpenQuerySidecarResult::Ready(sidecar) => sidecar,
            _ => panic!("indexed sidecar must be queryable"),
        };
        let baseline = sidecar
            .published_view(
                &context.repository,
                &PublishedViewName::new(CANONICAL_VIEW).unwrap(),
            )
            .unwrap()
            .unwrap()
            .snapshot_id;
        drop(sidecar);
        context.repository_view = Some(
            project::RepositoryViewReference::new(
                Some(baseline.clone()),
                None,
                project::RepositoryViewStatus::Available,
            )
            .unwrap(),
        );

        std::fs::write(
            source_root.join("src/lib.rs"),
            b"pub struct CanonicalAdvanced;\n",
        )
        .unwrap();
        let source = context.discover().unwrap();
        let mut sidecar = match open_for_build_at(&sidecar_path).unwrap() {
            OpenSidecarResult::Ready(sidecar) => sidecar,
            OpenSidecarResult::RequiresRebuild(_) => panic!("sidecar unexpectedly incompatible"),
        };
        let advanced = IndexCoordinator::new(&mut sidecar)
            .index(
                &source,
                &context.config,
                IndexRequest {
                    build_id: BuildId::new("build-advanced").unwrap(),
                    view_name: PublishedViewName::new(CANONICAL_VIEW).unwrap(),
                    force_full: false,
                },
            )
            .unwrap();
        assert_ne!(advanced.snapshot.id, baseline);
        drop(sidecar);

        let request = SearchRequest {
            scope: context
                .scope(default_budget(&context.config.query_limits).unwrap())
                .unwrap(),
            text: "RuntimeTaskContext".to_string(),
            node_kinds: vec![],
            paths: vec![],
            page: repository_graph::query::PageRequest { cursor: None },
        };
        let response = search_response_at(&context, &sidecar_path, None, &request).unwrap();

        assert_eq!(response.snapshot_id, baseline);
        assert!(response.data.hits.iter().any(|hit| {
            hit.semantic_key
                .as_ref()
                .is_some_and(|key| key.as_str().contains("RuntimeTaskContext"))
        }));
    }

    #[tokio::test]
    async fn dispatch_pins_git_baseline_without_changing_task_lifecycle() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let previous = std::env::current_dir().unwrap();
        std::fs::create_dir_all(root.join(".ferrus/projects/test-project")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("ferrus.toml"),
            "[repository_graph]\nenabled = true\n",
        )
        .unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub struct BaselineSymbol;\n").unwrap();
        std::fs::write(root.join("src/deleted.rs"), "pub struct DeletedSymbol;\n").unwrap();
        let data_dir = root.join(".ferrus/projects/test-project");
        std::fs::write(
            root.join(".ferrus/project.toml"),
            toml::to_string(&project::LocalProjectRef {
                project_id: "test-project".to_string(),
                name: "test".to_string(),
                data_dir: data_dir.to_string_lossy().into_owned(),
            })
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            data_dir.join("project.toml"),
            toml::to_string(&project::ProjectMetadata {
                id: "test-project".to_string(),
                name: "test".to_string(),
                workspace_dir: root.to_string_lossy().into_owned(),
                ferrus_dir: root.join(".ferrus").to_string_lossy().into_owned(),
                vcs: Some("git".to_string()),
                origin_repo: None,
                default_branch: Some("main".to_string()),
                current_head: None,
                created_at: "2026-07-22T00:00:00Z".to_string(),
                last_opened_at: "2026-07-22T00:00:00Z".to_string(),
                version: 1,
            })
            .unwrap(),
        )
        .unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git command failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };
        git(&["init"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Ferrus Test"]);
        git(&["config", "commit.gpgsign", "false"]);
        git(&["add", "ferrus.toml", "src/lib.rs", "src/deleted.rs"]);
        git(&["commit", "-m", "baseline"]);
        let baseline_tree = git(&["rev-parse", "HEAD^{tree}"]);

        std::env::set_current_dir(root).unwrap();
        project::record_task_status(
            "t-001",
            ".ferrus/tasks/t-001.md",
            project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        schedule_task_baseline_pin("t-001", root, Some(&baseline_tree)).await;
        for _ in 0..100 {
            if project::task_repository_view("t-001")
                .await
                .unwrap()
                .is_some_and(|view| view.status == project::RepositoryViewStatus::Available)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let repository_view = project::task_repository_view("t-001")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            repository_view.status,
            project::RepositoryViewStatus::Available
        );
        assert!(repository_view.baseline_snapshot_id.is_some());
        assert_eq!(
            project::list_tasks().await.unwrap()[0].status,
            project::TaskStatus::Executing.as_str()
        );

        std::fs::write(
            root.join("src/lib.rs"),
            "pub mod added;\npub struct OverlaySymbol;\n",
        )
        .unwrap();
        std::fs::write(root.join("src/added.rs"), "pub struct AddedSymbol;\n").unwrap();
        std::fs::remove_file(root.join("src/deleted.rs")).unwrap();
        let repository_view = refresh_task_overlay("t-001", root, &baseline_tree)
            .await
            .unwrap();
        assert!(repository_view.overlay_revision_id.is_some());

        let overlay_sidecar = match open_for_query_at(&data_dir.join(SIDECAR_FILE_NAME)).unwrap() {
            OpenQuerySidecarResult::Ready(sidecar) => sidecar,
            _ => panic!("refreshed task overlay must be queryable"),
        };
        let overlay_publication = overlay_sidecar
            .published_view(
                &LocalGraphContext::load(false).await.unwrap().repository,
                &task_overlay_view_name(&TaskViewId::new("t-001").unwrap()).unwrap(),
            )
            .unwrap()
            .unwrap();
        let overlay_metrics = overlay_sidecar
            .index_build_metrics(&overlay_publication.build_id)
            .unwrap()
            .unwrap();
        assert_eq!(overlay_metrics.parsed_files, 2);
        assert_eq!(overlay_metrics.reused_files, 1);
        drop(overlay_sidecar);

        let mut task_context = LocalGraphContext::load(false).await.unwrap();
        task_context.repository_view = Some(repository_view.clone());
        task_context.task_view_id = Some(TaskViewId::new("t-001").unwrap());
        task_context.root = root.to_path_buf();
        let search = |text: &str| SearchRequest {
            scope: task_context
                .scope(default_budget(&task_context.config.query_limits).unwrap())
                .unwrap(),
            text: text.to_string(),
            node_kinds: vec![],
            paths: vec![],
            page: repository_graph::query::PageRequest { cursor: None },
        };
        let overlay_response = task_context
            .search(&search("OverlaySymbol"))
            .await
            .unwrap()
            .unwrap();
        assert!(!overlay_response.data.hits.is_empty());
        assert_eq!(
            overlay_response.task_view,
            Some(TaskViewEnvelope {
                task_view_id: TaskViewId::new("t-001").unwrap(),
                baseline_snapshot_id: repository_view.baseline_snapshot_id.clone().unwrap(),
                overlay_revision_id: repository_view.overlay_revision_id.clone(),
                lifecycle: TaskViewLifecycle::Mutable,
            })
        );
        assert!(
            task_context
                .search(&search("BaselineSymbol"))
                .await
                .unwrap()
                .unwrap()
                .data
                .hits
                .is_empty()
        );
        assert!(
            task_context
                .search(&search("DeletedSymbol"))
                .await
                .unwrap()
                .unwrap()
                .data
                .hits
                .is_empty()
        );
        assert!(
            !task_context
                .search(&search("AddedSymbol"))
                .await
                .unwrap()
                .unwrap()
                .data
                .hits
                .is_empty()
        );
        let context_request = ContextRequest {
            scope: task_context
                .scope(default_budget(&task_context.config.query_limits).unwrap())
                .unwrap(),
            seeds: vec![repository_graph::query::ContextSeed::Path(
                repository_graph::domain::RepoPath::new("src/lib.rs").unwrap(),
            )],
            policy: repository_graph::query::ContextPolicy {
                direction: repository_graph::query::EdgeDirection::Both,
                edge_kinds: vec![],
                include_unresolved: false,
                include_external: false,
            },
            page: repository_graph::query::PageRequest { cursor: None },
        };
        let context_response = task_context
            .context_with_snippets(&context_request, NonZeroU64::new(1024).unwrap())
            .await
            .unwrap()
            .unwrap();
        assert!(
            context_response
                .data
                .snippets
                .iter()
                .any(|snippet| snippet.text.contains("OverlaySymbol"))
        );
        assert!(
            context_response
                .data
                .items
                .iter()
                .any(|item| item.path.as_str() == "src/added.rs")
        );
        assert_eq!(context_response.task_view, overlay_response.task_view);

        let frozen_view = repository_view
            .clone()
            .frozen(capture_worktree_tree(root).unwrap())
            .unwrap();
        task_context.repository_view = Some(frozen_view);
        std::fs::write(
            root.join("src/lib.rs"),
            "pub struct ChangedAfterSubmission;\n",
        )
        .unwrap();
        let frozen_response = task_context
            .context_with_snippets(&context_request, NonZeroU64::new(1024).unwrap())
            .await
            .unwrap()
            .unwrap();
        assert!(
            frozen_response
                .data
                .snippets
                .iter()
                .any(|snippet| snippet.text.contains("OverlaySymbol"))
        );
        assert_eq!(
            frozen_response.task_view.unwrap().lifecycle,
            TaskViewLifecycle::FrozenSubmitted
        );

        project::record_task_status(
            "t-002",
            ".ferrus/tasks/t-002.md",
            project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        schedule_task_baseline_pin("t-002", root, Some("invalid-tree")).await;
        for _ in 0..100 {
            if project::task_repository_view("t-002")
                .await
                .unwrap()
                .is_some_and(|view| view.status == project::RepositoryViewStatus::Failed)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(
            project::task_repository_view("t-002")
                .await
                .unwrap()
                .unwrap()
                .status,
            project::RepositoryViewStatus::Failed
        );
        assert_eq!(
            project::list_tasks()
                .await
                .unwrap()
                .into_iter()
                .find(|task| task.id == "t-002")
                .unwrap()
                .status,
            project::TaskStatus::Executing.as_str()
        );

        std::fs::write(root.join("src/lib.rs"), "pub struct BaselineSymbol;\n").unwrap();
        std::fs::write(root.join("src/deleted.rs"), "pub struct DeletedSymbol;\n").unwrap();
        std::fs::remove_file(root.join("src/added.rs")).unwrap();
        schedule_task_baseline_pin("t-002", root, Some(&baseline_tree)).await;
        for _ in 0..100 {
            if project::task_repository_view("t-002")
                .await
                .unwrap()
                .is_some_and(|view| view.status == project::RepositoryViewStatus::Available)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let retried_view = project::task_repository_view("t-002")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            retried_view.status,
            project::RepositoryViewStatus::Available
        );
        assert!(retried_view.baseline_snapshot_id.is_some());

        std::fs::write(root.join("src/lib.rs"), "pub struct AgentEdit;\n").unwrap();
        project::record_task_status(
            "t-003",
            ".ferrus/tasks/t-003.md",
            project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        schedule_task_baseline_pin("t-003", root, Some(&baseline_tree)).await;
        for _ in 0..100 {
            if project::task_repository_view("t-003")
                .await
                .unwrap()
                .is_some_and(|view| view.status == project::RepositoryViewStatus::Available)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let changed_worktree_view = project::task_repository_view("t-003")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            changed_worktree_view.status,
            project::RepositoryViewStatus::Available
        );
        assert!(changed_worktree_view.baseline_snapshot_id.is_some());
        assert_eq!(
            project::list_tasks()
                .await
                .unwrap()
                .into_iter()
                .find(|task| task.id == "t-003")
                .unwrap()
                .status,
            project::TaskStatus::Executing.as_str()
        );
        let previous_agent = std::env::var_os(ENV_AGENT_ID);
        let previous_task = std::env::var_os(ENV_TASK_ID);
        // SAFETY: cwd_lock serializes Ferrus tests that mutate process-global runtime context.
        unsafe {
            std::env::set_var(ENV_AGENT_ID, "executor:codex:missing");
            std::env::set_var(ENV_TASK_ID, "t-missing");
        }
        let invalid_binding = match LocalGraphContext::load(false).await {
            Ok(_) => panic!("invalid task binding unexpectedly selected canonical context"),
            Err(error) => error,
        };
        assert!(invalid_binding.to_string().contains("not attached"));
        // SAFETY: the same lock remains held while the prior environment is restored.
        unsafe {
            match previous_agent {
                Some(value) => std::env::set_var(ENV_AGENT_ID, value),
                None => std::env::remove_var(ENV_AGENT_ID),
            }
            match previous_task {
                Some(value) => std::env::set_var(ENV_TASK_ID, value),
                None => std::env::remove_var(ENV_TASK_ID),
            }
        }
        std::env::set_current_dir(previous).unwrap();
    }

    #[test]
    fn context_snippets_are_deduplicated_hash_verified_and_stale_safe() {
        let directory = tempfile::tempdir().unwrap();
        let sidecar = directory.path().join(SIDECAR_FILE_NAME);
        let (mut context, request) = indexed_context(directory.path(), &sidecar);
        let response = context_response_at(&context, &sidecar, None, &request).unwrap();

        let enriched = attach_snippets_at(
            &context,
            &sidecar,
            &request,
            response.clone(),
            NonZeroU64::new(1024).unwrap(),
        )
        .unwrap();
        assert!(!enriched.data.snippets.is_empty());
        assert_eq!(enriched.data.items, response.data.items);
        assert_eq!(enriched.page, response.page);
        assert!(
            enriched
                .data
                .snippets
                .iter()
                .all(|snippet| snippet.text.contains("RuntimeTaskContext"))
        );
        let unique = enriched
            .data
            .snippets
            .iter()
            .map(|snippet| serde_json::to_string(&(snippet.path.clone(), snippet.span.clone())))
            .collect::<Result<BTreeSet<_>, _>>()
            .unwrap();
        assert_eq!(unique.len(), enriched.data.snippets.len());

        std::fs::write(
            directory.path().join("src/lib.rs"),
            b"pub struct Changed;\n",
        )
        .unwrap();
        context.config.query_limits.max_diagnostics = 1;
        let stale = attach_snippets_at(
            &context,
            &sidecar,
            &request,
            response,
            NonZeroU64::new(1024).unwrap(),
        )
        .unwrap();
        assert!(stale.data.snippets.is_empty());
        assert_eq!(stale.diagnostics.items.len(), 1);
        assert!(stale.diagnostics.summary.warning > 1);
        assert!(stale.diagnostics.truncated);
        assert!(
            stale
                .diagnostics
                .items
                .iter()
                .any(|diagnostic| diagnostic.code.as_str() == "content.changed")
        );
    }
}
