use anyhow::Result;
use neva::prelude::*;

use crate::project;

use super::tool_err;

pub const DESCRIPTION: &str = "Query the current state of the ferrus orchestration system. Returns state, \
     retry counters, scoped task context, and any failure reason. Safe to call from any state.";

pub async fn handler(ctx: neva::Context) -> Result<String, Error> {
    let agent_id = super::agent_id_from_context(&ctx)?;
    handler_for_agent(&agent_id).await
}

pub async fn handler_for_agent(agent_id: &str) -> Result<String, Error> {
    run(Some(agent_id)).await.map_err(tool_err)
}

async fn run(agent_id: Option<&str>) -> Result<String> {
    let context = match agent_id {
        Some(agent_id) => project::runtime_task_context_for_agent(agent_id).await?,
        None => None,
    };
    let mut lines = if let Some(context) = context.as_ref() {
        vec![
            format!("**State:** {}", context.status),
            format!("**Check retries:** {}", context.check_retries),
            format!("**Review cycles:** {}", context.review_cycles),
        ]
    } else {
        sqlite_status_lines().await?
    };

    if let Some(reason) = context
        .as_ref()
        .and_then(|context| context.failure_reason.as_ref())
    {
        lines.push(format!("**Failure reason:** {reason}"));
    }

    if let Some(agent_id) = agent_id
        && let Some(context) = context
    {
        lines.push(String::new());
        lines.push(format!("**Agent:** {agent_id}"));
        lines.push(format!("**Task:** {}", context.task_id));
        lines.push(format!("**Task status:** {}", context.status));
        lines.push(format!("**Task path:** {}", context.task_path));
        lines.push(format!("**Run dir:** {}", context.run_dir));
        lines.push(format!("**Task check retries:** {}", context.check_retries));
        lines.push(format!("**Task review cycles:** {}", context.review_cycles));
        if let Some(run_id) = context.run_id {
            lines.push(format!("**Run:** {run_id}"));
        }
        if let Some(workspace_path) = context.workspace_path {
            lines.push(format!("**Workspace:** {workspace_path}"));
        }
        if let Some(reason) = context.failure_reason {
            lines.push(format!("**Task failure reason:** {reason}"));
        }
    }

    Ok(lines.join("\n"))
}

async fn sqlite_status_lines() -> Result<Vec<String>> {
    let tasks = project::list_tasks().await?;
    let running = tasks
        .iter()
        .filter(|task| {
            matches!(
                task.status.parse::<project::TaskStatus>().ok(),
                Some(
                    project::TaskStatus::Executing
                        | project::TaskStatus::Addressing
                        | project::TaskStatus::Consultation
                        | project::TaskStatus::Reviewing
                )
            )
        })
        .count();
    let awaiting_human = tasks
        .iter()
        .filter(|task| {
            task.status.parse::<project::TaskStatus>().ok()
                == Some(project::TaskStatus::AwaitingHuman)
        })
        .count();
    let pending = tasks
        .iter()
        .filter(|task| {
            task.status.parse::<project::TaskStatus>().ok() == Some(project::TaskStatus::Pending)
        })
        .count();
    let complete = tasks
        .iter()
        .filter(|task| {
            task.status.parse::<project::TaskStatus>().ok() == Some(project::TaskStatus::Complete)
        })
        .count();

    let mut lines = vec![
        "**State:** sqlite-runtime".to_string(),
        format!(
            "**Tasks:** {running} running, {awaiting_human} awaiting human, {pending} pending, {complete} complete"
        ),
    ];
    for task in tasks.iter().filter(|task| {
        task.status
            .parse::<project::TaskStatus>()
            .is_ok_and(|status| !status.is_terminal())
    }) {
        let claim = task
            .claimed_by
            .as_deref()
            .map(|claimed_by| format!(" claimed_by={claimed_by}"))
            .unwrap_or_default();
        lines.push(format!(
            "- {} {} ({}){}",
            task.id, task.status, task.path, claim
        ));
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
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
    async fn status_includes_agent_runtime_task_context() {
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

        let output = run(Some("executor:codex:7")).await.unwrap();

        assert!(output.contains("**State:** executing"));
        assert!(output.contains("**Agent:** executor:codex:7"));
        assert!(output.contains("**Task:** t-007"));
        assert!(output.contains("**Task status:** executing"));
        assert!(output.contains("**Task path:** .ferrus/tasks/t-007.md"));
        teardown(previous);
    }

    #[tokio::test]
    async fn status_uses_database_context_when_state_json_is_absent() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
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

        let output = run(Some("executor:codex:7")).await.unwrap();

        assert!(output.contains("**State:** addressing"));
        assert!(output.contains("**Task:** t-007"));
        teardown(previous);
    }

    #[tokio::test]
    async fn status_reports_sqlite_summary_when_state_json_and_agent_context_are_absent() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        crate::project::record_task_status(
            "t-007",
            ".ferrus/tasks/t-007.md",
            crate::project::TaskStatus::Pending,
        )
        .await
        .unwrap();

        let output = run(None).await.unwrap();

        assert!(output.contains("**State:** sqlite-runtime"));
        assert!(output.contains("**Tasks:** 0 running, 0 awaiting human, 1 pending, 0 complete"));
        assert!(output.contains("- t-007 pending (.ferrus/tasks/t-007.md)"));
        teardown(previous);
    }
}
