use anyhow::Result;
use neva::prelude::*;
use tracing::{info, warn};

use crate::{
    config::Config,
    project::{self, RuntimeTaskContext, TaskReviewRejection},
    state::store,
};

use super::{ensure_lease_owner_or_reclaim, require_runtime_task_context, tool_err};

pub const DESCRIPTION: &str = "Reject the current submission with review notes. Writes notes to REVIEW.md and \
     transitions state Reviewing -> Addressing (or Failed if the review cycle limit is \
     exhausted). The Executor's check retry counter is reset for the new cycle.";

pub const INPUT_SCHEMA: &str = r#"{
    "type": "object",
    "properties": {
        "notes": {
            "type": "string",
            "description": "Markdown-formatted review notes explaining what needs to change"
        }
    },
    "required": ["notes"]
}"#;

pub async fn handler(
    ctx: neva::di::Dc<crate::server::ServerContext>,
    notes: String,
) -> Result<String, Error> {
    handler_for_agent(ctx.agent_id(), notes).await
}

pub async fn handler_for_agent(agent_id: &str, notes: String) -> Result<String, Error> {
    run(agent_id, notes).await.map_err(tool_err)
}

async fn run(agent_id: &str, notes: String) -> Result<String> {
    let config = Config::load().await?;
    let context = require_runtime_task_context(agent_id).await?;

    if context.status.parse::<project::TaskStatus>()? != project::TaskStatus::Reviewing {
        anyhow::bail!(
            "Cannot reject from state {}. Call /review_pending first.",
            context.status
        );
    }
    ensure_lease_owner_or_reclaim(agent_id, config.lease.ttl_secs).await?;

    write_review(&context, &notes).await?;

    let rejection =
        project::record_task_review_rejected(&context.task_id, config.limits.max_review_cycles)
            .await?;
    crate::repository_graph_runtime::release_submitted_tree_pin_best_effort(&context).await;

    match rejection {
        TaskReviewRejection::Addressing { cycles } => {
            project::record_runtime_event_best_effort(
                context.run_id.clone(),
                "rejected",
                serde_json::json!({
                    "task_id": context.task_id.as_str(),
                    "review_cycles": cycles,
                    "max_review_cycles": config.limits.max_review_cycles,
                    "notes_bytes": notes.len(),
                }),
            )
            .await;
            info!(
                review_cycles = cycles,
                task_id = context.task_id,
                "Submission rejected, DB task -> addressing"
            );
            Ok(format!(
                "Submission rejected (cycle {}/{}).\n\n**Review notes written.** \
                 State: Addressing. The Executor should call /wait_for_task to see the notes \
                 and /check after addressing them.",
                cycles, config.limits.max_review_cycles,
            ))
        }
        TaskReviewRejection::LimitExceeded { cycles } => {
            project::record_runtime_event_best_effort(
                context.run_id.clone(),
                "review_limit_exceeded",
                serde_json::json!({
                    "task_id": context.task_id.as_str(),
                    "review_cycles": cycles,
                    "max_review_cycles": config.limits.max_review_cycles,
                    "notes_bytes": notes.len(),
                }),
            )
            .await;
            warn!(
                review_cycles = cycles,
                task_id = context.task_id,
                "Review cycle limit reached, DB task -> failed"
            );
            Ok(format!(
                "Review cycle limit reached ({cycles}/{}).\n\nState is now Failed. \
                 A human must call /reset to recover.",
                config.limits.max_review_cycles,
            ))
        }
    }
}

async fn write_review(context: &RuntimeTaskContext, notes: &str) -> Result<()> {
    store::write_review_for_run_dir(&context.run_dir, notes).await
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
            "[checks]\ncommands = []\n\n[limits]\nmax_check_retries = 20\nmax_review_cycles = 3\nmax_feedback_lines = 30\nwait_timeout_secs = 1\n\n[lease]\nttl_secs = 60\n",
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
    async fn reject_updates_agent_review_task_and_scoped_review_notes() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
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

        run("supervisor:codex:7", "fix this".to_string())
            .await
            .unwrap();

        crate::test_support::assert_no_state_json();
        assert_eq!(
            tokio::fs::read_to_string(".ferrus/runs/t-007/REVIEW.md")
                .await
                .unwrap(),
            "fix this"
        );
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "addressing");
        assert_eq!(task.review_cycles, 1);
        assert_eq!(task.check_retries, 0);
        assert_eq!(task.claimed_by, None);

        teardown(previous);
    }

    #[tokio::test]
    async fn reject_uses_database_context_when_state_json_is_absent() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
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

        run("supervisor:codex:7", "fix this".to_string())
            .await
            .unwrap();

        assert_eq!(
            tokio::fs::read_to_string(".ferrus/runs/t-007/REVIEW.md")
                .await
                .unwrap(),
            "fix this"
        );
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.status, "addressing");
        assert_eq!(task.review_cycles, 1);
        assert_eq!(task.claimed_by, None);

        teardown(previous);
    }

    #[tokio::test]
    async fn reject_resets_database_retry_counters() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup().await;
        crate::project::record_task_status(
            "t-001",
            ".ferrus/tasks/t-001.md",
            crate::project::TaskStatus::Reviewing,
        )
        .await
        .unwrap();
        crate::project::record_task_check_failed("t-001", "fmt failed", 3)
            .await
            .unwrap();
        crate::project::claim_task("t-001", ".ferrus/tasks/t-001.md", "supervisor:codex:1", 60)
            .await
            .unwrap();

        run("supervisor:codex:1", "fix this".to_string())
            .await
            .unwrap();

        crate::test_support::assert_no_state_json();
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-001").unwrap();
        assert_eq!(task.status, "addressing");
        assert_eq!(task.review_cycles, 1);
        assert_eq!(task.check_retries, 0);
        assert_eq!(task.claimed_by, None);

        teardown(previous);
    }
}
