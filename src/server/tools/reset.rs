//! Mark a failed task Reset through its runtime context while preserving scoped artifacts.

use anyhow::Result;
use neva::prelude::*;
use tracing::info;

use crate::project::{self, RuntimeTaskContext};

use super::{require_runtime_task_context, tool_err};

pub const DESCRIPTION: &str = "Human escape hatch: reset a Failed SQLite task row.";

pub async fn handler(ctx: neva::di::Dc<crate::server::ServerContext>) -> Result<String, Error> {
    handler_for_agent(ctx.agent_id()).await
}

pub async fn handler_for_agent(agent_id: &str) -> Result<String, Error> {
    run_for_agent(Some(agent_id)).await.map_err(tool_err)
}

async fn run_for_agent(agent_id: Option<&str>) -> Result<String> {
    let Some(agent_id) = agent_id else {
        anyhow::bail!("Cannot reset without an agent runtime context");
    };
    let context = require_runtime_task_context(agent_id).await?;
    reset_runtime_task(&context).await
}

async fn reset_runtime_task(context: &RuntimeTaskContext) -> Result<String> {
    if context.status.parse::<project::TaskStatus>()? != project::TaskStatus::Failed {
        anyhow::bail!(
            "Cannot reset task {} from status {}. Reset is only available for failed tasks.",
            context.task_id,
            context.status
        );
    }

    project::record_task_status_with_origin(
        &context.task_id,
        &context.task_path,
        project::TaskStatus::Reset,
        None,
        None,
    )
    .await?;
    project::record_runtime_event_best_effort(
        context.run_id.clone(),
        "reset",
        serde_json::json!({ "task_id": context.task_id }),
    )
    .await;

    info!(task_id = context.task_id, "Task reset in ferrus.db");
    Ok(format!(
        "Task {} reset. State: reset. The task artifact remains at {}.",
        context.task_id, context.task_path
    ))
}

#[cfg(test)]
mod tests {
    //! Only failed SQLite tasks may be reset through MCP.

    use super::*;
    use tempfile::TempDir;

    async fn setup() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        let data_dir = dir.path().join(".ferrus/projects/test-project");
        std::fs::create_dir_all(dir.path().join(".ferrus")).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();
        let local_ref = crate::project::LocalProjectRef {
            project_id: "test-project".to_string(),
            name: "test".to_string(),
            data_dir: data_dir.display().to_string(),
        };
        let local_ref = toml::to_string_pretty(&local_ref).unwrap();
        std::fs::write(dir.path().join(".ferrus/project.toml"), local_ref).unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        (dir, previous)
    }

    fn teardown(previous: std::path::PathBuf) {
        std::env::set_current_dir(previous).unwrap();
    }

    #[tokio::test]
    async fn reset_uses_database_context_when_state_json_is_absent() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        crate::project::record_task_status(
            "t-007",
            ".ferrus/tasks/t-007.md",
            crate::project::TaskStatus::Failed,
        )
        .await
        .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "executor:codex:7", 60)
            .await
            .unwrap();

        let response = run_for_agent(Some("executor:codex:7")).await.unwrap();

        assert!(response.contains("Task t-007 reset"));
        crate::test_support::assert_no_state_json();
        let tasks = crate::project::list_tasks().await.unwrap();
        assert_eq!(tasks[0].status, "reset");

        teardown(previous);
    }

    #[tokio::test]
    async fn reset_rejects_non_failed_database_task() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        crate::project::record_task_status(
            "t-007",
            ".ferrus/tasks/t-007.md",
            crate::project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "executor:codex:7", 60)
            .await
            .unwrap();

        let message = run_for_agent(Some("executor:codex:7"))
            .await
            .unwrap_err()
            .to_string();

        assert!(message.contains("Reset is only available for failed tasks"));
        let tasks = crate::project::list_tasks().await.unwrap();
        assert_eq!(tasks[0].status, "executing");

        teardown(previous);
    }
}
