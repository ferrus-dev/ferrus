use anyhow::Result;
use neva::prelude::*;
use serde_json::json;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::info;

use crate::{
    config::Config,
    project::{self, RuntimeTaskContext, TaskHumanAnswerRestore},
    state::store,
};

use super::{ensure_lease_owner_or_reclaim, require_runtime_task_context, tool_err};

pub const DESCRIPTION: &str = "Block until the human provides an answer to the question you asked via /ask_human. \
     Polls the task-scoped ANSWER.md until it has content, then restores the paused state and \
     returns the answer. \
     Returns {\"status\":\"answered\", \"answer\":\"...\", \"resumed_state\":\"...\"} on success, \
     or {\"status\":\"timeout\"} if `wait_timeout_secs` elapses for this call. On timeout, call this tool again \
     to keep waiting. \
     Must only be called immediately after /ask_human while state is AwaitingHuman.";

pub async fn handler(ctx: neva::di::Dc<crate::server::ServerContext>) -> Result<String, Error> {
    handler_for_agent(ctx.agent_id()).await
}

pub async fn handler_for_agent(agent_id: &str) -> Result<String, Error> {
    run(agent_id).await.map_err(tool_err)
}

async fn run(agent_id: &str) -> Result<String> {
    let config = Config::load().await?;
    let context = require_runtime_task_context(agent_id).await?;
    if context.status.parse::<project::TaskStatus>()? != project::TaskStatus::AwaitingHuman {
        anyhow::bail!(
            "Cannot wait for answer from task status {:?}; expected awaiting_human",
            context.status
        );
    }
    ensure_scoped_answer_waiter(&context, agent_id, config.lease.ttl_secs).await?;

    let timeout = Duration::from_secs(config.limits.wait_timeout_secs);
    let start = Instant::now();

    loop {
        match read_answer(&context).await {
            Ok(ans) if !ans.trim().is_empty() => {
                let restored = project::restore_task_from_human_answer(&context.task_id).await?;
                let resumed = match restored {
                    TaskHumanAnswerRestore::Restored { status } => status,
                    TaskHumanAnswerRestore::NotAwaitingHuman => context.status.clone(),
                };
                store::clear_answer_for_run_dir(&context.run_dir).await?;
                store::clear_question_for_run_dir(&context.run_dir).await?;

                let answer = ans.trim().to_string();
                info!(resumed, "Human answered; task restored");
                let response = json!({
                    "status": "answered",
                    "answer": answer,
                    "resumed_state": resumed,
                });
                return Ok(response.to_string());
            }
            _ => {}
        }

        if start.elapsed() >= timeout {
            info!("wait_for_answer timed out");
            let response = json!({"status": "timeout"});
            return Ok(response.to_string());
        }

        sleep(Duration::from_secs(2)).await;
    }
}

async fn ensure_scoped_answer_waiter(
    context: &RuntimeTaskContext,
    agent_id: &str,
    ttl_secs: u64,
) -> Result<()> {
    if let Some(owner) = project::task_human_question_owner(&context.task_id).await? {
        if owner == agent_id {
            if is_consultation_supervisor_human_wait(context, agent_id).await? {
                return Ok(());
            }
            return ensure_lease_owner_or_reclaim(agent_id, ttl_secs).await;
        }
        anyhow::bail!("Cannot wait for answer: question was asked by {owner}, not {agent_id}");
    }
    ensure_lease_owner_or_reclaim(agent_id, ttl_secs).await
}

async fn is_consultation_supervisor_human_wait(
    context: &RuntimeTaskContext,
    agent_id: &str,
) -> Result<bool> {
    if !agent_id.starts_with("supervisor:") {
        return Ok(false);
    }
    Ok(project::task_awaiting_human_status(&context.task_id)
        .await?
        .as_deref()
        == Some(project::TaskStatus::Consultation.as_str()))
}

async fn read_answer(context: &RuntimeTaskContext) -> Result<String> {
    store::read_answer_for_run_dir(&context.run_dir).await
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
    async fn wait_for_answer_restores_scoped_runtime_task() {
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
        crate::project::record_task_human_question_requested(
            "t-007",
            crate::project::TaskStatus::Executing,
            "executor:codex:7",
        )
        .await
        .unwrap();
        store::write_answer_for_run_dir(".ferrus/runs/t-007", "Use the stable path.")
            .await
            .unwrap();

        let response = run("executor:codex:7").await.unwrap();
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();

        assert_eq!(response["status"], "answered");
        assert_eq!(response["answer"], "Use the stable path.");
        assert_eq!(response["resumed_state"], "executing");
        crate::test_support::assert_no_state_json();
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "executing");
        assert_eq!(task.paused_status, None);
        assert_eq!(
            store::read_answer_for_run_dir(".ferrus/runs/t-007")
                .await
                .unwrap(),
            ""
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn wait_for_answer_uses_database_context_when_state_json_is_absent() {
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
        crate::project::record_task_human_question_requested(
            "t-007",
            crate::project::TaskStatus::Executing,
            "executor:codex:7",
        )
        .await
        .unwrap();
        store::write_answer_for_run_dir(".ferrus/runs/t-007", "Use the stable path.")
            .await
            .unwrap();

        let response = run("executor:codex:7").await.unwrap();
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();

        assert_eq!(response["status"], "answered");
        assert_eq!(response["answer"], "Use the stable path.");
        assert_eq!(response["resumed_state"], "executing");
        crate::test_support::assert_no_state_json();
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "executing");
        assert_eq!(task.paused_status, None);
        assert_eq!(
            crate::project::task_human_question_owner("t-007")
                .await
                .unwrap(),
            None
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn wait_for_answer_renews_question_owner_lease_on_timeout() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        crate::project::record_task_status(
            "t-008",
            ".ferrus/tasks/t-008.md",
            crate::project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        crate::project::claim_task("t-008", ".ferrus/tasks/t-008.md", "executor:codex:8", 2)
            .await
            .unwrap();
        crate::project::record_task_human_question_requested(
            "t-008",
            crate::project::TaskStatus::Executing,
            "executor:codex:8",
        )
        .await
        .unwrap();
        let original_lease = crate::project::list_tasks()
            .await
            .unwrap()
            .into_iter()
            .find(|task| task.id == "t-008")
            .and_then(|task| task.lease_until)
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

        let response = run("executor:codex:8").await.unwrap();
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();

        assert_eq!(response["status"], "timeout");
        let task = crate::project::list_tasks()
            .await
            .unwrap()
            .into_iter()
            .find(|task| task.id == "t-008")
            .unwrap();
        assert_eq!(task.status, "awaiting_human");
        assert_eq!(task.claimed_by.as_deref(), Some("executor:codex:8"));
        assert!(task.lease_until.unwrap() > original_lease);

        teardown(previous);
    }

    #[tokio::test]
    async fn wait_for_answer_restores_database_paused_status() {
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
        crate::project::record_task_human_question_requested(
            "t-001",
            crate::project::TaskStatus::Addressing,
            "executor:codex:1",
        )
        .await
        .unwrap();
        store::write_answer_for_run_dir(".ferrus/runs/t-001", "Use the stable path.")
            .await
            .unwrap();

        let response = run("executor:codex:1").await.unwrap();
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();

        assert_eq!(response["status"], "answered");
        assert_eq!(response["answer"], "Use the stable path.");
        assert_eq!(response["resumed_state"], "addressing");
        crate::test_support::assert_no_state_json();
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-001").unwrap();
        assert_eq!(task.status, "addressing");
        assert_eq!(task.paused_status, None);
        assert_eq!(
            store::read_answer_for_run_dir(".ferrus/runs/t-001")
                .await
                .unwrap(),
            ""
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn wait_for_answer_rejects_scoped_agent_that_did_not_ask_question() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        crate::project::record_task_status(
            "t-007",
            ".ferrus/tasks/t-007.md",
            crate::project::TaskStatus::Consultation,
        )
        .await
        .unwrap();
        crate::project::record_run_started("supervisor", "supervisor:codex:7", std::process::id())
            .await
            .unwrap();
        crate::project::attach_running_run_to_task(
            "supervisor:codex:7",
            "t-007",
            ".ferrus/tasks/t-007.md",
        )
        .await
        .unwrap();
        crate::project::record_task_human_question_requested(
            "t-007",
            crate::project::TaskStatus::Consultation,
            "supervisor:codex:1",
        )
        .await
        .unwrap();
        store::write_answer_for_run_dir(".ferrus/runs/t-007", "Use the stable path.")
            .await
            .unwrap();

        let error = run("supervisor:codex:7").await.unwrap_err().to_string();

        assert!(error.contains("question was asked by supervisor:codex:1"));
        assert_eq!(
            store::read_answer_for_run_dir(".ferrus/runs/t-007")
                .await
                .unwrap(),
            "Use the stable path."
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn wait_for_answer_allows_consultation_supervisor_question_owner_without_task_lease() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        crate::project::record_task_status(
            "t-009",
            ".ferrus/tasks/t-009.md",
            crate::project::TaskStatus::Consultation,
        )
        .await
        .unwrap();
        crate::project::claim_task("t-009", ".ferrus/tasks/t-009.md", "executor:codex:9", 60)
            .await
            .unwrap();
        crate::project::record_run_started("supervisor", "supervisor:codex:9", std::process::id())
            .await
            .unwrap();
        crate::project::attach_running_run_to_task(
            "supervisor:codex:9",
            "t-009",
            ".ferrus/tasks/t-009.md",
        )
        .await
        .unwrap();
        crate::project::record_task_human_question_requested_with_resume(
            "t-009",
            crate::project::TaskStatus::Consultation,
            Some(crate::project::TaskStatus::Addressing),
            "supervisor:codex:9",
        )
        .await
        .unwrap();
        store::write_answer_for_run_dir(".ferrus/runs/t-009", "Use the narrow option.")
            .await
            .unwrap();

        let response = run("supervisor:codex:9").await.unwrap();
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();

        assert_eq!(response["status"], "answered");
        assert_eq!(response["resumed_state"], "consultation");
        let task = crate::project::list_tasks()
            .await
            .unwrap()
            .into_iter()
            .find(|task| task.id == "t-009")
            .unwrap();
        assert_eq!(task.status, "consultation");
        assert_eq!(task.paused_status.as_deref(), Some("addressing"));
        assert_eq!(task.claimed_by.as_deref(), Some("executor:codex:9"));

        teardown(previous);
    }
}
