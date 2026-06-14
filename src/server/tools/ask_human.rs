use anyhow::Result;
use tracing::info;

use crate::{
    config::Config,
    project::{self, RuntimeTaskContext},
    state::store,
};

use super::{ensure_lease_owner_or_reclaim, require_runtime_task_context, tool_err};

pub const DESCRIPTION: &str = "Ask the human a question. \
     Writes the question to QUESTION.md, transitions state to AwaitingHuman, \
     and returns immediately. You MUST call /wait_for_answer immediately after \
     to block until the human responds — do not call any other tools in between. \
     Can be called from Executing, Addressing, Consultation, or Reviewing state.";

pub const INPUT_SCHEMA: &str = r#"{
    "type": "object",
    "properties": {
        "question": {
            "type": "string",
            "description": "The question to ask the human"
        }
    },
    "required": ["question"]
}"#;

pub async fn handler(ctx: neva::Context, question: String) -> Result<String, neva::prelude::Error> {
    let agent_id = super::agent_id_from_context(&ctx)?;
    handler_for_agent(&agent_id, question).await
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
    ensure_can_ask_human(&context, agent_id, config.lease.ttl_secs).await?;

    write_question(&context, &question).await?;
    clear_answer(&context).await?;

    let (resume_status, paused_status) = human_pause_context(&context)?;
    project::record_task_human_question_requested_with_resume(
        &context.task_id,
        resume_status,
        paused_status,
        agent_id,
    )
    .await?;
    let paused = context.status.clone();

    info!(paused, "Task → AwaitingHuman");
    Ok(format!(
        "Your question has been written to `.ferrus/QUESTION.md`.\n\
         State is now AwaitingHuman (paused from {paused}).\n\
         Call /wait_for_answer immediately to block until the human responds.\n\
         Do NOT call any other tools while waiting."
    ))
}

async fn ensure_can_ask_human(
    context: &RuntimeTaskContext,
    agent_id: &str,
    ttl_secs: u64,
) -> Result<()> {
    if !can_ask_from_context(context) {
        anyhow::bail!(
            "Cannot ask human from state {}. /ask_human is only available while active work is in progress.",
            context.status
        );
    }
    if can_supervisor_ask_during_consultation(context, agent_id) {
        return Ok(());
    }
    ensure_lease_owner_or_reclaim(agent_id, ttl_secs).await
}

fn can_ask_from_context(context: &RuntimeTaskContext) -> bool {
    matches!(
        context.status.parse::<project::TaskStatus>().ok(),
        Some(
            project::TaskStatus::Executing
                | project::TaskStatus::Addressing
                | project::TaskStatus::Consultation
                | project::TaskStatus::Reviewing
        )
    )
}

fn can_supervisor_ask_during_consultation(context: &RuntimeTaskContext, agent_id: &str) -> bool {
    if !is_supervisor(agent_id) {
        return false;
    }
    context.status.parse::<project::TaskStatus>().ok() == Some(project::TaskStatus::Consultation)
}

fn human_pause_context(
    context: &RuntimeTaskContext,
) -> Result<(project::TaskStatus, Option<project::TaskStatus>)> {
    let current_status = context.status.parse::<project::TaskStatus>()?;
    if current_status == project::TaskStatus::Consultation {
        let paused_status = context
            .paused_status
            .as_deref()
            .map(str::parse::<project::TaskStatus>)
            .transpose()?;
        return Ok((project::TaskStatus::Consultation, paused_status));
    }
    Ok((current_status, Some(current_status)))
}

async fn write_question(context: &RuntimeTaskContext, question: &str) -> Result<()> {
    store::write_question_for_run_dir(&context.run_dir, question).await
}

async fn clear_answer(context: &RuntimeTaskContext) -> Result<()> {
    store::clear_answer_for_run_dir(&context.run_dir).await
}

fn is_supervisor(agent_id: &str) -> bool {
    agent_id.starts_with("supervisor:")
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
    async fn ask_human_for_scoped_runtime_task_writes_scoped_question() {
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

        run("executor:codex:7", "Which path should I take?".to_string())
            .await
            .unwrap();

        crate::test_support::assert_no_state_json();
        assert_eq!(
            store::read_question_for_run_dir(".ferrus/runs/t-007")
                .await
                .unwrap(),
            "Which path should I take?"
        );
        assert_eq!(
            tokio::fs::read_to_string(".ferrus/QUESTION.md")
                .await
                .unwrap_or_default(),
            ""
        );
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "awaiting_human");
        assert_eq!(task.paused_status.as_deref(), Some("executing"));
        assert_eq!(task.claimed_by.as_deref(), Some("executor:codex:7"));

        teardown(previous);
    }

    #[tokio::test]
    async fn ask_human_uses_database_context_when_state_json_is_absent() {
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

        run("executor:codex:7", "Which path should I take?".to_string())
            .await
            .unwrap();

        crate::test_support::assert_no_state_json();
        assert_eq!(
            store::read_question_for_run_dir(".ferrus/runs/t-007")
                .await
                .unwrap(),
            "Which path should I take?"
        );
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "awaiting_human");
        assert_eq!(task.paused_status.as_deref(), Some("executing"));
        assert_eq!(
            crate::project::task_human_question_owner("t-007")
                .await
                .unwrap()
                .as_deref(),
            Some("executor:codex:7")
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn ask_human_records_paused_status_in_database() {
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

        run("executor:codex:1", "Which path should I take?".to_string())
            .await
            .unwrap();

        crate::test_support::assert_no_state_json();
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-001").unwrap();
        assert_eq!(task.status, "awaiting_human");
        assert_eq!(task.paused_status.as_deref(), Some("addressing"));
        assert_eq!(task.claimed_by.as_deref(), Some("executor:codex:1"));

        teardown(previous);
    }

    #[tokio::test]
    async fn ask_human_during_consultation_preserves_pre_consult_paused_status() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        crate::project::record_task_status(
            "t-002",
            ".ferrus/tasks/t-002.md",
            crate::project::TaskStatus::Addressing,
        )
        .await
        .unwrap();
        crate::project::claim_task("t-002", ".ferrus/tasks/t-002.md", "executor:codex:2", 60)
            .await
            .unwrap();
        crate::project::record_task_consultation_requested(
            "t-002",
            crate::project::TaskStatus::Addressing,
        )
        .await
        .unwrap();

        run("executor:codex:2", "Can I proceed?".to_string())
            .await
            .unwrap();

        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-002").unwrap();
        assert_eq!(task.status, "awaiting_human");
        assert_eq!(task.paused_status.as_deref(), Some("addressing"));
        let restored = crate::project::restore_task_from_human_answer("t-002")
            .await
            .unwrap();
        assert!(matches!(
            restored,
            crate::project::TaskHumanAnswerRestore::Restored { ref status }
                if status == "consultation"
        ));
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-002").unwrap();
        assert_eq!(task.status, "consultation");
        assert_eq!(task.paused_status.as_deref(), Some("addressing"));

        let restored = crate::project::restore_task_from_consultation("t-002")
            .await
            .unwrap();
        assert!(matches!(
            restored,
            crate::project::TaskConsultRestore::Restored { ref status }
                if status == "addressing"
        ));

        teardown(previous);
    }
}
