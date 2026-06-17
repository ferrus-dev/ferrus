use anyhow::Result;
use neva::prelude::*;
use tracing::{info, warn};

use crate::{
    config::Config,
    project::{self, TaskCheckFailure},
};

use super::{
    check_gate::{self, CheckGateResult},
    ensure_lease_owner_or_reclaim, require_runtime_task_context, tool_err,
};

pub const DESCRIPTION: &str = "Run all configured checks (clippy, fmt, tests, etc.) against the current \
     codebase. Can be called from state Executing or Addressing. \
     On pass: stay in the current work state and clear check-failure metadata. \
     On fail: stay in the current work state (or state → Failed if the retry \
     limit is exhausted).";

pub async fn handler(ctx: neva::Context) -> Result<String, Error> {
    let agent_id = super::agent_id_from_context(&ctx)?;
    handler_for_agent(&agent_id).await
}

pub async fn handler_for_agent(agent_id: &str) -> Result<String, Error> {
    run(Some(agent_id)).await.map_err(tool_err)
}

async fn run(agent_id: Option<&str>) -> Result<String> {
    let Some(agent_id) = agent_id else {
        anyhow::bail!("Cannot run task checks without an agent runtime context");
    };
    let config = Config::load().await?;
    let context = require_runtime_task_context(agent_id).await?;

    if !context
        .status
        .parse::<project::TaskStatus>()?
        .is_executor_working()
    {
        anyhow::bail!(
            "Cannot run checks from state {}. \
             Checks are only valid in Executing or Addressing state.",
            context.status
        );
    }
    ensure_lease_owner_or_reclaim(agent_id, config.lease.ttl_secs).await?;

    if config.checks.commands.is_empty() {
        project::record_task_check_passed(&context.task_id).await?;
        project::record_runtime_event_best_effort(
            context.run_id.clone(),
            "check_passed",
            serde_json::json!({ "commands": 0 }),
        )
        .await;
        info!("No check commands configured; treating /check as pass");
        return Ok(
            "All checks passed. Warning: no check commands are configured in ferrus.toml. State remains unchanged."
                .to_string(),
        );
    }

    info!("Running {} check(s)", config.checks.commands.len());
    let attempt = context.check_retries + 1;
    let log_scope = context
        .run_id
        .as_deref()
        .unwrap_or(context.task_id.as_str());
    match check_gate::run(&config, attempt, log_scope).await? {
        CheckGateResult::Passed => {
            project::record_task_check_passed(&context.task_id).await?;
            project::record_runtime_event_best_effort(
                context.run_id.clone(),
                "check_passed",
                serde_json::json!({ "commands": config.checks.commands.len() }),
            )
            .await;
            let state_label = context.status.clone();
            info!(
                state = state_label,
                "All checks passed; staying in current work state"
            );
            Ok(format!(
                "All checks passed. State remains {state_label}. Continue working or call /submit when the task is ready for review."
            ))
        }
        CheckGateResult::Failed(failure) => {
            match project::record_task_check_failed(
                &context.task_id,
                &failure.failure_reason,
                config.limits.max_check_retries,
            )
            .await?
            {
                TaskCheckFailure::Failed { retries } => {
                    project::record_runtime_event_best_effort(
                        context.run_id.clone(),
                        "check_failed",
                        serde_json::json!({
                            "task_id": context.task_id,
                            "retries": retries,
                            "max_retries": config.limits.max_check_retries,
                            "state": context.status,
                        }),
                    )
                    .await;
                    warn!(
                        retries,
                        task_id = context.task_id,
                        "Checks failed; DB task stays in current work state"
                    );
                    Ok(format!(
                        "Checks failed (retry {}/{}).\n\n{}\n\nState remains {}. Fix the issues and call /check again.",
                        retries, config.limits.max_check_retries, failure.report, context.status,
                    ))
                }
                TaskCheckFailure::LimitExceeded { retries } => {
                    project::record_runtime_event_best_effort(
                        context.run_id.clone(),
                        "check_limit_exceeded",
                        serde_json::json!({
                            "task_id": context.task_id,
                            "retries": retries,
                            "max_retries": config.limits.max_check_retries,
                        }),
                    )
                    .await;
                    warn!(retries, "Check retry limit reached, state → Failed");
                    Ok(format!(
                        "Check retry limit reached ({retries}/{}).\n\n{}\n\nState is now Failed. A human must call /reset to recover.",
                        config.limits.max_check_retries, failure.report,
                    ))
                }
            }
        }
    }
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
        tokio::fs::write(
            "ferrus.toml",
            "[checks]\ncommands = []\n\n[limits]\nmax_check_retries = 2\nmax_review_cycles = 3\nmax_feedback_lines = 30\nwait_timeout_secs = 1\n\n[lease]\nttl_secs = 60\n",
        )
        .await
        .unwrap();
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
    async fn check_pass_clears_database_retry_metadata() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        crate::project::record_task_status(
            "t-001",
            ".ferrus/tasks/t-001.md",
            crate::project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        crate::project::record_task_check_failed("t-001", "fmt failed", 2)
            .await
            .unwrap();
        crate::project::claim_task("t-001", ".ferrus/tasks/t-001.md", "executor:codex:1", 60)
            .await
            .unwrap();

        run(Some("executor:codex:1")).await.unwrap();

        crate::test_support::assert_no_state_json();
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-001").unwrap();
        assert_eq!(task.status, "executing");
        assert_eq!(task.check_retries, 0);
        assert_eq!(task.failure_reason, None);
        assert_eq!(task.claimed_by.as_deref(), Some("executor:codex:1"));

        teardown(previous);
    }

    #[tokio::test]
    async fn check_uses_database_context_when_state_json_is_absent() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        crate::project::record_task_status(
            "t-007",
            ".ferrus/tasks/t-007.md",
            crate::project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        crate::project::record_task_check_failed("t-007", "fmt failed", 2)
            .await
            .unwrap();
        crate::project::claim_task("t-007", ".ferrus/tasks/t-007.md", "executor:codex:7", 60)
            .await
            .unwrap();

        run(Some("executor:codex:7")).await.unwrap();

        crate::test_support::assert_no_state_json();
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "executing");
        assert_eq!(task.check_retries, 0);
        assert_eq!(task.failure_reason, None);
        assert_eq!(task.claimed_by.as_deref(), Some("executor:codex:7"));

        teardown(previous);
    }
}
