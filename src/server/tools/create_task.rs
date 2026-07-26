use anyhow::Result;
use neva::prelude::*;
use tracing::info;

use crate::project;

use super::tool_err;

pub const DESCRIPTION: &str = "Compatibility task creation tool. Prefer /enqueue_task. Writes \
     the task description to .ferrus/tasks/<task-id>.md and records a pending SQLite task row.";

pub const INPUT_SCHEMA: &str = r#"{
    "type": "object",
    "properties": {
        "description": {
            "type": "string",
            "description": "Full task description in Markdown"
        }
    },
    "required": ["description"]
}"#;

pub async fn handler(description: String) -> Result<String, Error> {
    run(description).await.map_err(tool_err)
}

async fn run(description: String) -> Result<String> {
    if description.trim().is_empty() {
        anyhow::bail!("Cannot create task: description is empty.");
    }

    let artifact = project::create_pending_task_artifact(&description, None, None).await?;
    project::record_runtime_event_best_effort(
        None,
        "task_created",
        serde_json::json!({
            "task_id": artifact.id,
            "path": artifact.path,
            "run_dir": artifact.run_dir,
            "spec_path": null,
            "milestone_id": null,
            "description_bytes": description.len(),
        }),
    )
    .await;

    info!(
        task_id = artifact.id,
        "Task created through compatibility tool, DB task -> pending"
    );
    Ok(format!(
        "Task {} created. State: pending. Artifact: {}",
        artifact.id, artifact.path
    ))
}

#[cfg(test)]
mod tests {
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
    async fn create_task_writes_numbered_task_artifact_without_rewriting_template() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        tokio::fs::write(".ferrus/TASK.md", "task template")
            .await
            .unwrap();

        run("Build the thing".to_string()).await.unwrap();

        crate::test_support::assert_no_state_json();
        assert_eq!(
            tokio::fs::read_to_string(".ferrus/tasks/t-001.md")
                .await
                .unwrap(),
            "Build the thing"
        );
        assert_eq!(
            tokio::fs::read_to_string(".ferrus/TASK.md").await.unwrap(),
            "task template"
        );
        let tasks = project::list_tasks().await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "t-001");
        assert_eq!(tasks[0].path, ".ferrus/tasks/t-001.md");
        assert_eq!(tasks[0].status, "pending");

        teardown(previous);
    }

    #[tokio::test]
    async fn concurrent_task_creation_reserves_distinct_ids() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;

        let (first, second) = tokio::join!(
            run("First task".to_string()),
            run("Second task".to_string())
        );

        assert!(first.is_ok(), "first create failed: {first:?}");
        assert!(second.is_ok(), "second create failed: {second:?}");
        let tasks = project::list_tasks().await.unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].id, "t-001");
        assert_eq!(tasks[1].id, "t-002");
        assert_eq!(tasks[0].status, "pending");
        assert_eq!(tasks[1].status, "pending");
        let contents = [
            tokio::fs::read_to_string(&tasks[0].path).await.unwrap(),
            tokio::fs::read_to_string(&tasks[1].path).await.unwrap(),
        ];
        assert!(contents.contains(&"First task".to_string()));
        assert!(contents.contains(&"Second task".to_string()));

        teardown(previous);
    }
}
