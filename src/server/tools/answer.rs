//! Record a human answer for a paused task so its waiting agent can resume.

use anyhow::Result;
use neva::prelude::*;

use crate::project;

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
    if response.trim().is_empty() {
        anyhow::bail!("Human answer cannot be empty.");
    }

    if let Some(question) = project::list_human_questions().await?.into_iter().next() {
        project::record_scoped_human_answer(&question, &response).await?;
        return Ok(format!(
            "Response recorded for `{}` in `{}/ANSWER.md`. The waiting agent can call /wait_for_answer and continue.",
            question.task_id, question.run_dir
        ));
    }

    anyhow::bail!("No task is currently waiting for a human answer.")
}

#[cfg(test)]
mod tests {
    //! Scoped human answers preserve pending questions on invalid input.

    use super::*;
    use crate::state::store;
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
        assert!(
            crate::project::list_human_questions()
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            crate::project::list_answered_human_waiters().await.unwrap(),
            [crate::project::AnsweredHumanWaiter {
                task_id: "t-007".to_string(),
                awaiting_human_by: "executor:codex:7".to_string(),
            }]
        );
        crate::test_support::assert_no_state_json();
        teardown(previous);
    }

    #[tokio::test]
    async fn answer_rejects_whitespace_without_hiding_the_question() {
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

        let error = run(" \n\t ".to_string()).await.unwrap_err().to_string();

        assert_eq!(error, "Human answer cannot be empty.");
        assert!(!std::path::Path::new(".ferrus/runs/t-007/ANSWER.md").exists());
        let questions = crate::project::list_human_questions().await.unwrap();
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].task_id, "t-007");
        assert!(
            crate::project::list_answered_human_waiters()
                .await
                .unwrap()
                .is_empty()
        );

        teardown(previous);
    }
}
