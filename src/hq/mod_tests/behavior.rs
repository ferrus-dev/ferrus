//! Tests for behavior.

use super::*;

#[tokio::test]
async fn run_plan_selects_ready_milestones_and_skips_existing_tasks() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let previous = std::env::current_dir().unwrap();
    std::fs::create_dir_all(dir.path().join(".ferrus")).unwrap();
    std::fs::create_dir_all(dir.path().join("docs/specs")).unwrap();
    std::env::set_current_dir(dir.path()).unwrap();

    let data_dir = dir.path().join("runtime");
    tokio::fs::create_dir_all(&data_dir).await.unwrap();
    let local_ref = crate::project::LocalProjectRef {
        project_id: "test-project".to_string(),
        name: "test".to_string(),
        data_dir: data_dir.to_string_lossy().into_owned(),
    };
    let local_ref_toml = toml::to_string_pretty(&local_ref).unwrap();
    tokio::fs::write(".ferrus/project.toml", local_ref_toml)
        .await
        .unwrap();
    let spec_path = "docs/specs/spec.md";
    tokio::fs::write(
        spec_path,
        "## Milestones\n\
         - [x] #1.0 Foundation\n\
           - ID: m1.0\n\
           - Depends on: none\n\n\
         - [ ] #1.1 Ready one\n\
           - ID: m1.1\n\
           - Depends on: m1.0\n\n\
         - [ ] #1.2 Already queued\n\
           - ID: m1.2\n\
           - Depends on: m1.0\n\n\
         - [ ] #2.0 Blocked\n\
           - ID: m2.0\n\
           - Depends on: m1.1\n",
    )
    .await
    .unwrap();
    crate::project::record_task_status_with_origin(
        "t-002",
        ".ferrus/tasks/t-002.md",
        crate::project::TaskStatus::Pending,
        Some(spec_path),
        Some("m1.2"),
    )
    .await
    .unwrap();

    let plan = build_run_plan(spec_path).await.unwrap();

    assert_eq!(plan.eligible.len(), 1);
    assert_eq!(plan.eligible[0].id, "m1.1");
    assert!(plan.skipped.iter().any(|milestone| {
        milestone.id == "m1.2" && milestone.reason == "task t-002 is pending"
    }));
    assert!(
        plan.skipped
            .iter()
            .any(|milestone| milestone.id == "m2.0" && milestone.reason == "waiting for m1.1")
    );

    std::env::set_current_dir(previous).unwrap();
}

#[tokio::test]
async fn reconcile_runtime_schedule_announces_new_completed_tasks_once() {
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
        crate::project::TaskStatus::Pending,
    )
    .await
    .unwrap();

    let (_state_tx, state_rx) = watch::channel::<Option<WatchedState>>(None);
    let (msg_tx, mut msg_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut ctx = HqContext::new(state_rx, Display(msg_tx), false);
    ctx.seed_completed_task_announcements().await.unwrap();

    crate::project::record_task_status(
        "t-001",
        ".ferrus/tasks/t-001.md",
        crate::project::TaskStatus::Complete,
    )
    .await
    .unwrap();

    ctx.reconcile_runtime_schedule().await.unwrap();

    match msg_rx
        .try_recv()
        .expect("completion message should be sent")
    {
        tui::UiMessage::Success(text) => assert_eq!(text, "Task t-001 completed."),
        _ => panic!("expected success message"),
    }

    assert!(msg_rx.try_recv().is_err());

    ctx.reconcile_runtime_schedule().await.unwrap();
    assert!(msg_rx.try_recv().is_err());

    std::env::set_current_dir(previous).unwrap();
}

#[cfg(unix)]
#[test]
fn stale_pid_detection() {
    assert!(platform::pid_is_alive(std::process::id()));
    assert!(!platform::pid_is_alive(999999));
}
