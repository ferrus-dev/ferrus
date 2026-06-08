use anyhow::{Context, Result};
use neva::prelude::*;
use neva::types::CallToolRequestParams;
use tracing::info;

use crate::project;

use super::tool_err;

pub const DESCRIPTION: &str = "Enqueue an approved task artifact for later execution. Writes \
     .ferrus/tasks/<task-id>.md and records a pending SQLite task row.";

pub const INPUT_SCHEMA: &str = r#"{
    "type": "object",
    "properties": {
        "description": {
            "type": "string",
            "description": "Full approved task description in Markdown"
        },
        "spec_path": {
            "type": "string",
            "description": "Optional spec path that originated this task"
        },
        "milestone_id": {
            "type": "string",
            "description": "Optional milestone ID that originated this task"
        }
    },
    "required": ["description"]
}"#;

pub async fn handler(params: CallToolRequestParams) -> Result<String, Error> {
    let (description, spec_path, milestone_id) = parse_input(params).map_err(tool_err)?;
    run(description, spec_path, milestone_id)
        .await
        .map_err(tool_err)
}

async fn run(
    description: String,
    spec_path: Option<String>,
    milestone_id: Option<String>,
) -> Result<String> {
    if description.trim().is_empty() {
        anyhow::bail!("Cannot enqueue task: description is empty.");
    }

    let spec_path = normalize_optional(spec_path);
    let milestone_id = normalize_optional(milestone_id);
    if spec_path.is_some() != milestone_id.is_some() {
        anyhow::bail!("Cannot enqueue task: spec_path and milestone_id must be provided together.");
    }

    if let (Some(spec_path), Some(milestone_id)) = (spec_path.as_deref(), milestone_id.as_deref())
        && let Some(existing) =
            project::find_non_terminal_task_by_origin(spec_path, milestone_id).await?
    {
        anyhow::bail!(
            "Cannot enqueue task: milestone {milestone_id} from {spec_path} already has task {} ({}) in status {}.",
            existing.id,
            existing.path,
            existing.status
        );
    }

    let artifact = project::allocate_task_artifact().await?;
    tokio::fs::write(&artifact.path, &description)
        .await
        .with_context(|| format!("Failed to write {}", artifact.path))?;
    tokio::fs::create_dir_all(&artifact.run_dir)
        .await
        .with_context(|| format!("Failed to create {}", artifact.run_dir))?;

    project::record_task_status_with_origin(
        &artifact.id,
        &artifact.path,
        project::TaskStatus::Pending,
        spec_path.as_deref(),
        milestone_id.as_deref(),
    )
    .await?;
    project::record_runtime_event_best_effort(
        None,
        "task_enqueued",
        serde_json::json!({
            "task_id": artifact.id,
            "path": artifact.path,
            "run_dir": artifact.run_dir,
            "spec_path": spec_path,
            "milestone_id": milestone_id,
            "description_bytes": description.len(),
        }),
    )
    .await;

    info!(task_id = artifact.id, "Task enqueued, DB task → pending");
    Ok(format!(
        "Task {} enqueued. State: pending. Artifact: {}",
        artifact.id, artifact.path
    ))
}

fn normalize_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn parse_input(params: CallToolRequestParams) -> Result<(String, Option<String>, Option<String>)> {
    let args = params.args.unwrap_or_default();
    let description = args
        .get("description")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("Cannot enqueue task: description is required."))?;
    let spec_path = args
        .get("spec_path")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let milestone_id = args
        .get("milestone_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    Ok((description, spec_path, milestone_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::LocalProjectRef;
    use std::collections::HashMap;
    use tempfile::TempDir;

    async fn setup() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        let data_dir = dir.path().join(".ferrus/projects/test-project");
        std::fs::create_dir_all(dir.path().join(".ferrus")).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();
        let local_ref = LocalProjectRef {
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

    #[test]
    fn parse_input_reads_named_arguments() {
        let params = CallToolRequestParams {
            name: "enqueue_task".to_string(),
            args: Some(HashMap::from([
                ("description".to_string(), serde_json::json!("Build task")),
                (
                    "spec_path".to_string(),
                    serde_json::json!("docs/specs/spec.md"),
                ),
                ("milestone_id".to_string(), serde_json::json!("m1.0")),
            ])),
            meta: None,
        };

        let (description, spec_path, milestone_id) = parse_input(params).unwrap();

        assert_eq!(description, "Build task");
        assert_eq!(spec_path.as_deref(), Some("docs/specs/spec.md"));
        assert_eq!(milestone_id.as_deref(), Some("m1.0"));
    }

    #[tokio::test]
    async fn enqueue_task_writes_pending_artifact_without_state_json() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;

        let response = run(
            "Build queued task".to_string(),
            Some("docs/specs/spec.md".to_string()),
            Some("m1.0".to_string()),
        )
        .await
        .unwrap();

        assert!(response.contains("t-001"));
        crate::test_support::assert_no_state_json();
        assert_eq!(
            tokio::fs::read_to_string(".ferrus/tasks/t-001.md")
                .await
                .unwrap(),
            "Build queued task"
        );
        assert!(std::path::Path::new(".ferrus/runs/t-001").is_dir());
        let tasks = project::list_tasks().await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "t-001");
        assert_eq!(tasks[0].status, "pending");
        assert_eq!(tasks[0].spec_path.as_deref(), Some("docs/specs/spec.md"));
        assert_eq!(tasks[0].milestone_id.as_deref(), Some("m1.0"));

        teardown(previous);
    }

    #[tokio::test]
    async fn enqueue_task_rejects_duplicate_non_terminal_origin() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;

        run(
            "First task".to_string(),
            Some("docs/specs/spec.md".to_string()),
            Some("m1.0".to_string()),
        )
        .await
        .unwrap();
        let err = run(
            "Duplicate task".to_string(),
            Some("docs/specs/spec.md".to_string()),
            Some("m1.0".to_string()),
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("already has task t-001"));

        teardown(previous);
    }
}
