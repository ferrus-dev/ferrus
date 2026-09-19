//! Managed authority failures must not change tasks, leases, runs, or events.

use super::*;
use crate::project::{LocalProjectRef, ProjectMetadata, TaskStatus};
use rusqlite::Connection;
use serde_json::Value;
use tempfile::TempDir;

const AGENT: &str = "executor:nano:1";
const TASK: &str = "t-001";
const RUN: &str = "nano-run-001";

struct Fixture {
    _dir: TempDir,
    previous: PathBuf,
    root: PathBuf,
    data: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.previous).unwrap();
    }
}

impl Fixture {
    async fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let data = root.join(".ferrus/projects/test-project");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(root.join(".ferrus/tasks")).unwrap();
        let fixture = Self {
            previous: std::env::current_dir().unwrap(),
            _dir: dir,
            root,
            data,
        };
        std::env::set_current_dir(&fixture.root).unwrap();
        let local_ref = LocalProjectRef {
            project_id: "test-project".into(),
            name: "test".into(),
            data_dir: fixture.data.to_string_lossy().into_owned(),
        };
        std::fs::write(".ferrus/project.toml", toml::to_string(&local_ref).unwrap()).unwrap();
        let metadata = ProjectMetadata {
            id: local_ref.project_id,
            name: local_ref.name,
            workspace_dir: fixture.root.to_string_lossy().into_owned(),
            ferrus_dir: fixture.root.join(".ferrus").to_string_lossy().into_owned(),
            vcs: None,
            origin_repo: None,
            default_branch: None,
            current_head: None,
            created_at: "2026-09-07T00:00:00Z".into(),
            last_opened_at: "2026-09-07T00:00:00Z".into(),
            version: 1,
        };
        std::fs::write(
            fixture.data.join("project.toml"),
            toml::to_string(&metadata).unwrap(),
        )
        .unwrap();
        std::fs::write(
            "ferrus.toml",
            "[checks]\ncommands = []\n[limits]\nmax_check_retries = 20\nmax_review_cycles = 3\nmax_feedback_lines = 30\nwait_timeout_secs = 1\n[lease]\nttl_secs = 60\n",
        )
        .unwrap();
        std::fs::write(".ferrus/tasks/t-001.md", "Implement the fixture task.\n").unwrap();
        project::record_task_status(TASK, ".ferrus/tasks/t-001.md", TaskStatus::Pending)
            .await
            .unwrap();
        project::record_run_started_for_task_with_workspace(
            RUN,
            "executor",
            AGENT,
            std::process::id(),
            Some(TASK),
            fixture.root.to_string_lossy().into_owned(),
        )
        .await
        .unwrap();
        fixture
    }

    fn launch(&self) -> LaunchContext {
        LaunchContext {
            project_root: self.root.clone(),
            workspace: self.root.clone(),
            agent_id: AGENT.into(),
            task_id: TASK.into(),
            run_id: RUN.into(),
            baseline_tree: None,
        }
    }

    fn connection(&self) -> Connection {
        Connection::open(self.data.join("ferrus.db")).unwrap()
    }

    fn events(&self) -> Vec<(String, Value)> {
        let connection = self.connection();
        connection
            .prepare("SELECT type, payload_json FROM events ORDER BY id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get::<_, String>(1)?)))
            .unwrap()
            .map(|row| {
                let (kind, body) = row.unwrap();
                (kind, serde_json::from_str(&body).unwrap())
            })
            .collect()
    }

    async fn assert_no_effect(
        &self,
        tasks: Vec<project::TaskRecord>,
        events: Vec<(String, Value)>,
    ) {
        assert_eq!(project::list_tasks().await.unwrap(), tasks);
        assert_eq!(self.events(), events);
        crate::test_support::assert_no_state_json();
    }
}

#[test]
fn launch_capture_requires_host_identity() {
    let env = [
        (ENV_PROJECT_ROOT, "/project"),
        (ENV_AGENT_ID, AGENT),
        (ENV_TASK_ID, TASK),
        (ENV_RUN_ID, RUN),
    ];
    for missing in [ENV_PROJECT_ROOT, ENV_AGENT_ID, ENV_TASK_ID, ENV_RUN_ID] {
        let result = LaunchContext::capture(
            |key| {
                env.iter()
                    .find(|(name, _)| *name == key && key != missing)
                    .map(|(_, value)| value.to_string())
            },
            PathBuf::from("/workspace"),
        );
        assert!(result.unwrap_err().to_string().contains(missing));
    }
    let context = LaunchContext::capture(
        |key| {
            env.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.to_string())
        },
        PathBuf::from("/workspace"),
    )
    .unwrap();
    assert_eq!(context.run_id, RUN);
    assert!(context.baseline_tree.is_none());
}

#[tokio::test]
async fn native_claim_status_and_heartbeat_match_mcp() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let fixture = Fixture::new().await;
    let events = fixture.events();
    let session = FerrusSession::bind(fixture.launch()).await.unwrap();
    assert_eq!(fixture.events(), events, "binding must be read-only");
    assert_eq!(session.project_id(), "test-project");
    assert_eq!(session.project_root(), fixture.root);
    assert_eq!(session.baseline_tree(), None);
    assert!(matches!(
        session.claim().await.unwrap(),
        ReadyTaskClaim::Claimed(_)
    ));
    let existing = match session.claim().await.unwrap() {
        ReadyTaskClaim::AlreadyClaimed(lease) => lease,
        other => panic!("expected existing claim: {other:?}"),
    };
    let mcp: Value = serde_json::from_str(
        &crate::server::tools::wait_for_task::handler_for_agent(AGENT)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(mcp["task_id"], existing.task_id);
    assert_eq!(mcp["task_path"], existing.task_path);
    assert_eq!(mcp["claimed_by"], existing.claimed_by);
    assert_eq!(
        mcp["lease_until"],
        serde_json::to_value(existing.lease_until).unwrap()
    );
    assert_eq!(mcp["state"], "Executing");
    let events = fixture.events();
    let status = session.status().await.unwrap();
    let mcp = crate::server::tools::status::handler_for_agent(AGENT)
        .await
        .unwrap();
    for line in [
        format!("**Task:** {}", status.task_id),
        format!("**State:** {}", status.status),
        format!("**Run:** {}", status.run_id.unwrap()),
        format!("**Workspace:** {}", status.workspace_path.unwrap()),
        format!("**Run dir:** {}", status.run_dir),
    ] {
        assert!(mcp.contains(&line), "{line}");
    }
    assert_eq!(fixture.events(), events);
    assert!(
        matches!(session.heartbeat().await.unwrap(), LeaseRenewal::Renewed { task_id, .. } if task_id == TASK)
    );
    let mcp: Value = serde_json::from_str(
        &crate::server::tools::heartbeat::handler_for_agent(AGENT)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(mcp["status"], "renewed");
    assert_eq!(mcp["task_id"], TASK);
    let events = fixture.events();
    assert_eq!(
        events
            .iter()
            .filter(|(kind, _)| kind == "task_claimed")
            .count(),
        1
    );
    let renewals: Vec<_> = events
        .iter()
        .filter(|(kind, _)| kind == "task_lease_renewed")
        .collect();
    assert_eq!(renewals.len(), 2);
    for (_, payload) in renewals {
        assert_eq!(payload["task_id"], TASK);
        assert_eq!(payload["claimed_by"], AGENT);
    }
    crate::test_support::assert_no_state_json();
}

#[tokio::test]
async fn binding_rejects_missing_or_mismatched_context_without_effects() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let fixture = Fixture::new().await;
    let tasks = project::list_tasks().await.unwrap();
    let events = fixture.events();
    for field in ["agent", "task", "run", "workspace", "project", "baseline"] {
        let mut launch = fixture.launch();
        match field {
            "agent" => launch.agent_id = "executor:nano:2".into(),
            "task" => launch.task_id = "t-other".into(),
            "run" => launch.run_id = "missing-run".into(),
            "workspace" => {
                let other = fixture.root.join("other");
                std::fs::create_dir_all(other.join(".ferrus")).unwrap();
                std::fs::copy(
                    fixture.root.join(".ferrus/project.toml"),
                    other.join(".ferrus/project.toml"),
                )
                .unwrap();
                launch.workspace = other;
            }
            "project" => launch.project_root = fixture.data.clone(),
            "baseline" => launch.baseline_tree = Some("unrecorded-tree".into()),
            _ => unreachable!(),
        }
        assert!(FerrusSession::bind(launch).await.is_err(), "{field}");
    }
    fixture.assert_no_effect(tasks, events).await;
}

#[tokio::test]
async fn every_operation_revalidates_the_bound_run() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let fixture = Fixture::new().await;
    let session = FerrusSession::bind(fixture.launch()).await.unwrap();
    session.claim().await.unwrap();
    project::record_task_status(
        "other-task",
        ".ferrus/tasks/other-task.md",
        TaskStatus::Pending,
    )
    .await
    .unwrap();
    let tasks = project::list_tasks().await.unwrap();
    let events = fixture.events();
    for (column, value, original) in [
        ("role", "supervisor", "executor"),
        ("agent", "executor:nano:2", AGENT),
        ("task_id", "other-task", TASK),
        ("status", "exited", "running"),
        (
            "workspace_path",
            fixture.data.to_str().unwrap(),
            fixture.root.to_str().unwrap(),
        ),
    ] {
        // Only fixed fixture column names are interpolated; values remain bound parameters.
        let sql = format!("UPDATE runs SET {column} = ?1 WHERE id = ?2");
        fixture.connection().execute(&sql, [value, RUN]).unwrap();
        assert!(session.claim().await.is_err(), "claim: {column}");
        assert!(session.heartbeat().await.is_err(), "heartbeat: {column}");
        assert!(session.status().await.is_err(), "status: {column}");
        assert!(
            FerrusSession::bind(fixture.launch()).await.is_err(),
            "bind: {column}"
        );
        fixture.connection().execute(&sql, [original, RUN]).unwrap();
    }
    fixture.assert_no_effect(tasks, events).await;
}

#[tokio::test]
async fn worktree_binding_uses_explicit_paths_and_requires_its_baseline() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let fixture = Fixture::new().await;
    let workspace = fixture.data.join("worktrees/t-001");
    std::fs::create_dir_all(workspace.join(".ferrus")).unwrap();
    std::fs::copy(
        fixture.root.join(".ferrus/project.toml"),
        workspace.join(".ferrus/project.toml"),
    )
    .unwrap();
    fixture
        .connection()
        .execute(
            "UPDATE runs SET workspace_path = ?1 WHERE id = ?2",
            [workspace.to_str().unwrap(), RUN],
        )
        .unwrap();
    let mut launch = fixture.launch();
    launch.workspace = workspace.clone();
    assert!(FerrusSession::bind(launch.clone()).await.is_err());
    let baseline_path = fixture.data.join("worktrees/.baseline-trees/t-001.txt");
    std::fs::create_dir_all(baseline_path.parent().unwrap()).unwrap();
    std::fs::write(&baseline_path, "tree-a\n").unwrap();
    launch.baseline_tree = Some("tree-a".into());
    // Neither binding nor later effects may depend on ambient project resolution.
    std::env::set_current_dir(&fixture.data).unwrap();
    let session = FerrusSession::bind(launch).await.unwrap();
    assert!(matches!(
        session.claim().await.unwrap(),
        ReadyTaskClaim::Claimed(_)
    ));
    let context = session.status().await.unwrap();
    assert_eq!(context.workspace_path.as_deref(), workspace.to_str());
    assert_eq!(context.task_path, ".ferrus/tasks/t-001.md");
    assert_eq!(context.run_dir, ".ferrus/runs/t-001");
    assert!(matches!(
        session.heartbeat().await.unwrap(),
        LeaseRenewal::Renewed { .. }
    ));
}

#[tokio::test]
async fn heartbeat_stops_at_handoff_and_claim_accepts_addressing_only_when_ready() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let fixture = Fixture::new().await;
    let session = FerrusSession::bind(fixture.launch()).await.unwrap();
    session.claim().await.unwrap();
    fixture
        .connection()
        .execute(
            "UPDATE tasks SET status = 'reviewing' WHERE id = ?1",
            [TASK],
        )
        .unwrap();
    let tasks = project::list_tasks().await.unwrap();
    let events = fixture.events();
    assert_eq!(session.status().await.unwrap().status, "reviewing");
    assert!(session.heartbeat().await.is_err());
    assert!(matches!(
        session.claim().await.unwrap(),
        ReadyTaskClaim::NoAvailable
    ));
    fixture.assert_no_effect(tasks, events).await;
    fixture.connection().execute("UPDATE tasks SET status = 'addressing', check_retries = 2, review_cycles = 1 WHERE id = ?1", [TASK]).unwrap();
    match session.claim().await.unwrap() {
        ReadyTaskClaim::AlreadyClaimed(lease) => {
            assert_eq!(lease.status, "addressing");
            assert_eq!(lease.check_retries, 2);
            assert_eq!(lease.review_cycles, 1);
        }
        other => panic!("unexpected claim: {other:?}"),
    }
    assert!(matches!(
        session.heartbeat().await.unwrap(),
        LeaseRenewal::Renewed { .. }
    ));
}

#[tokio::test]
async fn native_operations_cannot_take_or_renew_another_agents_live_lease() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let fixture = Fixture::new().await;
    let session = FerrusSession::bind(fixture.launch()).await.unwrap();
    project::claim_task(TASK, ".ferrus/tasks/t-001.md", "executor:codex:2", 60)
        .await
        .unwrap();
    let tasks = project::list_tasks().await.unwrap();
    let events = fixture.events();
    assert!(matches!(
        session.claim().await.unwrap(),
        ReadyTaskClaim::NoAvailable
    ));
    assert!(matches!(
        session.heartbeat().await.unwrap(),
        LeaseRenewal::NotClaimed
    ));
    // The same claim primitive must also leave another owner's pending task untouched.
    assert!(matches!(
        project::claim_ready_task_by_id(TASK, AGENT, 60)
            .await
            .unwrap(),
        ReadyTaskClaim::NoAvailable
    ));
    fixture.assert_no_effect(tasks, events).await;
}

#[tokio::test]
async fn explicit_scope_never_follows_another_task_or_latest_run_for_the_agent() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let fixture = Fixture::new().await;
    let session = FerrusSession::bind(fixture.launch()).await.unwrap();
    session.claim().await.unwrap();
    project::record_task_status("t-002", ".ferrus/tasks/t-002.md", TaskStatus::Executing)
        .await
        .unwrap();
    project::claim_task("t-002", ".ferrus/tasks/t-002.md", AGENT, 3600)
        .await
        .unwrap();
    project::record_run_started_for_task_with_workspace(
        "later-run",
        "executor",
        AGENT,
        std::process::id(),
        Some("t-002"),
        fixture.root.to_string_lossy().into_owned(),
    )
    .await
    .unwrap();
    let other = project::list_tasks()
        .await
        .unwrap()
        .into_iter()
        .find(|task| task.id == "t-002")
        .unwrap();
    assert_eq!(session.status().await.unwrap().run_id.as_deref(), Some(RUN));
    assert!(
        matches!(session.heartbeat().await.unwrap(), LeaseRenewal::Renewed { task_id, .. } if task_id == TASK)
    );
    assert_eq!(
        project::list_tasks()
            .await
            .unwrap()
            .into_iter()
            .find(|task| task.id == "t-002")
            .unwrap(),
        other
    );
    fixture
        .connection()
        .execute(
            "UPDATE tasks SET lease_until = '2000-01-01T00:00:00Z' WHERE id = ?1",
            [TASK],
        )
        .unwrap();
    let events = fixture.events();
    assert!(matches!(
        session.heartbeat().await.unwrap(),
        LeaseRenewal::Expired
    ));
    assert_eq!(fixture.events(), events);
    assert!(matches!(
        session.claim().await.unwrap(),
        ReadyTaskClaim::Claimed(_)
    ));
}

#[tokio::test]
async fn baseline_is_bound_independently_of_optional_graph_state() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let fixture = Fixture::new().await;
    let baseline_path = fixture.data.join("worktrees/.baseline-trees/t-001.txt");
    std::fs::create_dir_all(baseline_path.parent().unwrap()).unwrap();
    std::fs::write(&baseline_path, "tree-a\n").unwrap();
    let mut launch = fixture.launch();
    assert!(FerrusSession::bind(launch.clone()).await.is_err());
    launch.baseline_tree = Some("tree-a".into());
    let session = FerrusSession::bind(launch).await.unwrap();
    assert_eq!(session.baseline_tree(), Some("tree-a"));
    session.claim().await.unwrap();
    let tasks = project::list_tasks().await.unwrap();
    let events = fixture.events();
    std::fs::write(&baseline_path, "tree-b\n").unwrap();
    assert!(session.claim().await.is_err());
    assert!(session.heartbeat().await.is_err());
    assert!(session.status().await.is_err());
    fixture.assert_no_effect(tasks, events).await;
}

#[tokio::test]
async fn missing_database_is_not_created_and_old_schema_is_not_migrated() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let fixture = Fixture::new().await;
    let session = FerrusSession::bind(fixture.launch()).await.unwrap();
    let database = fixture.data.join("ferrus.db");
    let saved = fixture.data.join("saved.db");
    std::fs::rename(&database, &saved).unwrap();
    assert!(session.status().await.is_err());
    assert!(session.claim().await.is_err());
    assert!(session.heartbeat().await.is_err());
    assert!(!database.exists());
    std::fs::rename(&saved, &database).unwrap();
    let events = fixture.events();
    fixture
        .connection()
        .pragma_update(None, "user_version", 0)
        .unwrap();
    assert!(session.status().await.is_err());
    assert!(session.claim().await.is_err());
    assert!(session.heartbeat().await.is_err());
    let version: u32 = fixture
        .connection()
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 0);
    assert_eq!(fixture.events(), events);
}

#[path = "context_tests.rs"]
mod context_tests;

#[path = "lifecycle_tests.rs"]
mod lifecycle_tests;
