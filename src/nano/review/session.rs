//! Exact HQ run binding for independent review and consultation.
use super::*;
use crate::{
    config::Config,
    project::{self, ExecutorSessionScope, RuntimeTaskContext},
};
use anyhow::{Context, Result, ensure};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Review,
    Consult,
}
#[derive(Clone)]
pub(crate) struct Session {
    pub scope: ExecutorSessionScope,
    pub mode: Mode,
    pub root: PathBuf,
    pub data: PathBuf,
    pub project_id: String,
    pub ttl: u64,
    pub source: Arc<source::Source>,
}
impl Session {
    pub async fn bind(launch: super::super::ferrus::LaunchContext) -> Result<Self> {
        ensure!(
            launch.agent_id.starts_with("supervisor:nano:")
                && launch.project_root.is_absolute()
                && launch.workspace.is_absolute(),
            "Invalid Supervisor launch identity"
        );
        ensure!(
            super::super::journal::valid_id(&launch.task_id)
                && super::super::journal::valid_id(&launch.run_id),
            "Invalid Supervisor task/run ID"
        );
        let root = launch.project_root.canonicalize()?;
        let registration = project::read_project_registration_at(&root).await?;
        let workspace = launch.workspace.canonicalize()?;
        let workspace_registration = project::read_project_registration_at(&workspace).await?;
        ensure!(
            workspace_registration.metadata.id == registration.metadata.id
                && workspace_registration.data_dir == registration.data_dir,
            "Supervisor workspace belongs to another project"
        );
        ensure!(
            Path::new(&registration.metadata.workspace_dir).canonicalize()? == root,
            "Supervisor project root mismatch"
        );
        let scope = ExecutorSessionScope {
            database_path: registration.database_path,
            agent_id: launch.agent_id,
            task_id: launch.task_id,
            run_id: launch.run_id,
            workspace_path: workspace.clone(),
        };
        let (context, resume) =
            project::with_supervisor_session(&scope, false, |tx, scope, context| {
                let resume: Option<String> = tx.query_row(
                    "SELECT awaiting_human_status FROM tasks WHERE id=?1",
                    [&scope.task_id],
                    |row| row.get(0),
                )?;
                Ok((context, resume))
            })
            .await?;
        let mode = match if context.status == "awaiting_human" {
            resume.as_deref().unwrap_or("")
        } else {
            context.status.as_str()
        } {
            "reviewing" => Mode::Review,
            "consultation" => Mode::Consult,
            _ => anyhow::bail!("No review or consultation for this run"),
        };
        let ttl = Config::load_from(&root).await?.lease.ttl_secs;
        ensure!(ttl > 0, "Supervisor lease TTL must be positive");
        let source = if mode == Mode::Review {
            ensure!(
                context.repository_view.lifecycle
                    == crate::repository_graph::domain::TaskViewLifecycle::FrozenSubmitted,
                "Reviewer submitted view is missing"
            );
            let tree = context
                .repository_view
                .frozen_source_tree
                .as_ref()
                .context("Reviewer submitted tree is missing")?
                .value()
                .to_string();
            let parent = registration.data_dir.join("nano");
            super::super::private::directory(&parent, false)?;
            let parent = parent.join("review-sources");
            super::super::private::directory(&parent, false)?;
            let target = parent.join(&scope.run_id);
            let owned_root = root.clone();
            Arc::new(
                tokio::task::spawn_blocking(move || source::snapshot(&owned_root, &target, &tree))
                    .await??,
            )
        } else {
            Arc::new(source::Source {
                path: workspace,
                owned: false,
                omitted_links: 0,
            })
        };
        project::start_supervisor_session(&scope, mode == Mode::Review, ttl).await?;
        let session = Self {
            scope,
            mode,
            root,
            data: registration.data_dir,
            project_id: registration.metadata.id,
            ttl,
            source,
        };
        session.authorize().await?;
        Ok(session)
    }
    pub async fn status(&self) -> Result<RuntimeTaskContext> {
        let mode = self.mode;
        let path = self.source.path.to_string_lossy().into_owned();
        project::with_supervisor_session(&self.scope,false,move |tx,scope,mut context| {
            if mode==Mode::Consult {
                let workspace:Option<String>=tx.query_row("SELECT workspace_path FROM runs WHERE role='executor' AND task_id=?1 ORDER BY updated_at DESC, started_at DESC, id DESC LIMIT 1",[&scope.task_id],|row| row.get(0))?;
                ensure!(Path::new(&workspace.context("Missing Executor workspace")?).canonicalize()?==scope.workspace_path,"Executor workspace changed");
                context.repository_view=tx.query_row("SELECT baseline_snapshot_id, overlay_revision_id, repository_view_snapshot_id, repository_view_tree_algorithm, repository_view_tree_digest, repository_view_lifecycle, repository_view_status FROM tasks WHERE id=?1",[&scope.task_id],|row|project::repository_view_reference_from_row(row,0))?;
            }
            context.repository_workspace_path=Some(path);
            Ok(context)
        }).await
    }
    pub async fn authorize(&self) -> Result<RuntimeTaskContext> {
        let mode = self.mode;
        project::with_supervisor_session(&self.scope, false, move |tx, scope, context| {
            let competing:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runs WHERE task_id=?1 AND role='supervisor' AND status IN ('running','checking','reviewing') AND id!=?2)",rusqlite::params![scope.task_id,scope.run_id],|row|row.get(0))?;
            ensure!(!competing,"Another Supervisor run owns this task attachment");
            if context.status == "awaiting_human" {
                let (owner, resume): (Option<String>, Option<String>) = tx.query_row(
                    "SELECT awaiting_human_by, awaiting_human_status FROM tasks WHERE id=?1",
                    [&scope.task_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                ensure!(
                    owner.as_deref() == Some(&scope.agent_id)
                        && resume.as_deref()
                            == Some(if mode == Mode::Review {
                                "reviewing"
                            } else {
                                "consultation"
                            }),
                    "Question binding mismatch"
                );
            } else {
                ensure!(
                    context.status
                        == if mode == Mode::Review {
                            "reviewing"
                        } else {
                            "consultation"
                        },
                    "Supervisor phase ended"
                );
            }
            if mode==Mode::Consult {
                let workspace:String=tx.query_row("SELECT workspace_path FROM runs WHERE role='executor' AND task_id=?1 ORDER BY updated_at DESC, started_at DESC, id DESC LIMIT 1",[&scope.task_id],|row|row.get(0))?;
                ensure!(Path::new(&workspace).canonicalize()?==scope.workspace_path,"Consultation Executor workspace changed");
            }
            if mode == Mode::Review {
                project::require_executor_owner(tx, scope)?;
                let tree: Option<String> = tx.query_row(
                    "SELECT repository_view_tree_digest FROM tasks WHERE id=?1",
                    [&scope.task_id],
                    |row| row.get(0),
                )?;
                ensure!(
                    tree.as_deref()
                        == context
                            .repository_view
                            .frozen_source_tree
                            .as_ref()
                            .map(|tree| tree.value()),
                    "Review submission changed"
                );
            }
            Ok(context)
        })
        .await
    }
}

impl Session {
    pub async fn fail_stopped(&self, code: &'static str) -> Result<()> {
        let root = self.root.clone();
        let mode = self.mode;
        project::with_supervisor_session(&self.scope,true,move |tx,scope,context| {
            if context.status != if mode==Mode::Review {"reviewing"} else {"consultation"} {return Ok(());}
            if mode==Mode::Review {project::require_executor_owner(tx,scope)?;} else if super::super::workspace::instruction_file(&root,&format!("{}/CONSULT_RESPONSE.md",context.run_dir),16384)?.is_some_and(|(text,_)|!text.trim().is_empty()) {return Ok(());}
            tx.execute("UPDATE tasks SET status='failed',failure_reason=?2,claimed_by=NULL,lease_until=NULL,last_heartbeat=NULL WHERE id=?1",rusqlite::params![scope.task_id,code])?;
            project::executor_event(tx,scope,"nano_supervisor_stopped",json!({"task_id":scope.task_id,"reason":code}))?;
            Ok(())
        }).await
    }
}
