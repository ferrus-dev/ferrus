use neva::prelude::*;

use crate::{project::RuntimeTaskContext, state::store};

fn to_err(e: impl std::fmt::Display) -> Error {
    Error::new(
        ErrorCode::InternalError,
        std::io::Error::other(e.to_string()),
    )
}

pub async fn executor_context_for_agent(agent_id: Option<&str>) -> Result<GetPromptResult, Error> {
    let runtime_context = runtime_context(agent_id).await.map_err(to_err)?;
    let task = read_task(runtime_context.as_ref()).await.map_err(to_err)?;

    let mut sections = vec![
        state_section("State", runtime_context.as_ref()).map_err(to_err)?,
        format!("## Task\n\n{task}"),
    ];

    let review = read_review(runtime_context.as_ref())
        .await
        .unwrap_or_default();
    if !review.trim().is_empty() {
        sections.push(format!("## Review Notes (Re-address)\n\n{review}"));
    }

    Ok(GetPromptResult::new()
        .with_descr("Executor task context: state, task description, and review notes")
        .with_message(PromptMessage::user().with(sections.join("\n\n---\n\n"))))
}

pub async fn executor_context(ctx: neva::Context) -> Result<GetPromptResult, Error> {
    let agent_id = ctx
        .resolve::<crate::server::ServerContext>()?
        .agent_id()
        .to_string();
    executor_context_for_agent(Some(agent_id.as_str())).await
}

pub async fn supervisor_review_for_agent(agent_id: Option<&str>) -> Result<GetPromptResult, Error> {
    let runtime_context = runtime_context(agent_id).await.map_err(to_err)?;
    let task = read_task(runtime_context.as_ref()).await.map_err(to_err)?;

    let mut sections = vec![
        state_section("State", runtime_context.as_ref()).map_err(to_err)?,
        format!("## Task\n\n{task}"),
    ];

    let submission = read_submission(runtime_context.as_ref())
        .await
        .unwrap_or_default();
    if !submission.trim().is_empty() {
        sections.push(format!("## Submission Notes\n\n{submission}"));
    }

    Ok(GetPromptResult::new()
        .with_descr("Supervisor review context: state, task description, and submission notes")
        .with_message(PromptMessage::user().with(sections.join("\n\n---\n\n"))))
}

pub async fn supervisor_review(ctx: neva::Context) -> Result<GetPromptResult, Error> {
    let agent_id = ctx
        .resolve::<crate::server::ServerContext>()?
        .agent_id()
        .to_string();
    supervisor_review_for_agent(Some(agent_id.as_str())).await
}

async fn runtime_context(agent_id: Option<&str>) -> anyhow::Result<Option<RuntimeTaskContext>> {
    let Some(agent_id) = agent_id else {
        return Ok(None);
    };
    crate::project::runtime_task_context_for_agent(agent_id).await
}

async fn read_task(context: Option<&RuntimeTaskContext>) -> anyhow::Result<String> {
    if let Some(context) = context {
        return store::read_task_at(&context.task_path).await;
    }
    store::read_task().await
}

async fn read_review(context: Option<&RuntimeTaskContext>) -> anyhow::Result<String> {
    if let Some(context) = context {
        return store::read_review_for_run_dir(&context.run_dir).await;
    }
    Ok(String::new())
}

async fn read_submission(context: Option<&RuntimeTaskContext>) -> anyhow::Result<String> {
    if let Some(context) = context {
        return store::read_submission_for_run_dir(&context.run_dir).await;
    }
    Ok(String::new())
}

fn state_section(title: &str, context: Option<&RuntimeTaskContext>) -> anyhow::Result<String> {
    let (state_label, check_retries, review_cycles) = if let Some(context) = context {
        (
            context.status.clone(),
            context.check_retries,
            context.review_cycles,
        )
    } else {
        ("sqlite-runtime".to_string(), 0, 0)
    };
    let mut lines = vec![format!(
        "## {title}\n\nCurrent state: **{state_label}** | Check retries: {check_retries} | Review cycles: {review_cycles}"
    )];
    if let Some(context) = context {
        lines.push(format!(
            "Agent task: **{}** | Task status: **{}** | Task path: `{}` | Run dir: `{}`",
            context.task_id, context.status, context.task_path, context.run_dir
        ));
        if let Some(run_id) = context.run_id.as_deref() {
            lines.push(format!("Run: `{run_id}`"));
        }
        if let Some(workspace_path) = context.workspace_path.as_deref() {
            lines.push(format!("Workspace: `{workspace_path}`"));
        }
    } else {
        lines.push(
            "No scoped SQLite task context is attached to this agent; call the appropriate wait tool first."
                .to_string(),
        );
    }
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use neva::types::Content;
    use tempfile::TempDir;

    async fn setup() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::fs::create_dir_all(dir.path().join(".ferrus/tasks")).unwrap();
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

    fn prompt_text(result: GetPromptResult) -> String {
        match &result.messages.first().unwrap().content {
            Content::Text(text) => text.text.clone(),
            other => panic!("expected text prompt content, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn executor_prompt_prefers_agent_runtime_task_context() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        tokio::fs::write(".ferrus/TASK.md", "legacy task")
            .await
            .unwrap();
        tokio::fs::write(".ferrus/tasks/t-007.md", "scoped task")
            .await
            .unwrap();
        store::write_review_for_run_dir(".ferrus/runs/t-007", "scoped review")
            .await
            .unwrap();
        crate::project::record_task_status(
            "t-007",
            ".ferrus/tasks/t-007.md",
            crate::project::TaskStatus::Addressing,
        )
        .await
        .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "executor:codex:7", 60)
            .await
            .unwrap();

        let text = prompt_text(
            executor_context_for_agent(Some("executor:codex:7"))
                .await
                .unwrap(),
        );

        assert!(text.contains("scoped task"));
        assert!(text.contains("scoped review"));
        assert!(text.contains("Current state: **addressing**"));
        assert!(text.contains("Agent task: **t-007**"));
        assert!(!text.contains("legacy task"));
        teardown(previous);
    }

    #[tokio::test]
    async fn supervisor_prompt_prefers_agent_runtime_task_context() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        tokio::fs::write(".ferrus/TASK.md", "legacy task")
            .await
            .unwrap();
        tokio::fs::write(".ferrus/tasks/t-007.md", "review task")
            .await
            .unwrap();
        store::write_submission_for_run_dir(".ferrus/runs/t-007", "scoped submission")
            .await
            .unwrap();
        crate::project::record_task_status(
            "t-007",
            ".ferrus/tasks/t-007.md",
            crate::project::TaskStatus::Reviewing,
        )
        .await
        .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "supervisor:codex:7", 60)
            .await
            .unwrap();

        let text = prompt_text(
            supervisor_review_for_agent(Some("supervisor:codex:7"))
                .await
                .unwrap(),
        );

        assert!(text.contains("review task"));
        assert!(text.contains("scoped submission"));
        assert!(text.contains("Current state: **reviewing**"));
        assert!(text.contains("Agent task: **t-007**"));
        assert!(!text.contains("legacy task"));
        teardown(previous);
    }

    #[tokio::test]
    async fn executor_prompt_uses_database_context_when_state_json_is_absent() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        tokio::fs::write(".ferrus/tasks/t-007.md", "scoped task")
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

        let text = prompt_text(
            executor_context_for_agent(Some("executor:codex:7"))
                .await
                .unwrap(),
        );

        assert!(text.contains("Current state: **executing**"));
        assert!(text.contains("scoped task"));
        teardown(previous);
    }

    #[tokio::test]
    async fn executor_prompt_without_task_context_does_not_require_state_json() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        tokio::fs::write(".ferrus/TASK.md", "task template")
            .await
            .unwrap();

        let text = prompt_text(
            executor_context_for_agent(Some("executor:codex:7"))
                .await
                .unwrap(),
        );

        assert!(text.contains("Current state: **sqlite-runtime**"));
        assert!(text.contains("No scoped SQLite task context"));
        assert!(text.contains("task template"));
        teardown(previous);
    }
}
