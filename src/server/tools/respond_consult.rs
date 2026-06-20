use anyhow::Result;
use neva::prelude::*;
use tracing::info;

use crate::{
    project::{self, RuntimeTaskContext},
    state::store,
};

use super::{runtime_task_context_for_agent_best_effort, tool_err};

pub const DESCRIPTION: &str = "Record the Supervisor's consultation response. \
     Writes the response to CONSULT_RESPONSE.md. Must only be called while state is Consultation.";

pub const INPUT_SCHEMA: &str = r#"{
    "type": "object",
    "properties": {
        "response": {
            "type": "string",
            "description": "The Supervisor's consultation response"
        }
    },
    "required": ["response"]
}"#;

pub async fn handler(ctx: neva::Context, response: String) -> Result<String, Error> {
    let agent_id = super::agent_id_from_context(&ctx)?;
    handler_for_agent(&agent_id, response).await
}

pub async fn handler_for_agent(agent_id: &str, response: String) -> Result<String, Error> {
    run(Some(agent_id), response).await.map_err(tool_err)
}

async fn run(agent_id: Option<&str>, response: String) -> Result<String> {
    if response.trim().is_empty() {
        anyhow::bail!("Consultation response cannot be empty.");
    }

    let runtime_context = match agent_id {
        Some(agent_id) => match runtime_task_context_for_agent_best_effort(agent_id).await {
            Some(context) => Some(context),
            None => match project::attach_running_run_to_next_consultation(agent_id).await {
                Ok(context) => context,
                Err(err) => {
                    tracing::warn!(
                        error = ?err,
                        agent_id,
                        "failed to attach supervisor run to consultation"
                    );
                    None
                }
            },
        },
        None => None,
    };

    let Some(context) = runtime_context else {
        anyhow::bail!("Cannot respond to consultation without a SQLite runtime task context.");
    };
    if context.status.parse::<project::TaskStatus>()? != project::TaskStatus::Consultation {
        anyhow::bail!(
            "Cannot respond to consultation from state {}. /respond_consult is only valid in Consultation state.",
            context.status
        );
    }

    write_consult_response(&context, &response).await?;
    info!("Consultation response recorded");
    Ok("Consultation response recorded in `.ferrus/CONSULT_RESPONSE.md`.".to_string())
}

async fn write_consult_response(context: &RuntimeTaskContext, response: &str) -> Result<()> {
    store::write_consult_response_for_run_dir(&context.run_dir, response).await
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
    async fn respond_consult_writes_scoped_response_for_attached_consultation_run() {
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
        crate::project::record_task_consultation_requested(
            "t-007",
            crate::project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        crate::project::record_run_started("supervisor", "supervisor:codex:1", std::process::id())
            .await
            .unwrap();
        crate::project::attach_running_run_to_next_consultation("supervisor:codex:1")
            .await
            .unwrap();

        run(Some("supervisor:codex:1"), "Use option A.".to_string())
            .await
            .unwrap();

        assert_eq!(
            store::read_consult_response_for_run_dir(".ferrus/runs/t-007")
                .await
                .unwrap(),
            "Use option A."
        );
        assert_eq!(
            tokio::fs::read_to_string(".ferrus/CONSULT_RESPONSE.md")
                .await
                .unwrap_or_default(),
            ""
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn respond_consult_uses_database_context_when_state_json_is_absent() {
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
        crate::project::record_task_consultation_requested(
            "t-007",
            crate::project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        crate::project::record_run_started("supervisor", "supervisor:codex:1", std::process::id())
            .await
            .unwrap();
        crate::project::attach_running_run_to_next_consultation("supervisor:codex:1")
            .await
            .unwrap();

        run(Some("supervisor:codex:1"), "Use option A.".to_string())
            .await
            .unwrap();

        crate::test_support::assert_no_state_json();
        assert_eq!(
            store::read_consult_response_for_run_dir(".ferrus/runs/t-007")
                .await
                .unwrap(),
            "Use option A."
        );

        teardown(previous);
    }

    #[tokio::test]
    async fn respond_consult_does_not_reassign_an_existing_non_consultation_run() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        let agent_id = "supervisor:codex:t-001";

        crate::project::record_task_status(
            "t-001",
            ".ferrus/tasks/t-001.md",
            crate::project::TaskStatus::Reviewing,
        )
        .await
        .unwrap();
        crate::project::claim_review_task_by_id("t-001", agent_id, 60)
            .await
            .unwrap();
        crate::project::record_run_started_for_task_with_workspace(
            "run-supervisor-t-001",
            "supervisor",
            agent_id,
            std::process::id(),
            Some("t-001"),
            std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        )
        .await
        .unwrap();

        crate::project::record_task_status(
            "t-002",
            ".ferrus/tasks/t-002.md",
            crate::project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        crate::project::record_task_consultation_requested(
            "t-002",
            crate::project::TaskStatus::Executing,
        )
        .await
        .unwrap();

        let error = run(Some(agent_id), "Accidental response".to_string())
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("only valid in Consultation state"));
        let run = crate::project::list_runs(10)
            .await
            .unwrap()
            .into_iter()
            .find(|run| run.id == "run-supervisor-t-001")
            .unwrap();
        assert_eq!(run.task_id, "t-001");
        assert!(
            store::read_consult_response_for_run_dir(".ferrus/runs/t-002")
                .await
                .unwrap_or_default()
                .is_empty()
        );

        teardown(previous);
    }
}
