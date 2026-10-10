//! Native tools share project context; only managed sessions own a task lease.

use super::ferrus::FerrusSession;
use anyhow::Result;
#[cfg(feature = "nano-openai")]
use anyhow::ensure;
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub(crate) enum Binding {
    Managed(FerrusSession),
    Interactive {
        project_root: PathBuf,
        project_id: String,
        data_dir: PathBuf,
        agent_id: String,
        run_id: String,
    },
}

impl From<FerrusSession> for Binding {
    fn from(session: FerrusSession) -> Self {
        Self::Managed(session)
    }
}

impl Binding {
    #[cfg(feature = "nano-openai")]
    pub(crate) async fn interactive_from_env() -> Result<Self> {
        use crate::agent_id::{
            ENV_AGENT_ID, ENV_BASELINE_TREE, ENV_PROJECT_ROOT, ENV_RUN_ID, ENV_TASK_ID,
        };
        let required = |key| {
            std::env::var(key)
                .map_err(|_| anyhow::anyhow!("Missing interactive launch context: {key}"))
        };
        ensure!(
            [ENV_TASK_ID, ENV_BASELINE_TREE]
                .iter()
                .all(|key| { std::env::var(key).ok().is_none_or(|value| value.is_empty()) }),
            "Taskless launch cannot carry a task or baseline binding"
        );
        let root = PathBuf::from(required(ENV_PROJECT_ROOT)?);
        ensure!(
            root.is_absolute(),
            "Interactive project root must be absolute"
        );
        let project_root = tokio::fs::canonicalize(root).await?;
        ensure!(
            tokio::fs::canonicalize(std::env::current_dir()?).await? == project_root,
            "Taskless interactive sessions must use the canonical workspace"
        );
        let registration = crate::project::read_project_registration_at(&project_root).await?;
        ensure!(
            tokio::fs::canonicalize(&registration.metadata.workspace_dir).await? == project_root,
            "Interactive project binding mismatch"
        );
        let binding = Self::Interactive {
            project_root,
            project_id: registration.metadata.id,
            data_dir: registration.data_dir,
            agent_id: required(ENV_AGENT_ID)?,
            run_id: required(ENV_RUN_ID)?,
        };
        binding.status().await?;
        Ok(binding)
    }

    pub(crate) fn managed(&self) -> Option<&FerrusSession> {
        match self {
            Self::Managed(session) => Some(session),
            Self::Interactive { .. } => None,
        }
    }
    pub(crate) fn project_root(&self) -> &Path {
        match self {
            Self::Managed(session) => session.project_root(),
            Self::Interactive { project_root, .. } => project_root,
        }
    }
    pub(crate) fn workspace(&self) -> &Path {
        self.managed()
            .map_or_else(|| self.project_root(), FerrusSession::workspace)
    }
    pub(crate) fn project_id(&self) -> &str {
        match self {
            Self::Managed(session) => session.project_id(),
            Self::Interactive { project_id, .. } => project_id,
        }
    }
    pub(crate) fn data_dir(&self) -> &Path {
        match self {
            Self::Managed(session) => session.data_dir(),
            Self::Interactive { data_dir, .. } => data_dir,
        }
    }
    pub(crate) fn run_id(&self) -> &str {
        match self {
            Self::Managed(session) => &session.scope.run_id,
            Self::Interactive { run_id, .. } => run_id,
        }
    }
    pub(crate) fn task_id(&self) -> Option<&str> {
        self.managed().map(|session| session.scope.task_id.as_str())
    }
    pub(crate) async fn status(&self) -> Result<Option<crate::project::RuntimeTaskContext>> {
        match self {
            Self::Managed(session) => session.status().await.map(Some),
            Self::Interactive {
                agent_id, run_id, ..
            } => {
                crate::project::authorize_taskless_executor_run_at(
                    &self.data_dir().join("ferrus.db"),
                    run_id,
                    agent_id,
                    self.workspace(),
                )
                .await?;
                Ok(None)
            }
        }
    }
    pub(crate) async fn graph(&self) -> Result<crate::repository_graph_runtime::LocalGraphContext> {
        match self.status().await? {
            Some(runtime) => {
                crate::repository_graph_runtime::LocalGraphContext::load_for_runtime(
                    self.project_root(),
                    self.project_id(),
                    self.data_dir(),
                    &runtime,
                )
                .await
            }
            None => {
                crate::repository_graph_runtime::LocalGraphContext::load_canonical(
                    self.project_root(),
                    self.project_id(),
                    self.data_dir(),
                )
                .await
            }
        }
    }
    pub(crate) async fn project_context(
        &self,
        domain: crate::project_memory::federation::ContextDomain,
        snippets: bool,
    ) -> Result<crate::project_memory_runtime::LocalProjectContext> {
        let runtime = self.status().await?;
        match runtime {
            Some(runtime) => {
                crate::project_memory_runtime::LocalProjectContext::load_for_runtime(
                    self.project_root(),
                    self.project_id(),
                    self.data_dir(),
                    &runtime,
                    domain,
                    snippets,
                )
                .await
            }
            None => {
                crate::project_memory_runtime::LocalProjectContext::load_for_binding(
                    self.project_root(),
                    self.project_id(),
                    self.data_dir(),
                    None,
                    domain,
                    snippets,
                )
                .await
            }
        }
    }
}

pub(super) fn view_identity(view: &crate::project::RepositoryViewReference) -> serde_json::Value {
    serde_json::json!({"baseline_snapshot_id":view.baseline_snapshot_id, "overlay_revision_id":view.overlay_revision_id,
        "view_snapshot_id":view.view_snapshot_id, "lifecycle":view.lifecycle, "status":view.status.as_str()})
}

impl super::instructions::InstructionScope for Binding {
    async fn snapshot(&self) -> anyhow::Result<super::instructions::ScopeSnapshot> {
        use super::instructions::{INTERACTIVE_POLICY, ROLE_POLICY, ScopeSnapshot};
        let runtime = self.status().await?;
        let mut scope = ScopeSnapshot {
            task_id: self.task_id().unwrap_or_default().into(),
            run_id: Some(self.run_id().into()),
            task_status: runtime
                .as_ref()
                .map_or("interactive", |r| r.status.as_str())
                .into(),
            policy: if runtime.is_some() {
                ROLE_POLICY
            } else {
                INTERACTIVE_POLICY
            },
            task_path: None,
            review_path: None,
        };
        if let Some(runtime) = runtime {
            let task_path = format!(".ferrus/tasks/{}.md", runtime.task_id);
            anyhow::ensure!(
                runtime.task_path == task_path,
                "Unexpected managed task artifact"
            );
            scope.task_path = Some(task_path);
            if runtime.status == "addressing"
                || runtime.paused_status.as_deref() == Some("addressing")
                || runtime.review_cycles > 0
            {
                scope.review_path = Some(format!(".ferrus/runs/{}/REVIEW.md", runtime.task_id));
            }
        }
        Ok(scope)
    }
    fn project_root(&self) -> &std::path::Path {
        Binding::project_root(self)
    }
    fn workspace(&self) -> &std::path::Path {
        Binding::workspace(self)
    }
}

impl super::instructions::InstructionScope for super::ferrus::FerrusSession {
    async fn snapshot(&self) -> anyhow::Result<super::instructions::ScopeSnapshot> {
        Binding::from(self.clone()).snapshot().await
    }
    fn project_root(&self) -> &std::path::Path {
        super::ferrus::FerrusSession::project_root(self)
    }
    fn workspace(&self) -> &std::path::Path {
        super::ferrus::FerrusSession::workspace(self)
    }
}
