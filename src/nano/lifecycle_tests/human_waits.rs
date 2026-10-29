//! Tests for human waits.

use super::*;

#[tokio::test]
async fn stale_answer_without_a_matching_prior_question_is_not_reused() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    previous_run(&f, "old-read");
    previous_intent(&f, "old-read", "read_file", None, true);
    let run_dir = f.root.join(".ferrus/runs/t-001");
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("ANSWER.md"), "stale instruction").unwrap();
    let workspace =
        workspace::Workspace::new(session.workspace(), workspace::Limits::default()).unwrap();

    let recovered = crate::nano::resume::recover_previous(&session, &workspace)
        .await
        .unwrap()
        .unwrap();
    assert!(!recovered.note.contains("stale instruction"));
    assert_eq!(session.status().await.unwrap().status, "executing");
}

#[tokio::test]
async fn managed_relaunch_cannot_claim_foreign_or_nonworking_human_waits() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    lifecycle::ask(&session, true, "Which option?".into())
        .await
        .unwrap();
    for (index, (owner, resume)) in [
        (Some("supervisor:codex:1"), "executing"),
        (Some("executor:nano:2"), "addressing"),
        (None, "executing"),
        (Some(AGENT), "consultation"),
        (Some(AGENT), "reviewing"),
    ]
    .into_iter()
    .enumerate()
    {
        f.connection()
            .execute(
                "UPDATE tasks SET awaiting_human_by=?1, awaiting_human_status=?2,
             lease_until='2000-01-01T00:00:00Z' WHERE id=?3",
                rusqlite::params![owner, resume, TASK],
            )
            .unwrap();
        let tasks = project::list_tasks().await.unwrap();
        let events = f.events();
        let id = format!("invalid-wait-{index}");
        let (tools, journal) = native(&f, session.clone(), &id);
        let provider = script(vec![]);
        let starts = provider.starts.clone();
        assert!(
            managed::run(
                session.clone(),
                identity(&id),
                Limits::default(),
                provider,
                tools,
                journal,
                &Cancellation::default()
            )
            .await
            .is_err()
        );
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        f.assert_no_effect(tasks, events).await;
    }
}

#[tokio::test]
async fn managed_relaunch_preserves_answers_when_delivery_cannot_start() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    for human in [true, false] {
        let (state, file, event) = if human {
            ("awaiting_human", "ANSWER.md", "task_human_answered")
        } else {
            (
                "consultation",
                "CONSULT_RESPONSE.md",
                "task_consultation_resolved",
            )
        };
        for reason in ["missing", "cancelled", "context"] {
            let f = Fixture::new().await;
            let session = FerrusSession::bind(f.launch()).await.unwrap();
            session.claim().await.unwrap();
            lifecycle::ask(&session, human, QUESTION.into())
                .await
                .unwrap();
            let path = f.root.join(".ferrus/runs/t-001").join(file);
            let answer = if reason == "missing" {
                String::new()
            } else {
                "x".repeat(16 * 1024)
            };
            std::fs::write(&path, &answer).unwrap();
            let (tools, journal) = native(&f, session.clone(), "answer-delivery");
            let provider = script(vec![]);
            let starts = provider.starts.clone();
            let stop = Cancellation::default();
            let mut limits = Limits::default();
            if reason == "cancelled" {
                stop.cancel();
            }

            if reason == "context" {
                limits.context_bytes = 16 * 1024;
            }

            let error = managed::run(
                session.clone(),
                identity("answer-delivery"),
                limits,
                provider,
                tools,
                journal,
                &stop,
            )
            .await
            .unwrap_err();
            if reason == "missing" {
                assert!(error.to_string().contains("no stored"));
            }

            assert_eq!(starts.load(Ordering::SeqCst), 0);
            assert_eq!(session.status().await.unwrap().status, state);
            assert_eq!(std::fs::read_to_string(path).unwrap(), answer);
            assert!(!f.events().iter().any(|(kind, _)| kind == event));
        }
    }
}

#[tokio::test]
async fn managed_wait_rejects_wrong_owner_and_oversized_answers_without_consuming_them() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    lifecycle::ask(&session, true, "Need input".into())
        .await
        .unwrap();
    let answer = f.root.join(".ferrus/runs/t-001/ANSWER.md");
    std::fs::write(&answer, "Real answer").unwrap();
    f.connection()
        .execute(
            "UPDATE tasks SET awaiting_human_by = 'supervisor:other:1'",
            [],
        )
        .unwrap();
    assert!(lifecycle::poll_answer(&session, true).await.is_err());
    f.connection()
        .execute("UPDATE tasks SET awaiting_human_by = 'executor:nano:1'", [])
        .unwrap();
    std::fs::write(&answer, "x".repeat(16 * 1024 + 1)).unwrap();
    assert!(lifecycle::poll_answer(&session, true).await.is_err());
    assert_eq!(session.status().await.unwrap().status, "awaiting_human");
    assert!(answer.exists());
}

#[tokio::test]
async fn managed_consult_wait_preserves_a_nested_supervisor_human_question() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    lifecycle::ask(&session, false, QUESTION.into())
        .await
        .unwrap();
    project::record_task_human_question_requested_with_resume(
        TASK,
        TaskStatus::Consultation,
        Some(TaskStatus::Executing),
        "supervisor:fixture:1",
    )
    .await
    .unwrap();
    let directory = f.root.join(".ferrus/runs/t-001");
    std::fs::write(directory.join("ANSWER.md"), "For the Supervisor only").unwrap();
    assert!(
        lifecycle::poll_answer(&session, false)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(session.status().await.unwrap().status, "awaiting_human");
    assert!(directory.join("ANSWER.md").exists());
    project::restore_task_from_human_answer(TASK).await.unwrap();
    std::fs::write(
        directory.join("CONSULT_RESPONSE.md"),
        "Supervisor recommendation",
    )
    .unwrap();
    let result = lifecycle::poll_answer(&session, false)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result["answer"], "Supervisor recommendation");
    assert_eq!(result["resumed_state"], "executing");
}
