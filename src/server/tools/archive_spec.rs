use anyhow::Result;
use neva::prelude::*;
use serde::Deserialize;

use crate::project;

use super::tool_err;

pub const DESCRIPTION: &str = "Archive completed spec task/run artifacts and write approved \
     project memory. Appends or replaces the spec's ## Outcome section, moves linked task and run \
     artifacts to the machine-local project archive, and records archive metadata in SQLite. Must \
     only be called after explicit user approval of the outcome text.";

pub const INPUT_SCHEMA: &str = r#"{
    "type": "object",
    "properties": {
        "input": {
            "type": "object",
            "description": "Approved spec archive request",
            "properties": {
                "spec_path": {
                    "type": "string",
                    "description": "Path of the completed spec to archive"
                },
                "outcome": {
                    "type": "string",
                    "description": "Approved Markdown for the spec's ## Outcome section"
                }
            },
            "required": ["spec_path", "outcome"]
        }
    },
    "required": ["input"]
}"#;

#[derive(Debug, Deserialize)]
struct ArchiveSpecInput {
    spec_path: String,
    outcome: String,
}

pub async fn handler(input: serde_json::Value) -> Result<String, Error> {
    let input = parse_input(input).map_err(tool_err)?;
    run(input.spec_path, input.outcome).await.map_err(tool_err)
}

async fn run(spec_path: String, outcome: String) -> Result<String> {
    let result = project::archive_completed_spec(&spec_path, &outcome).await?;
    crate::project_memory_runtime::refresh_after_archive_best_effort().await;
    Ok(format!(
        "Spec archived. Archive: {}. Tasks archived: {}. Runs archived: {}.",
        result.archive_dir, result.archived_tasks, result.archived_runs
    ))
}

fn parse_input(input: serde_json::Value) -> Result<ArchiveSpecInput> {
    let input: ArchiveSpecInput = serde_json::from_value(input).map_err(|err| {
        anyhow::anyhow!(
            "Cannot archive spec: expected input object with spec_path and outcome ({err})."
        )
    })?;
    if input.spec_path.trim().is_empty() {
        anyhow::bail!("Cannot archive spec: spec_path is required.");
    }
    if input.outcome.trim().is_empty() {
        anyhow::bail!("Cannot archive spec: outcome is required.");
    }
    Ok(input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use neva::types::CallToolRequestParams;
    use std::collections::HashMap;

    #[test]
    fn parses_archive_spec_input() {
        let input = parse_input(serde_json::json!({
            "spec_path": "docs/specs/example.md",
            "outcome": "## Outcome\n\nDone."
        }))
        .unwrap();
        assert_eq!(input.spec_path, "docs/specs/example.md");
        assert!(input.outcome.contains("Done."));
    }

    #[test]
    fn neva_extracts_wrapped_input_as_single_tool_argument() {
        let params = CallToolRequestParams {
            name: "archive_spec".to_string(),
            args: Some(HashMap::from([(
                "input".to_string(),
                serde_json::json!({
                    "spec_path": "docs/specs/example.md",
                    "outcome": "## Outcome\n\nDone."
                }),
            )])),
            meta: None,
        };

        let (input,): (serde_json::Value,) = params.try_into().unwrap();
        let input = parse_input(input).unwrap();

        assert_eq!(input.spec_path, "docs/specs/example.md");
        assert!(input.outcome.contains("Done."));
    }
}
