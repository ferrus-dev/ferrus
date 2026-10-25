//! Tests for behavior.

use super::*;

#[tokio::test]
async fn previously_sealed_pending_intent_cannot_be_ignored() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    previous_run(&f, "old-sealed");
    previous_intent(&f, "old-sealed", "exec", None, true);
    let directory = f.data.join("nano/sessions/old-sealed");
    FileJournal::recover(&directory, Quotas::default()).unwrap();
    let workspace =
        workspace::Workspace::new(session.workspace(), workspace::Limits::default()).unwrap();

    assert!(
        crate::nano::resume::recover_previous(&session, &workspace)
            .await
            .is_err()
    );
    assert_eq!(session.status().await.unwrap().status, "failed");
}

#[tokio::test]
async fn crash_before_execution_boundary_never_runs_or_marks_an_effect_unknown() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    previous_run(&f, "old-unstarted");
    previous_intent(&f, "old-unstarted", "exec", None, false);
    {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(f.data.join("nano/sessions/old-unstarted/events.jsonl"))
            .unwrap()
            .write_all(b"{\"interrupted\":")
            .unwrap();
    }
    let workspace =
        workspace::Workspace::new(session.workspace(), workspace::Limits::default()).unwrap();

    let note = crate::nano::resume::recover_previous(&session, &workspace)
        .await
        .unwrap()
        .unwrap();
    assert!(note.note.contains("no effect was started"));
    assert_eq!(session.status().await.unwrap().status, "executing");
}

#[tokio::test]
async fn taskless_mutation_persists_invalidation_without_scheduled_maintenance() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let _managed = git_session(&f, true).await;
    project::record_task_status("current", ".ferrus/TASK.md", TaskStatus::Unknown)
        .await
        .unwrap();
    project::record_run_started_for_task_with_workspace(
        "direct-invalidation",
        "executor",
        "executor:nano:1",
        std::process::id(),
        Some("current"),
        f.root.to_string_lossy().into_owned(),
    )
    .await
    .unwrap();
    let binding = crate::nano::binding::Binding::Interactive {
        definition: None,
        project_root: f.root.clone(),
        project_id: "test-project".into(),
        data_dir: f.data.clone(),
        agent_id: "executor:nano:1".into(),
        run_id: "direct-invalidation".into(),
    };
    let prior = binding.graph().await.unwrap().status().await.unwrap();
    let journal = FileJournal::create(&f.data, "direct-invalidation", Quotas::default()).unwrap();
    let coding = CodingTools {
        workspace: workspace::Workspace::new(&f.root, workspace::Limits::default()).unwrap(),
        commands: commands::Commands::trusted_local(
            &f.root,
            "direct-invalidation",
            journal.directory(),
            commands::Limits::default(),
        )
        .unwrap(),
    };
    let mut tools =
        NativeTools::new(binding.clone(), coding, instructions::Limits::default()).unwrap();
    tools.working_set_enabled = false;
    let patch = baseline_patch(&tools, "CanonicalUnrefreshedEdit");
    // Invalidation must use the bound database rather than the process working directory.
    let elsewhere = tempfile::tempdir().unwrap();
    std::env::set_current_dir(elsewhere.path()).unwrap();
    assert!(
        matches!(tools.execute(&patch, &Cancellation::default()).await, ToolOutcome::Success(value) if value["complete"] == true)
    );
    drop(tools);
    std::env::set_current_dir(&f.root).unwrap();
    let state = project::canonical_graph_reference().await.unwrap();
    assert_eq!(state.status, project::CanonicalGraphStatus::Stale);
    let invalidations: i64 = f.connection().query_row(
        "SELECT COUNT(*) FROM events WHERE type = 'canonical_graph_invalidated' AND run_id = 'direct-invalidation'",
        [], |row| row.get(0),
    ).unwrap();
    assert_eq!(invalidations, 1);
    let after = binding.graph().await.unwrap().status().await.unwrap();
    assert_eq!(after.snapshot_id, prior.snapshot_id);
    assert_eq!(
        after.freshness.freshness,
        crate::repository_graph::domain::Freshness::Stale
    );
}
