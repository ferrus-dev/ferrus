use anyhow::Result;
use neva::prelude::*;
use serde_json::json;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::info;

use crate::{agent_id::ENV_TASK_ID, config::Config, project, state::store};

use super::tool_err;

pub const DESCRIPTION: &str = "Block until an Executor consultation request is ready, attach this Supervisor run to that task, and return the consultation context. \
     Returns a JSON object: {\"status\":\"claimed\", \"task_id\":\"...\", \"task\":\"...\", \"consult_request\":\"...\"} \
     when consultation is ready, or {\"status\":\"timeout\"} on timeout. \
     Each call waits up to `wait_timeout_secs` (see ferrus.toml), then returns timeout so the Supervisor can poll again.";

pub async fn handler(ctx: neva::di::Dc<crate::server::ServerContext>) -> Result<String, Error> {
    handler_for_agent(ctx.agent_id()).await
}

pub async fn handler_for_agent(agent_id: &str) -> Result<String, Error> {
    run(agent_id).await.map_err(tool_err)
}

async fn run(agent_id: &str) -> Result<String> {
    let config = Config::load().await?;
    let timeout = Duration::from_secs(config.limits.wait_timeout_secs);
    let start = Instant::now();
    let target_task_id = runtime_task_id();

    loop {
        let context = match target_task_id.as_deref() {
            Some(task_id) => project::attach_running_run_to_consultation(task_id, agent_id).await?,
            None => project::attach_running_run_to_next_consultation(agent_id).await?,
        };
        if let Some(context) = context {
            let task = store::read_task_at(&context.task_path).await?;
            let consult_request = store::read_consult_request_for_run_dir(&context.run_dir).await?;
            let review = store::read_review_for_run_dir(&context.run_dir)
                .await
                .unwrap_or_default();

            info!(
                agent_id,
                task_id = context.task_id,
                "Supervisor attached to consultation"
            );
            let response = json!({
                "status": "claimed",
                "task_id": context.task_id,
                "task_path": context.task_path,
                "run_dir": context.run_dir,
                "state": "Consultation",
                "paused_state": context.paused_status,
                "task": task,
                "consult_request": consult_request,
                "review": review,
                "review_cycles_used": context.review_cycles,
                "check_retries_used": context.check_retries,
            });
            return Ok(response.to_string());
        }

        if start.elapsed() >= timeout {
            info!("wait_for_consultation timed out");
            let response = json!({
                "status": "timeout",
                "state": "NoConsultation",
            });
            return Ok(response.to_string());
        }

        sleep(Duration::from_millis(500)).await;
    }
}

fn runtime_task_id() -> Option<String> {
    std::env::var(ENV_TASK_ID)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
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
    async fn wait_for_consultation_attaches_supervisor_run_without_stealing_executor_lease() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        tokio::fs::write(".ferrus/tasks/t-007.md", "task body")
            .await
            .unwrap();
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
        crate::project::record_task_consultation_requested(
            "t-007",
            crate::project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        store::write_consult_request_for_run_dir(".ferrus/runs/t-007", "consult me")
            .await
            .unwrap();
        crate::project::record_run_started("supervisor", "supervisor:codex:1", std::process::id())
            .await
            .unwrap();

        let response = run("supervisor:codex:1").await.unwrap();
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();

        assert_eq!(response["status"], "claimed");
        assert_eq!(response["task_id"], "t-007");
        assert_eq!(response["consult_request"], "consult me");
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "consultation");
        assert_eq!(task.claimed_by.as_deref(), Some("executor:codex:7"));
        let runs = crate::project::list_runs(10).await.unwrap();
        let run = runs
            .iter()
            .find(|run| run.agent == "supervisor:codex:1")
            .unwrap();
        assert_eq!(run.task_id, "t-007");

        teardown(previous);
    }
}
