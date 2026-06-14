use anyhow::Result;
use neva::prelude::*;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::info;

use crate::{
    config::Config,
    project::{self, RuntimeTaskContext, TaskConsultRestore},
    state::store,
};

use super::{ensure_lease_owner_or_reclaim, require_runtime_task_context, tool_err};

pub const DESCRIPTION: &str = "Block until CONSULT_RESPONSE.md exists, then restore the pre-consult state and \
     return the consultant's response text. Each call waits up to `wait_timeout_secs` and then \
     returns an error telling the agent to call /wait_for_consult again. Must only be called while state is Consultation.";

pub async fn handler(ctx: neva::Context) -> Result<String, Error> {
    let agent_id = super::agent_id_from_context(&ctx)?;
    handler_for_agent(&agent_id).await
}

pub async fn handler_for_agent(agent_id: &str) -> Result<String, Error> {
    run(agent_id).await.map_err(tool_err)
}

async fn run(agent_id: &str) -> Result<String> {
    let config = Config::load().await?;
    let timeout = Duration::from_secs(config.limits.wait_timeout_secs);
    let start = Instant::now();

    let context = require_runtime_task_context(agent_id).await?;
    if context.status.parse::<project::TaskStatus>()? != project::TaskStatus::Consultation {
        anyhow::bail!(
            "Cannot wait for consultation from state {}. Call /consult first.",
            context.status
        );
    }
    ensure_lease_owner_or_reclaim(agent_id, config.lease.ttl_secs).await?;

    loop {
        match read_consult_response(&context).await {
            Ok(response) if !response.trim().is_empty() => {
                let restored = project::restore_task_from_consultation(&context.task_id).await?;
                let resumed = match restored {
                    TaskConsultRestore::Restored { status } => status,
                    TaskConsultRestore::NotInConsultation => context.status.clone(),
                };
                store::clear_consult_response_for_run_dir(&context.run_dir).await?;
                store::clear_consult_request_for_run_dir(&context.run_dir).await?;

                let response = response.trim().to_string();
                info!(
                    task_id = context.task_id,
                    resumed, "Consultation answered; DB task restored"
                );
                return Ok(response);
            }
            _ => {}
        }

        if start.elapsed() >= timeout {
            anyhow::bail!(
                "Timed out waiting for CONSULT_RESPONSE.md. Call /wait_for_consult again to keep waiting."
            );
        }

        sleep(Duration::from_millis(500)).await;
    }
}

async fn read_consult_response(context: &RuntimeTaskContext) -> Result<String> {
    store::read_consult_response_for_run_dir(&context.run_dir).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::store;
    use tempfile::TempDir;

    async fn setup() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::fs::create_dir_all(dir.path().join(".ferrus/tasks")).unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        tokio::fs::write(
            "ferrus.toml",
            "[checks]\ncommands = []\n\n[limits]\nmax_check_retries = 20\nmax_review_cycles = 3\nmax_feedback_lines = 30\nwait_timeout_secs = 1\n\n[lease]\nttl_secs = 60\n",
        )
        .await
        .unwrap();
        let data_dir = dir.path().join(".ferrus/projects/test-project");
        tokio::fs::create_dir_all(&data_dir).await.unwrap();
        let local_ref = crate::project::LocalProjectRef {
            project_id: "test-project".to_string(),
            name: "test".to_string(),
            data_dir: data_dir.to_string_lossy().into_owned(),
        };
        tokio::fs::write(
            ".ferrus/project.toml",
            toml::to_string_pretty(&local_ref).unwrap(),
        )
        .await
        .unwrap();
        (dir, previous)
    }

    fn teardown(previous: std::path::PathBuf) {
        std::env::set_current_dir(previous).unwrap();
    }

    #[tokio::test]
    async fn wait_for_consult_restores_agent_runtime_task_from_scoped_response() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        crate::project::record_task_status(
            "t-007",
            ".ferrus/tasks/t-007.md",
            crate::project::TaskStatus::Addressing,
        )
        .await
        .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "executor:codex:7", 60)
            .await
            .unwrap();
        crate::project::record_task_consultation_requested(
            "t-007",
            crate::project::TaskStatus::Addressing,
        )
        .await
        .unwrap();
        store::write_consult_request_for_run_dir(".ferrus/runs/t-007", "question")
            .await
            .unwrap();
        store::write_consult_response_for_run_dir(".ferrus/runs/t-007", "answer\n")
            .await
            .unwrap();

        let response = run("executor:codex:7").await.unwrap();

        assert_eq!(response, "answer");
        crate::test_support::assert_no_state_json();
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "addressing");
        assert_eq!(task.paused_status, None);
        assert_eq!(task.claimed_by.as_deref(), Some("executor:codex:7"));
        assert_eq!(
            store::read_consult_request_for_run_dir(".ferrus/runs/t-007")
                .await
                .unwrap(),
            ""
        );
        assert_eq!(
            store::read_consult_response_for_run_dir(".ferrus/runs/t-007")
                .await
                .unwrap(),
            ""
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn wait_for_consult_uses_database_context_when_state_json_is_absent() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        crate::project::record_task_status(
            "t-007",
            ".ferrus/tasks/t-007.md",
            crate::project::TaskStatus::Addressing,
        )
        .await
        .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "executor:codex:7", 60)
            .await
            .unwrap();
        crate::project::record_task_consultation_requested(
            "t-007",
            crate::project::TaskStatus::Addressing,
        )
        .await
        .unwrap();
        store::write_consult_response_for_run_dir(".ferrus/runs/t-007", "answer\n")
            .await
            .unwrap();

        let response = run("executor:codex:7").await.unwrap();

        assert_eq!(response, "answer");
        crate::test_support::assert_no_state_json();
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "addressing");
        assert_eq!(task.paused_status, None);
        assert_eq!(task.claimed_by.as_deref(), Some("executor:codex:7"));

        teardown(previous);
    }

    #[tokio::test]
    async fn wait_for_consult_restores_database_task_status() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        crate::project::record_task_status(
            "t-001",
            ".ferrus/tasks/t-001.md",
            crate::project::TaskStatus::Addressing,
        )
        .await
        .unwrap();
        crate::project::claim_task("t-001", ".ferrus/tasks/t-001.md", "executor:codex:1", 60)
            .await
            .unwrap();
        crate::project::record_task_consultation_requested(
            "t-001",
            crate::project::TaskStatus::Addressing,
        )
        .await
        .unwrap();
        store::write_consult_response_for_run_dir(".ferrus/runs/t-001", "answer\n")
            .await
            .unwrap();

        let response = run("executor:codex:1").await.unwrap();

        assert_eq!(response, "answer");
        crate::test_support::assert_no_state_json();
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-001").unwrap();
        assert_eq!(task.status, "addressing");
        assert_eq!(task.paused_status, None);
        assert_eq!(
            store::read_consult_response_for_run_dir(".ferrus/runs/t-001")
                .await
                .unwrap(),
            ""
        );

        teardown(previous);
    }
}
