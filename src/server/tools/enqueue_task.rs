//! Validate task intent and optional milestone origin, then enqueue a scoped runtime task.

use anyhow::Result;
use neva::prelude::*;
use serde::Deserialize;
use tracing::info;

use crate::project;

use super::tool_err;

pub const DESCRIPTION: &str = "Enqueue an approved task artifact for later execution. Writes \
     .ferrus/tasks/<task-id>.md and records a pending SQLite task row.";

pub const INPUT_SCHEMA: &str = r#"{
    "type": "object",
    "properties": {
        "input": {
            "type": "object",
            "description": "Approved task enqueue request",
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
        }
    },
    "required": ["input"]
}"#;

pub async fn handler(input: Json<EnqueueTaskInput>) -> Result<String, Error> {
    let input = validate_input(input.into_inner()).map_err(tool_err)?;
    run(input.description, input.spec_path, input.milestone_id)
        .await
        .map_err(tool_err)
}

#[derive(Debug, Deserialize)]
pub struct EnqueueTaskInput {
    description: String,
    spec_path: Option<String>,
    milestone_id: Option<String>,
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

    let artifact = project::create_pending_task_artifact(
        &description,
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

    info!(task_id = artifact.id, "Task enqueued, DB task -> pending");
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

fn validate_input(input: EnqueueTaskInput) -> Result<EnqueueTaskInput> {
    if input.description.trim().is_empty() {
        anyhow::bail!("Cannot enqueue task: description is required.");
    }
    Ok(input)
}

#[cfg(test)]
mod tests {
    //! Wrapped MCP input, task validation, and enqueue origin handling.

    use super::*;
    use crate::project::LocalProjectRef;
    use neva::types::{ArgNames, CallToolRequestParams, FromHandlerArgs};
    use std::collections::HashMap;
    use tempfile::TempDir;

    struct TestWorkspace {
        _dir: TempDir,
        previous: std::path::PathBuf,
    }

    impl Drop for TestWorkspace {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.previous);
        }
    }

    async fn setup() -> TestWorkspace {
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
        TestWorkspace {
            _dir: dir,
            previous,
        }
    }

    #[test]
    fn input_schema_requires_wrapped_input_object() {
        let schema: serde_json::Value = serde_json::from_str(INPUT_SCHEMA).unwrap();

        assert_eq!(
            schema
                .get("properties")
                .and_then(|properties| properties.get("input"))
                .and_then(|input| input.get("type"))
                .and_then(serde_json::Value::as_str),
            Some("object")
        );
        assert_eq!(
            schema
                .get("required")
                .and_then(serde_json::Value::as_array)
                .and_then(|required| required.first())
                .and_then(serde_json::Value::as_str),
            Some("input")
        );
    }

    #[test]
    fn validates_extracted_request_object() {
        let input = validate_input(EnqueueTaskInput {
            description: "Build task".to_string(),
            spec_path: Some("docs/specs/spec.md".to_string()),
            milestone_id: Some("m1.0".to_string()),
        })
        .unwrap();

        assert_eq!(input.description, "Build task");
        assert_eq!(input.spec_path.as_deref(), Some("docs/specs/spec.md"));
        assert_eq!(input.milestone_id.as_deref(), Some("m1.0"));
    }

    #[test]
    fn neva_extracts_wrapped_input_as_single_tool_argument() {
        let params = CallToolRequestParams {
            name: "enqueue_task".to_string(),
            args: Some(HashMap::from([(
                "input".to_string(),
                serde_json::json!({
                    "description": "Build task",
                    "spec_path": "docs/specs/spec.md",
                    "milestone_id": "m1.0",
                }),
            )])),
            meta: None,
        };

        let (input,): (Json<EnqueueTaskInput>,) =
            FromHandlerArgs::from_args(params, &ArgNames::new(["input"])).unwrap();
        let input = validate_input(input.into_inner()).unwrap();

        assert_eq!(input.description, "Build task");
        assert_eq!(input.spec_path.as_deref(), Some("docs/specs/spec.md"));
        assert_eq!(input.milestone_id.as_deref(), Some("m1.0"));
    }

    #[test]
    fn neva_json_extractor_rejects_bare_string() {
        let params = CallToolRequestParams {
            name: "enqueue_task".to_string(),
            args: Some(HashMap::from([(
                "input".to_string(),
                serde_json::json!("m1.0"),
            )])),
            meta: None,
        };
        let result: std::result::Result<(Json<EnqueueTaskInput>,), Error> =
            FromHandlerArgs::from_args(params, &ArgNames::new(["input"]));
        let err = result.unwrap_err();

        assert!(err.to_string().contains("invalid type: string"));
    }

    #[tokio::test]
    async fn enqueue_task_writes_pending_artifact_without_state_json() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let _workspace = setup().await;

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
    }

    #[tokio::test]
    async fn enqueue_task_rejects_duplicate_non_terminal_origin() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let _workspace = setup().await;

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
    }

    #[tokio::test]
    async fn concurrent_enqueue_allows_only_one_task_per_origin() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let _workspace = setup().await;

        let first = run(
            "First task".to_string(),
            Some("docs/specs/spec.md".to_string()),
            Some("m1.0".to_string()),
        );
        let second = run(
            "Second task".to_string(),
            Some("docs/specs/spec.md".to_string()),
            Some("m1.0".to_string()),
        );
        let (first, second) = tokio::join!(first, second);

        assert_ne!(first.is_ok(), second.is_ok());
        let error = first.err().or_else(|| second.err()).unwrap();
        assert!(
            error.to_string().contains("already has task"),
            "unexpected concurrent enqueue error: {error:#}"
        );
        let tasks = project::list_tasks().await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].status, "pending");
    }
}
