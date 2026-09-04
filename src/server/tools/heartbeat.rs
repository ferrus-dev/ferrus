//! Renew the calling agent's task lease and return a structured ownership result.

use anyhow::Result;
use neva::prelude::*;
use serde_json::json;
use tracing::info;

use crate::{
    config::Config,
    project::{self, LeaseRenewal},
};

use super::tool_err;

pub const DESCRIPTION: &str = "Renew the task lease for the calling agent. Validates that the server-scoped agent identity holds a runtime lease, \
     then extends lease_until by ttl_secs and updates last_heartbeat. \
     Returns a JSON object: {\"status\":\"renewed\", \"task_id\":\"...\", \"task_path\":\"...\", \"claimed_by\":\"...\", \"lease_until\":\"...\"} \
     on success, or {\"status\":\"error\", \"code\":\"...\", \"message\":\"...\"} on failure. \
     Error codes: not_claimed (no active lease), claimed_by_other (different agent holds lease), \
     expired (your lease timed out before renewal), invalid_state (state cannot be leased).";

pub async fn handler(ctx: neva::di::Dc<crate::server::ServerContext>) -> Result<String, Error> {
    handler_for_agent(ctx.agent_id()).await
}

pub async fn handler_for_agent(agent_id: &str) -> Result<String, Error> {
    run(agent_id).await.map_err(tool_err)
}

async fn run(agent_id: &str) -> Result<String> {
    let config = Config::load().await?;
    let ttl_secs = config.lease.ttl_secs;

    let db_renewal = project::renew_claimed_task_lease(agent_id, ttl_secs).await;

    match db_renewal {
        Ok(LeaseRenewal::Renewed {
            task_id,
            task_path,
            claimed_by,
            lease_until,
        }) => {
            info!(agent_id, task_id, "Lease renewed");
            Ok(json!({
                "status": "renewed",
                "task_id": task_id,
                "task_path": task_path,
                "claimed_by": claimed_by,
                "lease_until": lease_until,
            })
            .to_string())
        }
        Ok(LeaseRenewal::NotClaimed) => Ok(json!({
            "status": "error",
            "code": "not_claimed",
            "message": "No active lease exists"
        })
        .to_string()),
        Ok(LeaseRenewal::Expired) => Ok(json!({
            "status": "error",
            "code": "expired",
            "message": "Your lease expired before renewal"
        })
        .to_string()),
        Err(err) => {
            tracing::warn!(error = ?err, "failed to renew lease in ferrus.db");
            Ok(json!({
                "status": "error",
                "code": "not_claimed",
                "message": "No active lease exists"
            })
            .to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    //! Lease renewal resolves task ownership from SQLite context.

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
    async fn heartbeat_uses_database_context_when_state_json_is_absent() {
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

        let response: serde_json::Value =
            serde_json::from_str(&run("executor:codex:7").await.unwrap()).unwrap();

        assert_eq!(response["status"], "renewed");
        assert_eq!(response["task_id"], "t-007");
        crate::test_support::assert_no_state_json();
        let tasks = crate::project::list_tasks().await.unwrap();
        let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
        assert_eq!(task.claimed_by.as_deref(), Some("executor:codex:7"));
        assert!(task.lease_until.is_some());
        assert!(task.last_heartbeat.is_some());

        teardown(previous);
    }
}
