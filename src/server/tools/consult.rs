//! Pause Executor work and write a scoped consultation request for the Supervisor.

use anyhow::Result;
use tracing::info;

use crate::{
    config::Config,
    project::{self, RuntimeTaskContext},
    state::store,
};

use super::{ensure_lease_owner_or_reclaim, require_runtime_task_context, tool_err};

pub const DESCRIPTION: &str = "Ask the configured Supervisor for a consultation. \
     Writes CONSULT_REQUEST.md, transitions state to Consultation, clears any stale \
     CONSULT_RESPONSE.md, and returns immediately. HQ will spawn the consultant Supervisor. \
     After calling this tool, call /wait_for_consult to block until the answer is ready.";

pub const INPUT_SCHEMA: &str = r#"{
    "type": "object",
    "properties": {
        "question": {
            "type": "string",
            "description": "The executor's consultation request for the supervisor"
        }
    },
    "required": ["question"]
}"#;

pub async fn handler(
    ctx: neva::di::Dc<crate::server::ServerContext>,
    question: String,
) -> Result<String, neva::prelude::Error> {
    handler_for_agent(ctx.agent_id(), question).await
}

pub async fn handler_for_agent(
    agent_id: &str,
    question: String,
) -> Result<String, neva::prelude::Error> {
    run(agent_id, question).await.map_err(tool_err)
}

async fn run(agent_id: &str, question: String) -> Result<String> {
    let config = Config::load().await?;
    let context = require_runtime_task_context(agent_id).await?;
    let current_status = context.status.parse::<project::TaskStatus>()?;
    if !matches!(
        current_status,
        project::TaskStatus::Executing | project::TaskStatus::Addressing
    ) {
        anyhow::bail!(
            "Cannot consult from state {}. Consultation is only available while executing work.",
            context.status
        );
    }
    ensure_lease_owner_or_reclaim(agent_id, config.lease.ttl_secs).await?;

    validate_consult_request(&question)?;

    write_consult_request(&context, &question).await?;
    clear_consult_response(&context).await?;
    project::record_task_consultation_requested(&context.task_id, current_status).await?;
    let paused = context.status.clone();

    info!(paused, "Task -> Consultation");
    Ok(format!(
        "Consultation requested in `{}/CONSULT_REQUEST.md`.\n\
         State is now Consultation (paused from {paused}).\n\
         HQ should spawn the configured Supervisor in consultation mode.\n\
         Call /wait_for_consult to block until the response is ready.",
        context.run_dir,
    ))
}

async fn write_consult_request(context: &RuntimeTaskContext, question: &str) -> Result<()> {
    store::write_consult_request_for_run_dir(&context.run_dir, question).await
}

async fn clear_consult_response(context: &RuntimeTaskContext) -> Result<()> {
    store::clear_consult_response_for_run_dir(&context.run_dir).await
}

fn validate_consult_request(question: &str) -> Result<()> {
    let trimmed = question.trim();
    if trimmed.is_empty() {
        anyhow::bail!(
            "Consultation request cannot be empty. Read ferrus://consult_template and follow it exactly."
        );
    }

    let required_sections = [
        "## Problem",
        "## What I tried",
        "## Options (if any)",
        "## Question",
    ];

    for section in required_sections {
        if !trimmed.contains(section) {
            anyhow::bail!(
                "Consultation request must follow ferrus://consult_template exactly. Missing section: {section}"
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    //! Consultation request validation and scoped task transitions.

    use super::*;
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

    #[test]
    fn consult_request_requires_template_sections() {
        let err = validate_consult_request("Implementation complete, what now?")
            .expect_err("request without template should be rejected");
        let msg = err.to_string();
        assert!(msg.contains("ferrus://consult_template"));
        assert!(msg.contains("## Problem"));
    }

    #[test]
    fn consult_request_accepts_template_shape() {
        let request = "## Problem\n/check appears unavailable.\n\n## What I tried\nRetried once.\n\n## Options (if any)\n- Retry again\n\n## Question\nShould I keep retrying /check?\n";
        validate_consult_request(request).expect("template-shaped request should be accepted");
    }

    #[tokio::test]
    async fn consult_moves_agent_runtime_task_to_scoped_consultation() {
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
        let request = "## Problem\nNeed design input.\n\n## What I tried\nCompared options.\n\n## Options (if any)\n- A\n\n## Question\nWhich option?\n";

        let response = run("executor:codex:7", request.to_string()).await.unwrap();

        assert!(response.contains(".ferrus/runs/t-007/CONSULT_REQUEST.md"));
        assert_eq!(
            tokio::fs::read_to_string(".ferrus/runs/t-007/CONSULT_REQUEST.md")
                .await
                .unwrap(),
            request
        );
        crate::test_support::assert_no_state_json();
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "consultation");
        assert_eq!(task.paused_status.as_deref(), Some("executing"));
        assert_eq!(task.claimed_by.as_deref(), Some("executor:codex:7"));

        teardown(previous);
    }

    #[tokio::test]
    async fn consult_uses_database_context_when_state_json_is_absent() {
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
        let request = "## Problem\nNeed design input.\n\n## What I tried\nCompared options.\n\n## Options (if any)\n- A\n\n## Question\nWhich option?\n";

        run("executor:codex:7", request.to_string()).await.unwrap();

        crate::test_support::assert_no_state_json();
        assert_eq!(
            tokio::fs::read_to_string(".ferrus/runs/t-007/CONSULT_REQUEST.md")
                .await
                .unwrap(),
            request
        );
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "consultation");
        assert_eq!(task.paused_status.as_deref(), Some("executing"));
        assert_eq!(task.claimed_by.as_deref(), Some("executor:codex:7"));

        teardown(previous);
    }

    #[tokio::test]
    async fn consult_records_paused_status_in_database() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        crate::project::record_task_status(
            "t-001",
            ".ferrus/tasks/t-001.md",
            crate::project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        crate::project::claim_task("t-001", ".ferrus/tasks/t-001.md", "executor:codex:1", 60)
            .await
            .unwrap();
        let request = "## Problem\nNeed design input.\n\n## What I tried\nCompared options.\n\n## Options (if any)\n- A\n\n## Question\nWhich option?\n";

        run("executor:codex:1", request.to_string()).await.unwrap();

        crate::test_support::assert_no_state_json();
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-001").unwrap();
        assert_eq!(task.status, "consultation");
        assert_eq!(task.paused_status.as_deref(), Some("executing"));
        assert_eq!(task.claimed_by.as_deref(), Some("executor:codex:1"));

        teardown(previous);
    }
}
