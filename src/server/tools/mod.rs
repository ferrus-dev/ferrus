pub mod answer;
pub mod approve;
pub mod archive_spec;
pub mod ask_human;
pub mod check;
pub mod check_gate;
pub mod consult;
pub mod create_spec;
pub mod create_task;
pub mod enqueue_task;
pub mod heartbeat;
pub mod reject;
pub mod repository_graph_status;
pub mod repository_search;
pub mod reset;
pub mod respond_consult;
pub mod review_pending;
pub mod status;
pub mod submit;
pub mod wait_for_answer;
pub mod wait_for_consult;
pub mod wait_for_consultation;
pub mod wait_for_review;
pub mod wait_for_task;

use neva::prelude::*;

use crate::project::{self, RuntimeTaskContext, TaskClaim};

/// Convert an [`anyhow::Error`] into a neva tool error.
pub(super) fn tool_err(e: anyhow::Error) -> Error {
    Error::new(
        ErrorCode::InternalError,
        std::io::Error::other(e.to_string()),
    )
}

pub(super) async fn ensure_lease_owner_or_reclaim(
    agent_id: &str,
    ttl_secs: u64,
) -> anyhow::Result<()> {
    let context = require_runtime_task_context(agent_id).await?;
    match project::claim_task(&context.task_id, &context.task_path, agent_id, ttl_secs).await? {
        TaskClaim::Claimed | TaskClaim::AlreadyClaimed => Ok(()),
        TaskClaim::ClaimedByOther { claimed_by } => {
            anyhow::bail!("Cannot modify task: lease is held by {claimed_by}, not {agent_id}");
        }
    }
}

pub(super) async fn runtime_task_context_for_agent_best_effort(
    agent_id: &str,
) -> Option<RuntimeTaskContext> {
    match project::runtime_task_context_for_agent(agent_id).await {
        Ok(context) => context,
        Err(err) => {
            tracing::warn!(
                error = ?err,
                agent_id,
                "failed to resolve runtime task context from ferrus.db"
            );
            None
        }
    }
}

pub(super) async fn require_runtime_task_context(
    agent_id: &str,
) -> anyhow::Result<RuntimeTaskContext> {
    project::runtime_task_context_for_agent(agent_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("No SQLite runtime task is assigned to {agent_id}. Call the appropriate wait tool first."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn setup_runtime_project() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::fs::create_dir_all(dir.path().join(".ferrus")).unwrap();
        let data_dir = dir.path().join(".ferrus/projects/test-project");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
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
        let metadata = crate::project::ProjectMetadata {
            id: "test-project".to_string(),
            name: "test".to_string(),
            workspace_dir: dir.path().to_string_lossy().into_owned(),
            ferrus_dir: dir.path().join(".ferrus").to_string_lossy().into_owned(),
            vcs: None,
            origin_repo: None,
            default_branch: None,
            current_head: None,
            created_at: "2026-07-18T00:00:00Z".to_string(),
            last_opened_at: "2026-07-18T00:00:00Z".to_string(),
            version: 1,
        };
        tokio::fs::write(
            data_dir.join("project.toml"),
            toml::to_string_pretty(&metadata).unwrap(),
        )
        .await
        .unwrap();
        (dir, previous)
    }

    fn teardown(previous: std::path::PathBuf) {
        std::env::set_current_dir(previous).unwrap();
    }

    #[test]
    fn input_schemas_are_object_schemas() {
        for (name, schema) in [
            ("answer", answer::INPUT_SCHEMA),
            ("archive_spec", archive_spec::INPUT_SCHEMA),
            ("ask_human", ask_human::INPUT_SCHEMA),
            ("consult", consult::INPUT_SCHEMA),
            ("create_spec", create_spec::INPUT_SCHEMA),
            ("create_task", create_task::INPUT_SCHEMA),
            ("enqueue_task", enqueue_task::INPUT_SCHEMA),
            ("reject", reject::INPUT_SCHEMA),
            ("repository_search", repository_search::INPUT_SCHEMA),
            ("respond_consult", respond_consult::INPUT_SCHEMA),
            ("submit", submit::INPUT_SCHEMA),
        ] {
            let schema: serde_json::Value =
                serde_json::from_str(schema).unwrap_or_else(|err| panic!("{name}: {err}"));
            assert_eq!(
                schema.get("type").and_then(serde_json::Value::as_str),
                Some("object"),
                "{name} input schema must declare root type object"
            );
        }
    }

    #[tokio::test]
    async fn lease_owner_check_accepts_agent_database_task_context() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_runtime_project().await;
        crate::project::record_task_status(
            "t-002",
            ".ferrus/tasks/t-002.md",
            crate::project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        crate::project::claim_task("t-002", ".ferrus/tasks/t-002.md", "executor:codex:2", 60)
            .await
            .unwrap();
        ensure_lease_owner_or_reclaim("executor:codex:2", 60)
            .await
            .unwrap();

        teardown(previous);
    }

    #[tokio::test]
    async fn repository_reads_require_no_task_lease_and_do_not_touch_runtime_state() {
        let _guard = crate::test_support::cwd_lock().lock().unwrap();
        let (_dir, previous) = setup_runtime_project().await;
        tokio::fs::write("ferrus.toml", "[repository_graph]\nenabled = true\n")
            .await
            .unwrap();
        let data_dir = crate::project::current_project_data_dir().await.unwrap();
        let runtime_db = data_dir.join("ferrus.db");
        let sentinel = b"not a runtime database; repository reads must ignore it";
        tokio::fs::write(&runtime_db, sentinel).await.unwrap();

        let status = repository_graph_status::run().await.unwrap();
        let search = repository_search::handler(serde_json::json!({
            "query": "RuntimeTaskContext",
            "max_results": 5
        }))
        .await
        .unwrap();

        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&status).unwrap()["data"]["availability"],
            "not_built"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&search).unwrap()["code"],
            "not_built"
        );
        assert_eq!(tokio::fs::read(&runtime_db).await.unwrap(), sentinel);
        assert!(!data_dir.join("repo-graph.db").exists());
        assert!(!std::path::Path::new(".ferrus/tasks").exists());

        teardown(previous);
    }
}
