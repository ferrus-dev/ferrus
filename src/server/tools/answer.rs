use anyhow::Result;
use neva::prelude::*;

use crate::{project, state::store};

use super::tool_err;

pub const DESCRIPTION: &str = "Provide a response to a pending human question when the state is AwaitingHuman. \
     Writes the response to ANSWER.md and restores the previous state so the agent can continue.";

pub const INPUT_SCHEMA: &str = r#"{
    "type": "object",
    "properties": {
        "response": {
            "type": "string",
            "description": "The response to the question written in QUESTION.md"
        }
    },
    "required": ["response"]
}"#;

pub async fn handler(response: String) -> Result<String, Error> {
    run(response).await.map_err(tool_err)
}

async fn run(response: String) -> Result<String> {
    if let Some(question) = project::list_human_questions().await?.into_iter().next() {
        store::write_answer_for_run_dir(&question.run_dir, &response).await?;
        project::record_runtime_event_best_effort(
            None,
            "human_answer_recorded",
            serde_json::json!({
                "task_id": question.task_id,
                "run_dir": question.run_dir,
                "answer_bytes": response.len(),
            }),
        )
        .await;
        return Ok(format!(
            "Response recorded for `{}` in `{}/ANSWER.md`. The waiting agent can call /wait_for_answer and continue.",
            question.task_id, question.run_dir
        ));
    }

    anyhow::bail!("No task is currently waiting for a human answer.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn setup() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::fs::create_dir_all(dir.path().join(".ferrus/runs/t-007")).unwrap();
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
    async fn answer_writes_first_scoped_human_answer_without_state_json() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        crate::project::record_task_status(
            "t-007",
            ".ferrus/tasks/t-007.md",
            crate::project::TaskStatus::Addressing,
        )
        .await
        .unwrap();
        crate::project::record_task_human_question_requested(
            "t-007",
            crate::project::TaskStatus::Addressing,
            "executor:codex:7",
        )
        .await
        .unwrap();
        store::write_question_for_run_dir(".ferrus/runs/t-007", "Which path?")
            .await
            .unwrap();

        let output = run("Use the stable path.".to_string()).await.unwrap();

        assert!(output.contains("Response recorded for `t-007`"));
        assert_eq!(
            store::read_answer_for_run_dir(".ferrus/runs/t-007")
                .await
                .unwrap(),
            "Use the stable path."
        );
        crate::test_support::assert_no_state_json();
        teardown(previous);
    }
}
