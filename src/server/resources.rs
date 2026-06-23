use neva::prelude::*;

use crate::{
    agent_id::{ENV_AGENT_ID, ENV_PROJECT_ROOT, ENV_RUN_ID, ENV_TASK_ID},
    project,
    state::store,
    templates::SPEC_TEMPLATE,
};

fn to_err(e: impl std::fmt::Display) -> Error {
    Error::new(
        ErrorCode::InternalError,
        std::io::Error::other(e.to_string()),
    )
}

pub async fn read_for_agent(
    agent_id: Option<&str>,
    file: String,
) -> Result<ReadResourceResult, Error> {
    let (mime, content) = match file.as_str() {
        "task" => (
            "text/markdown",
            read_current_task_for_agent(agent_id)
                .await
                .map_err(to_err)?,
        ),
        "task_template" => (
            "text/markdown",
            store::read_task_template().await.map_err(to_err)?,
        ),
        "review" => (
            "text/markdown",
            read_review_for_agent(agent_id).await.map_err(to_err)?,
        ),
        "submission" => (
            "text/markdown",
            read_submission_for_agent(agent_id).await.map_err(to_err)?,
        ),
        "question" => (
            "text/markdown",
            read_question_for_agent(agent_id).await.map_err(to_err)?,
        ),
        "answer" => (
            "text/markdown",
            read_answer_for_agent(agent_id).await.map_err(to_err)?,
        ),
        "consult_template" => (
            "text/markdown",
            tokio::fs::read_to_string(store::resolve_project_path(".ferrus/CONSULT_TEMPLATE.md"))
                .await
                .unwrap_or_default(),
        ),
        "spec_template" => (
            "text/markdown",
            tokio::fs::read_to_string(store::resolve_project_path(".ferrus/SPEC_TEMPLATE.md"))
                .await
                .unwrap_or_else(|_| SPEC_TEMPLATE.to_string()),
        ),
        "consult_request" => (
            "text/markdown",
            read_consult_request_for_agent(agent_id)
                .await
                .map_err(to_err)?,
        ),
        "consult_response" => (
            "text/markdown",
            read_consult_response_for_agent(agent_id)
                .await
                .map_err(to_err)?,
        ),
        "state" => {
            let json = read_runtime_state_for_agent(agent_id)
                .await
                .map_err(to_err)?;
            ("application/json", json)
        }
        "runtime_context" => (
            "application/json",
            read_runtime_context_for_agent(agent_id, None)
                .await
                .map_err(to_err)?,
        ),
        _ => {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                std::io::Error::other(format!("Unknown ferrus resource: {file}")),
            ));
        }
    };

    let uri = format!("ferrus://{file}");
    Ok(ReadResourceResult::from(
        TextResourceContents::new(uri, content).with_mime(mime),
    ))
}

pub async fn read(ctx: neva::di::Dc<crate::server::ServerContext>, file: String) -> Result<ReadResourceResult, Error> {
    let agent_id = ctx.agent_id();
    if file == "runtime_context" {
        let content =
            read_runtime_context_for_agent(Some(agent_id), Some(&ctx))
                .await
                .map_err(to_err)?;
        return Ok(ReadResourceResult::from(
            TextResourceContents::new("ferrus://runtime_context", content)
                .with_mime("application/json"),
        ));
    }
    read_for_agent(Some(agent_id), file).await
}

async fn read_review_for_agent(agent_id: Option<&str>) -> anyhow::Result<String> {
    if let Some(agent_id) = agent_id
        && let Some(context) = project::runtime_task_context_for_agent(agent_id).await?
    {
        return store::read_review_for_run_dir(&context.run_dir).await;
    }
    Ok(String::new())
}

async fn read_submission_for_agent(agent_id: Option<&str>) -> anyhow::Result<String> {
    if let Some(agent_id) = agent_id
        && let Some(context) = project::runtime_task_context_for_agent(agent_id).await?
    {
        return store::read_submission_for_run_dir(&context.run_dir).await;
    }
    Ok(String::new())
}

async fn read_question_for_agent(agent_id: Option<&str>) -> anyhow::Result<String> {
    if let Some(agent_id) = agent_id
        && let Some(context) = project::runtime_task_context_for_agent(agent_id).await?
        && let Ok(contents) = store::read_question_for_run_dir(&context.run_dir).await
    {
        return Ok(contents);
    }
    Ok(String::new())
}

async fn read_answer_for_agent(agent_id: Option<&str>) -> anyhow::Result<String> {
    if let Some(agent_id) = agent_id
        && let Some(context) = project::runtime_task_context_for_agent(agent_id).await?
        && let Ok(contents) = store::read_answer_for_run_dir(&context.run_dir).await
    {
        return Ok(contents);
    }
    Ok(String::new())
}

async fn read_consult_request_for_agent(agent_id: Option<&str>) -> anyhow::Result<String> {
    if let Some(agent_id) = agent_id
        && let Some(context) = project::runtime_task_context_for_agent(agent_id).await?
        && let Ok(contents) = store::read_consult_request_for_run_dir(&context.run_dir).await
    {
        return Ok(contents);
    }
    Ok(String::new())
}

async fn read_consult_response_for_agent(agent_id: Option<&str>) -> anyhow::Result<String> {
    if let Some(agent_id) = agent_id
        && let Some(context) = project::runtime_task_context_for_agent(agent_id).await?
        && let Ok(contents) = store::read_consult_response_for_run_dir(&context.run_dir).await
    {
        return Ok(contents);
    }
    Ok(String::new())
}

async fn read_current_task_for_agent(agent_id: Option<&str>) -> anyhow::Result<String> {
    if let Some(agent_id) = agent_id
        && let Some(context) = project::runtime_task_context_for_agent(agent_id).await?
        && let Ok(contents) = store::read_task_at(&context.task_path).await
    {
        return Ok(contents);
    }
    store::read_task().await
}

async fn read_runtime_context_for_agent(
    agent_id: Option<&str>,
    server_context: Option<&crate::server::ServerContext>,
) -> anyhow::Result<String> {
    let environment = serde_json::json!({
        ENV_AGENT_ID: std::env::var(ENV_AGENT_ID).ok(),
        ENV_TASK_ID: std::env::var(ENV_TASK_ID).ok(),
        ENV_RUN_ID: std::env::var(ENV_RUN_ID).ok(),
        ENV_PROJECT_ROOT: std::env::var(ENV_PROJECT_ROOT).ok(),
    });
    let mut payload = serde_json::json!({
        "agent_id": agent_id,
        "environment": environment,
        "task_context": null,
    });
    if let Some(server_context) = server_context {
        payload["server"] = serde_json::json!({
            "agent_id": server_context.agent_id(),
            "role": server_context.role().map(crate::server::Role::as_str),
            "agent_name": server_context.agent_name(),
            "agent_index": server_context.agent_index(),
        });
    }

    if let Some(agent_id) = agent_id {
        match project::runtime_task_context_for_agent(agent_id).await {
            Ok(Some(context)) => {
                payload["task_context"] = serde_json::json!({
                    "task_id": context.task_id,
                    "task_path": context.task_path,
                    "run_dir": context.run_dir,
                    "status": context.status,
                    "paused_status": context.paused_status,
                    "check_retries": context.check_retries,
                    "review_cycles": context.review_cycles,
                    "failure_reason": context.failure_reason,
                    "run_id": context.run_id,
                    "workspace_path": context.workspace_path,
                });
            }
            Ok(None) => {}
            Err(err) => {
                payload["task_context_error"] = serde_json::json!(err.to_string());
            }
        }
    }

    serde_json::to_string_pretty(&payload).map_err(Into::into)
}

async fn read_runtime_state_for_agent(agent_id: Option<&str>) -> anyhow::Result<String> {
    read_sqlite_runtime_state_for_agent(agent_id).await
}

async fn read_sqlite_runtime_state_for_agent(agent_id: Option<&str>) -> anyhow::Result<String> {
    let selection = project::read_project_selection().await?;
    let tasks = project::list_tasks().await?;
    let runs = project::list_runs(10).await?;

    let task_payloads: Vec<_> = tasks
        .iter()
        .map(|task| {
            serde_json::json!({
                "id": task.id,
                "path": task.path,
                "spec_path": task.spec_path,
                "milestone_id": task.milestone_id,
                "status": task.status,
                "paused_status": task.paused_status,
                "claimed_by": task.claimed_by,
                "lease_until": task.lease_until,
                "last_heartbeat": task.last_heartbeat,
                "check_retries": task.check_retries,
                "review_cycles": task.review_cycles,
                "failure_reason": task.failure_reason,
            })
        })
        .collect();
    let run_payloads: Vec<_> = runs
        .iter()
        .map(|run| {
            serde_json::json!({
                "id": run.id,
                "task_id": run.task_id,
                "role": run.role,
                "agent": run.agent,
                "status": run.status,
                "started_at": run.started_at,
                "updated_at": run.updated_at,
                "pid": run.pid,
                "workspace_path": run.workspace_path,
            })
        })
        .collect();

    let mut payload = serde_json::json!({
        "source": "sqlite",
        "selection": {
            "selected_spec": selection.selected_spec,
        },
        "tasks": task_payloads,
        "recent_runs": run_payloads,
        "agent_id": agent_id,
        "task_context": null,
    });

    if let Some(agent_id) = agent_id
        && let Some(context) = project::runtime_task_context_for_agent(agent_id).await?
    {
        payload["task_context"] = serde_json::json!({
            "task_id": context.task_id,
            "task_path": context.task_path,
            "run_dir": context.run_dir,
            "status": context.status,
            "paused_status": context.paused_status,
            "check_retries": context.check_retries,
            "review_cycles": context.review_cycles,
            "failure_reason": context.failure_reason,
            "run_id": context.run_id,
            "workspace_path": context.workspace_path,
        });
    }

    serde_json::to_string_pretty(&payload).map_err(Into::into)
}

/// Handler for `ferrus://task/{task_id}` resource reads.
pub async fn read_task_by_id(task_id: String) -> Result<ReadResourceResult, Error> {
    if !valid_task_id(&task_id) {
        return Err(Error::new(
            ErrorCode::InvalidRequest,
            std::io::Error::other(format!("Invalid ferrus task id: {task_id}")),
        ));
    }

    let uri = format!("ferrus://task/{task_id}");
    let path = format!(".ferrus/tasks/{task_id}.md");
    let content = store::read_task_at(&path).await.map_err(to_err)?;
    Ok(ReadResourceResult::from(
        TextResourceContents::new(uri, content).with_mime("text/markdown"),
    ))
}

fn valid_task_id(task_id: &str) -> bool {
    let Some(number) = task_id.strip_prefix("t-") else {
        return false;
    };
    !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use neva::types::ResourceContents;
    use tempfile::TempDir;

    async fn setup() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::fs::create_dir_all(dir.path().join(".ferrus/tasks")).unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        (dir, previous)
    }

    fn teardown(previous: std::path::PathBuf) {
        std::env::set_current_dir(previous).unwrap();
    }

    fn text(result: ReadResourceResult) -> String {
        match result.contents.into_iter().next().unwrap() {
            ResourceContents::Text(text) => text.text,
            _ => panic!("expected text resource"),
        }
    }

    #[tokio::test]
    async fn task_template_reads_template_file() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        tokio::fs::write(".ferrus/TASK.md", "template body")
            .await
            .unwrap();

        let result = read_for_agent(None, "task_template".to_string())
            .await
            .unwrap();

        assert_eq!(text(result), "template body");
        teardown(previous);
    }

    #[tokio::test]
    async fn task_id_resource_reads_numbered_task_artifact() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        tokio::fs::write(".ferrus/tasks/t-042.md", "specific task")
            .await
            .unwrap();

        let result = read_task_by_id("t-042".to_string()).await.unwrap();

        assert_eq!(text(result), "specific task");
        teardown(previous);
    }

    #[tokio::test]
    async fn task_resource_prefers_agent_runtime_context() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (dir, previous) = setup().await;
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
        tokio::fs::write(".ferrus/TASK.md", "template body")
            .await
            .unwrap();
        tokio::fs::write(".ferrus/tasks/t-007.md", "assigned task")
            .await
            .unwrap();
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

        let result = read_for_agent(Some("executor:codex:7"), "task".to_string())
            .await
            .unwrap();

        assert_eq!(text(result), "assigned task");
        teardown(previous);
    }

    #[tokio::test]
    async fn run_artifact_resources_prefer_agent_runtime_context() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (dir, previous) = setup().await;
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
        tokio::fs::write(".ferrus/REVIEW.md", "legacy review")
            .await
            .unwrap();
        tokio::fs::write(".ferrus/SUBMISSION.md", "legacy submission")
            .await
            .unwrap();
        tokio::fs::write(".ferrus/QUESTION.md", "legacy question")
            .await
            .unwrap();
        tokio::fs::write(".ferrus/ANSWER.md", "legacy answer")
            .await
            .unwrap();
        store::write_review_for_run_dir(".ferrus/runs/t-007", "scoped review")
            .await
            .unwrap();
        store::write_submission_for_run_dir(".ferrus/runs/t-007", "scoped submission")
            .await
            .unwrap();
        store::write_question_for_run_dir(".ferrus/runs/t-007", "scoped question")
            .await
            .unwrap();
        store::write_answer_for_run_dir(".ferrus/runs/t-007", "scoped answer")
            .await
            .unwrap();
        crate::project::record_task_status(
            "t-007",
            ".ferrus/tasks/t-007.md",
            crate::project::TaskStatus::AwaitingHuman,
        )
        .await
        .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "executor:codex:7", 60)
            .await
            .unwrap();

        for (resource, expected) in [
            ("review", "scoped review"),
            ("submission", "scoped submission"),
            ("question", "scoped question"),
            ("answer", "scoped answer"),
        ] {
            let result = read_for_agent(Some("executor:codex:7"), resource.to_string())
                .await
                .unwrap();
            assert_eq!(text(result), expected);
        }

        teardown(previous);
    }

    #[tokio::test]
    async fn runtime_context_resource_reports_agent_and_task_context() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (dir, previous) = setup().await;
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

        let result = read_for_agent(Some("executor:codex:7"), "runtime_context".to_string())
            .await
            .unwrap();
        let payload: serde_json::Value = serde_json::from_str(&text(result)).unwrap();

        assert_eq!(payload["agent_id"], "executor:codex:7");
        assert_eq!(payload["task_context"]["task_id"], "t-007");
        assert_eq!(
            payload["task_context"]["task_path"],
            ".ferrus/tasks/t-007.md"
        );
        assert_eq!(payload["task_context"]["status"], "executing");
        assert!(payload["task_context_error"].is_null());
        teardown(previous);
    }

    #[tokio::test]
    async fn state_resource_prefers_sqlite_runtime_state_without_state_json() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (dir, previous) = setup().await;
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
        crate::project::write_project_selection(&crate::project::ProjectSelection {
            selected_spec: Some("docs/specs/example.md".to_string()),
        })
        .await
        .unwrap();
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

        let result = read_for_agent(Some("executor:codex:7"), "state".to_string())
            .await
            .unwrap();
        let payload: serde_json::Value = serde_json::from_str(&text(result)).unwrap();

        assert_eq!(payload["source"], "sqlite");
        assert_eq!(
            payload["selection"]["selected_spec"],
            "docs/specs/example.md"
        );
        assert_eq!(payload["tasks"][0]["id"], "t-007");
        assert_eq!(payload["task_context"]["task_id"], "t-007");
        teardown(previous);
    }

    #[tokio::test]
    async fn task_id_resource_rejects_path_traversal() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;

        let err = read_task_by_id("../TASK".to_string()).await.unwrap_err();

        assert!(err.to_string().contains("Invalid ferrus task id"));
        teardown(previous);
    }
}
