//! Managed Ferrus host: capture launch authority once, then call typed operations.
//! Model tool arguments must never supply project, agent, task, run, or workspace identity.

use std::path::PathBuf;

use anyhow::{Context, Result, ensure};

use crate::{
    agent_id::{ENV_AGENT_ID, ENV_BASELINE_TREE, ENV_PROJECT_ROOT, ENV_RUN_ID, ENV_TASK_ID},
    config::Config,
    project::{self, ExecutorSessionScope, LeaseRenewal, ReadyTaskClaim, RuntimeTaskContext},
};

/// Trusted HQ launch input, deliberately not serializable as a model tool schema.
#[derive(Debug, Clone)]
pub(crate) struct LaunchContext {
    pub project_root: PathBuf,
    pub workspace: PathBuf,
    pub agent_id: String,
    pub task_id: String,
    pub run_id: String,
    pub baseline_tree: Option<String>,
}

impl LaunchContext {
    pub(crate) fn from_env() -> Result<Self> {
        Self::capture(|key| std::env::var(key).ok(), std::env::current_dir()?)
    }

    fn capture(get: impl Fn(&str) -> Option<String>, workspace: PathBuf) -> Result<Self> {
        let required = |key| {
            get(key)
                .filter(|value| !value.trim().is_empty())
                .with_context(|| format!("Missing managed launch context: {key}"))
        };

        let baseline_tree = get(ENV_BASELINE_TREE);
        ensure!(
            baseline_tree
                .as_ref()
                .is_none_or(|value| !value.trim().is_empty()),
            "Empty managed baseline tree"
        );

        Ok(Self {
            project_root: PathBuf::from(required(ENV_PROJECT_ROOT)?),
            workspace,
            agent_id: required(ENV_AGENT_ID)?,
            task_id: required(ENV_TASK_ID)?,
            run_id: required(ENV_RUN_ID)?,
            baseline_tree,
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct FerrusSession {
    pub(super) scope: ExecutorSessionScope,
    project_id: String,
    project_root: PathBuf,
    baseline_tree: Option<String>,
    baseline_path: PathBuf,
    ttl_secs: u64,
}

impl FerrusSession {
    pub(crate) async fn bind(launch: LaunchContext) -> Result<Self> {
        ensure!(
            !launch.agent_id.trim().is_empty()
                && !launch.task_id.trim().is_empty()
                && !launch.run_id.trim().is_empty(),
            "Missing managed launch identity"
        );

        ensure!(
            launch.project_root.is_absolute() && launch.workspace.is_absolute(),
            "Managed launch paths must be absolute"
        );

        let project_root = tokio::fs::canonicalize(&launch.project_root).await?;
        let workspace = tokio::fs::canonicalize(&launch.workspace).await?;
        let registration = project::read_project_registration_at(&project_root).await?;
        let workspace_registration = project::read_project_registration_at(&workspace).await?;
        ensure!(
            registration.metadata.id == workspace_registration.metadata.id
                && registration.data_dir == workspace_registration.data_dir
                && tokio::fs::canonicalize(&registration.metadata.workspace_dir).await?
                    == project_root,
            "Managed project binding mismatch"
        );
        // Task IDs also name baseline metadata files. Accept one normal path component only.
        ensure!(
            PathBuf::from(&launch.task_id).components().count() == 1
                && !matches!(launch.task_id.as_str(), "." | "..")
                && !launch.task_id.contains(['/', '\\']),
            "Invalid managed task ID"
        );

        let baseline_path = registration
            .data_dir
            .join("worktrees/.baseline-trees")
            .join(format!("{}.txt", launch.task_id));

        let session = Self {
            scope: ExecutorSessionScope {
                database_path: registration.database_path,
                agent_id: launch.agent_id,
                task_id: launch.task_id,
                run_id: launch.run_id,
                workspace_path: workspace,
            },
            project_id: registration.metadata.id,
            ttl_secs: Config::load_from(&project_root).await?.lease.ttl_secs,
            project_root,
            baseline_tree: launch.baseline_tree,
            baseline_path,
        };

        session.status().await?;
        Ok(session)
    }

    pub(crate) fn data_dir(&self) -> &std::path::Path {
        self.scope
            .database_path
            .parent()
            .expect("bound database has a parent")
    }

    pub(crate) fn workspace(&self) -> &std::path::Path {
        &self.scope.workspace_path
    }

    pub(crate) fn project_id(&self) -> &str {
        &self.project_id
    }

    pub(crate) fn project_root(&self) -> &std::path::Path {
        &self.project_root
    }

    pub(crate) fn baseline_tree(&self) -> Option<&str> {
        self.baseline_tree.as_deref()
    }

    pub(super) fn lease_ttl_secs(&self) -> u64 {
        self.ttl_secs
    }

    /// One nonblocking claim attempt; the future engine owns polling and cancellation.
    pub(crate) async fn claim(&self) -> Result<ReadyTaskClaim> {
        self.validate_baseline().await?;
        project::claim_executor_session(&self.scope, self.ttl_secs).await
    }

    pub(crate) async fn status(&self) -> Result<RuntimeTaskContext> {
        self.validate_baseline().await?;
        project::executor_session_status(&self.scope).await
    }

    pub(crate) async fn heartbeat(&self) -> Result<LeaseRenewal> {
        self.validate_baseline().await?;
        project::renew_executor_session_lease(&self.scope, self.ttl_secs).await
    }

    /// Never reclaim here: after initial claim, loss of ownership ends this session.
    pub(crate) async fn authorize(&self) -> Result<RuntimeTaskContext> {
        self.validate_baseline().await?;
        project::with_executor_session(&self.scope, false, |tx, scope, context| {
            project::require_executor_owner(tx, scope)?;
            ensure!(
                matches!(
                    context.status.as_str(),
                    "executing" | "addressing" | "consultation" | "awaiting_human"
                ),
                "Executor work phase ended"
            );
            Ok(context)
        })
        .await
    }

    /// Inspect only the contiguous prior Executor runs for this agent, task,
    /// and workspace. Empty runs may be skipped, but a different owner or
    /// workspace is a hard history boundary. No prior lease is inherited.
    pub(crate) async fn previous_nano_runs(&self) -> Result<Vec<String>> {
        let workspace = self.workspace().to_string_lossy().into_owned();
        self.mutate(move |tx, scope, _| {
            let mut statement = tx.prepare(
                "SELECT id, agent, workspace_path, status FROM runs \
                 WHERE rowid < (SELECT rowid FROM runs WHERE id = ?1) \
                 AND task_id = ?2 AND role = 'executor' \
                 ORDER BY rowid DESC LIMIT 65",
            )?;
            let rows =
                statement.query_map(rusqlite::params![scope.run_id, scope.task_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })?;
            let mut runs = Vec::new();
            for row in rows {
                let (id, agent, path, status) = row?;
                if agent != scope.agent_id || path != workspace {
                    break;
                }
                ensure!(
                    matches!(status.as_str(), "failed" | "interrupted" | "completed"),
                    "Previous Executor run is still active"
                );
                runs.push(id);
            }
            Ok(runs)
        })
        .await
    }

    pub(crate) async fn fail_unknown_effect(&self) -> Result<()> {
        self.mutate(|tx, scope, context| {
            ensure!(
                matches!(
                    context.status.as_str(),
                    "executing" | "addressing" | "consultation" | "awaiting_human"
                ),
                "Unknown effect cannot fail a terminal task"
            );
            let updated = tx.execute(
                "UPDATE tasks SET status = 'failed', failure_reason = 'nano_effect_unknown', \
                 claimed_by = NULL, lease_until = NULL, last_heartbeat = NULL WHERE id = ?1",
                [&scope.task_id],
            )?;
            ensure!(updated == 1, "Bound task disappeared");
            project::executor_event(
                tx,
                scope,
                "nano_effect_unknown",
                serde_json::json!({"task_id":scope.task_id}),
            )?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn previous_submit_committed(&self, run_id: String) -> Result<bool> {
        self.mutate(move |tx, _, _| {
            let committed: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM events WHERE run_id = ?1 AND type = 'submitted')",
                [run_id],
                |row| row.get(0),
            )?;
            Ok(committed)
        })
        .await
    }

    pub(super) async fn mutate<T: Send + 'static>(
        &self,
        operation: impl FnOnce(
            &rusqlite::Transaction<'_>,
            &ExecutorSessionScope,
            RuntimeTaskContext,
        ) -> Result<T>
        + Send
        + 'static,
    ) -> Result<T> {
        self.validate_baseline().await?;
        project::with_executor_session(&self.scope, true, move |tx, scope, context| {
            project::require_executor_owner(tx, scope)?;
            operation(tx, scope, context)
        })
        .await
    }

    async fn validate_baseline(&self) -> Result<()> {
        // The Git baseline predates graph support and is stored by HQ separately
        // from optional snapshot IDs in SQLite. Do not infer one from the other.
        let recorded = match tokio::fs::read_to_string(&self.baseline_path).await {
            Ok(value) => Some(value.trim().to_string()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };

        ensure!(
            recorded.as_ref().is_none_or(|value| !value.is_empty())
                && recorded == self.baseline_tree,
            "Managed baseline binding mismatch"
        );
        // HQ only omits the baseline for a non-Git canonical workspace.
        ensure!(
            self.baseline_tree.is_some()
                || (self.scope.workspace_path == self.project_root
                    && !tokio::fs::try_exists(self.project_root.join(".git")).await?),
            "Managed Git workspace requires a baseline tree"
        );

        Ok(())
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
