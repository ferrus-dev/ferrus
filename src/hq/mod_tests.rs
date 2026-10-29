//! HQ tests for scheduling, worktree isolation, recovery, and paused-session resumption.

use super::*;

fn task_record(id: &str, status: crate::project::TaskStatus) -> TaskRecord {
    TaskRecord {
        id: id.to_string(),
        path: format!(".ferrus/tasks/{id}.md"),
        spec_path: None,
        milestone_id: None,
        status: status.as_str().to_string(),
        paused_status: None,
        claimed_by: None,
        lease_until: None,
        last_heartbeat: None,
        check_retries: 0,
        review_cycles: 0,
        failure_reason: None,
    }
}

#[cfg(unix)]
fn failed_exit_status() -> std::process::ExitStatus {
    use std::os::unix::process::ExitStatusExt;

    std::process::ExitStatus::from_raw(1 << 8)
}

#[cfg(windows)]
fn failed_exit_status() -> std::process::ExitStatus {
    use std::os::windows::process::ExitStatusExt;

    std::process::ExitStatus::from_raw(1)
}

#[path = "mod_tests/behavior.rs"]
mod behavior_tests;
#[path = "mod_tests/consultation.rs"]
mod consultation_tests;
#[path = "mod_tests/human_waits.rs"]
mod human_waits_tests;
#[path = "mod_tests/input.rs"]
mod input_tests;
#[path = "mod_tests/terminal.rs"]
mod terminal_tests;
#[path = "mod_tests/workspaces.rs"]
mod workspaces_tests;

#[test]
fn run_plan_lines_do_not_report_batch_launch_as_unwired() {
    let plan = RunPlan {
        spec_path: "docs/specs/spec.md".to_string(),
        eligible: vec![RunPlanMilestone {
            id: "m1.0".to_string(),
            marker: "#1.0".to_string(),
            title: "First task".to_string(),
        }],
        skipped: Vec::new(),
    };

    let lines = run_plan_lines(&plan, 1).join("\n");

    assert!(!lines.contains("not wired"));
    assert!(lines.contains("selected  : 1"));
}

#[test]
fn format_agent_details_normalizes_version_and_appends_model() {
    assert_eq!(
        format_agent_details(
            "claude-code",
            "2.1.143 (Claude Code)",
            AgentDisplayConfig {
                model: Some("claude-opus-4-6".to_string()),
                effort: Some("high".to_string()),
            }
        ),
        "2.1.143 (claude-opus-4-6, effort: high)"
    );
    assert_eq!(
        format_agent_details(
            "codex",
            "codex-cli 0.132.0",
            AgentDisplayConfig {
                model: Some("gpt-5.4".to_string()),
                effort: None,
            }
        ),
        "0.132.0 (gpt-5.4)"
    );
}

#[test]
fn archive_prompt_skips_failed_attempts_without_discarding_history_count() {
    let tasks = [
        task_record("t-001", crate::project::TaskStatus::Failed),
        task_record("t-002", crate::project::TaskStatus::Complete),
    ];
    let prompt = archive_spec_prompt_context("docs/spec.md", &tasks);
    assert!(prompt.contains("Task count: 2"));
    assert!(prompt.contains("Failed attempts skipped for Outcome: 1"));
    assert!(!prompt.contains("t-001"));
    assert!(prompt.contains("Task ID: t-002"));
    assert!(prompt.contains("Task path: .ferrus/tasks/t-002.md"));
}

#[test]
fn interactive_exit_error_names_role_agent_and_status() {
    let message = interactive_exit_error(
        ROLE_SUPERVISOR,
        "codex",
        failed_exit_status(),
        "broken config",
    );

    assert!(message.contains("supervisor agent (codex) exited with"));
    assert!(message.contains("stderr:\nbroken config"));
}

#[test]
fn format_agent_details_omits_missing_parts() {
    assert_eq!(
        format_agent_details("opencode", "opencode 0.6.0", AgentDisplayConfig::default()),
        "0.6.0"
    );
    assert_eq!(
        format_agent_details(
            "goose",
            "",
            AgentDisplayConfig {
                model: Some("qwen3-coder".to_string()),
                effort: Some("medium".to_string()),
            }
        ),
        "(qwen3-coder, effort: medium)"
    );
    assert_eq!(
        format_agent_details(
            "claude-code",
            "",
            AgentDisplayConfig {
                model: None,
                effort: Some("high".to_string()),
            }
        ),
        "(effort: high)"
    );
    assert_eq!(
        format_agent_details("qwen-code", "", AgentDisplayConfig::default()),
        ""
    );
}

#[tokio::test]
async fn reconcile_runtime_schedule_does_not_require_state_json() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let previous = std::env::current_dir().unwrap();
    std::fs::create_dir_all(dir.path().join(".ferrus")).unwrap();
    std::env::set_current_dir(dir.path()).unwrap();

    let data_dir = dir.path().join("runtime");
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
        "t-001",
        ".ferrus/tasks/t-001.md",
        crate::project::TaskStatus::Complete,
    )
    .await
    .unwrap();

    let (_state_tx, state_rx) = watch::channel::<Option<WatchedState>>(None);
    let (msg_tx, _msg_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut ctx = HqContext::new(state_rx, Display(msg_tx), false);
    ctx.seed_completed_task_announcements().await.unwrap();

    ctx.reconcile_runtime_schedule().await.unwrap();

    crate::test_support::assert_no_state_json();
    std::env::set_current_dir(previous).unwrap();
}

#[tokio::test]
async fn executor_parallel_limit_caps_non_git_projects() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let previous = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();

    let data_dir = dir.path().join("runtime");
    tokio::fs::create_dir_all(&data_dir).await.unwrap();
    tokio::fs::create_dir_all(".ferrus").await.unwrap();
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
        created_at: "2026-01-01T00:00:00Z".to_string(),
        last_opened_at: "2026-01-01T00:00:00Z".to_string(),
        version: 1,
    };
    tokio::fs::write(
        data_dir.join("project.toml"),
        toml::to_string_pretty(&metadata).unwrap(),
    )
    .await
    .unwrap();

    assert_eq!(executor_parallel_limit(4).await.unwrap(), 1);

    std::env::set_current_dir(previous).unwrap();
}

#[test]
fn executor_spawn_selection_skips_live_tasks_before_slot_limit() {
    let tasks = vec![
        task_record("t-001", crate::project::TaskStatus::Pending),
        task_record("t-002", crate::project::TaskStatus::Pending),
    ];

    let selected = select_executor_spawn_tasks(&tasks, 1, |task| task.id == "t-001");

    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].id, "t-002");
}

#[test]
fn task_claim_blocks_live_expected_or_active_external_agent() {
    let now = chrono::Utc::now();
    let mut task = task_record("t-001", crate::project::TaskStatus::Executing);
    task.claimed_by = Some("executor:codex:t-001".to_string());
    task.lease_until = Some((now + chrono::Duration::minutes(1)).to_rfc3339());
    let mut live_run_task_ids = HashSet::new();

    assert!(!task_claim_blocks_spawn(
        &task,
        "executor:codex:t-001",
        now,
        &live_run_task_ids,
    ));
    assert!(task_claim_blocks_spawn(
        &task,
        "executor:claude-code:t-001",
        now,
        &live_run_task_ids,
    ));

    live_run_task_ids.insert("t-001".to_string());
    assert!(task_claim_blocks_spawn(
        &task,
        "executor:codex:t-001",
        now,
        &live_run_task_ids,
    ));

    task.lease_until = Some((now - chrono::Duration::seconds(1)).to_rfc3339());
    assert!(task_claim_blocks_spawn(
        &task,
        "executor:codex:t-001",
        now,
        &live_run_task_ids,
    ));
    assert!(task_claim_blocks_spawn(
        &task,
        "executor:claude-code:t-001",
        now,
        &live_run_task_ids,
    ));

    live_run_task_ids.clear();
    assert!(!task_claim_blocks_spawn(
        &task,
        "executor:claude-code:t-001",
        now,
        &live_run_task_ids,
    ));
}

#[test]
fn task_claim_blocks_live_unclaimed_run() {
    let now = chrono::Utc::now();
    let task = task_record("t-001", crate::project::TaskStatus::Pending);
    let live_run_task_ids = HashSet::from(["t-001".to_string()]);

    assert!(task_claim_blocks_spawn(
        &task,
        "executor:codex:t-001",
        now,
        &live_run_task_ids,
    ));
}

#[tokio::test]
async fn human_answer_for_dead_waiter_is_not_restored_before_delivery() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let previous = std::env::current_dir().unwrap();
    std::fs::create_dir_all(dir.path().join(".ferrus")).unwrap();
    std::env::set_current_dir(dir.path()).unwrap();

    let data_dir = dir.path().join("runtime");
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
        "t-011",
        ".ferrus/tasks/t-011.md",
        crate::project::TaskStatus::Executing,
    )
    .await
    .unwrap();
    crate::project::claim_task(
        "t-011",
        ".ferrus/tasks/t-011.md",
        "executor:codex:t-011",
        60,
    )
    .await
    .unwrap();
    crate::project::record_task_human_question_requested(
        "t-011",
        crate::project::TaskStatus::Executing,
        "executor:codex:t-011",
    )
    .await
    .unwrap();
    store::write_question_for_run_dir(".ferrus/runs/t-011", "Which branch?")
        .await
        .unwrap();

    let (_state_tx, state_rx) = watch::channel::<Option<WatchedState>>(None);
    let (msg_tx, _msg_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut ctx = HqContext::new(state_rx, Display(msg_tx), false);

    let error = ctx
        .answer_scoped_human_question("Use the short branch.".to_string())
        .await
        .unwrap_err()
        .to_string();

    assert!(error.contains("Executor agent is not configured"));
    let task = crate::project::list_tasks()
        .await
        .unwrap()
        .into_iter()
        .find(|task| task.id == "t-011")
        .unwrap();
    assert_eq!(task.status, "awaiting_human");
    assert_eq!(
        store::read_answer_for_run_dir(".ferrus/runs/t-011")
            .await
            .unwrap(),
        "Use the short branch."
    );
    assert_eq!(
        store::read_question_for_run_dir(".ferrus/runs/t-011")
            .await
            .unwrap(),
        "Which branch?"
    );

    std::env::set_current_dir(previous).unwrap();
}
