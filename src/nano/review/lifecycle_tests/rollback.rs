//! Tests for rollback.

use super::*;

#[tokio::test]
async fn native_approval_preserves_integration_rollback_and_allows_review_feedback() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let executor = isolated(&f).await;
    std::fs::write(
        executor.workspace().join("src/lib.rs"),
        "pub struct ProposedChange;\n",
    )
    .unwrap();
    lifecycle::submit(
        &executor,
        "Proposed change".into(),
        &Cancellation::default(),
    )
    .await
    .unwrap();
    configure(&f, &["exit 1"], 3);
    let review = reviewer(&f, "rollback-review", &f.root).await;
    let before = std::fs::read(f.root.join("src/lib.rs")).unwrap();
    let end = decide(
        &f,
        review,
        vec![
            tool("context", "review_pending", json!({})),
            tool("approve", "approve", json!({})),
            tool("reread", "review_pending", json!({})),
            tool(
                "reject",
                "reject",
                json!({"notes":"Integration checks failed; address the diagnostic"}),
            ),
        ],
    )
    .await;
    assert_eq!(end.reason, EndReason::ReviewCompleted);
    assert_eq!(std::fs::read(f.root.join("src/lib.rs")).unwrap(), before);
    assert!(
        f.root
            .join(".ferrus/runs/t-001/INTEGRATION_ERROR.md")
            .exists()
    );
    assert_eq!(
        project::list_tasks()
            .await
            .unwrap()
            .into_iter()
            .find(|task| task.id == TASK)
            .unwrap()
            .status,
        "addressing"
    );
}

#[tokio::test]
async fn native_approval_seals_the_session_when_rollback_fails() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let executor = isolated(&f).await;
    std::fs::write(
        executor.workspace().join("src/lib.rs"),
        "pub struct ProposedChange;\n",
    )
    .unwrap();
    lifecycle::submit(
        &executor,
        "Proposed change".into(),
        &Cancellation::default(),
    )
    .await
    .unwrap();
    // Leave an index lock behind after applying the change. The failed check
    // triggers rollback, which cannot restore the canonical index through it.
    configure(&f, &["git rev-parse --git-dir > .git/index.lock", "exit 1"], 3);
    let session = reviewer(&f, "failed-rollback-review", &f.root).await;
    let end = decide(
        &f,
        session,
        vec![
            tool("context", "review_pending", json!({})),
            tool("approve", "approve", json!({})),
            tool("reread", "review_pending", json!({})),
            tool(
                "reject",
                "reject",
                json!({"notes":"Must not reach this decision"}),
            ),
        ],
    )
    .await;
    assert_eq!(end.reason, EndReason::EffectUnknown);
    assert!(end.durable);
    assert_eq!(end.budget.tool_calls, 2);
    let task = project::list_tasks()
        .await
        .unwrap()
        .into_iter()
        .find(|task| task.id == TASK)
        .unwrap();
    assert_eq!(task.status, "failed");
    assert_eq!(task.failure_reason.as_deref(), Some("nano_effect_unknown"));
    assert!(
        !f.events()
            .iter()
            .any(|(kind, _)| kind == "task_rejected" || kind == "approved")
    );
}

#[tokio::test]
async fn native_approval_restores_milestone_when_completion_commit_fails() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    for changed_source in [false, true] {
        let f = Fixture::new().await;
        let executor = isolated(&f).await;
        if changed_source {
            std::fs::write(
                executor.workspace().join("src/lib.rs"),
                "pub struct ProposedChange;\n",
            )
            .unwrap();
        }

        lifecycle::submit(&executor, "Submission".into(), &Cancellation::default())
            .await
            .unwrap();
        if !changed_source {
            assert!(
                std::fs::read(f.root.join(".ferrus/runs/t-001/PATCH.diff"))
                    .unwrap()
                    .is_empty()
            );
        }

        // The spec is outside Git's captured tree, as it can be in real projects.
        let spec = f.root.join(".ferrus/approval-spec.md");
        let original = b"# Spec\r\n\r\n- [ ] #1.0 Deliver\r\n  - ID: m1.0\r\n";
        std::fs::write(&spec, original).unwrap();
        f.connection().execute(
            "UPDATE tasks SET spec_path='.ferrus/approval-spec.md', milestone_id='m1.0' WHERE id=?1", [TASK],
        ).unwrap();
        let session = reviewer(&f, "commit-failure-review", &f.root).await;
        // A deferred foreign key lets the complete UPDATE, event and milestone
        // write succeed, but rejects the enclosing transaction at COMMIT.
        f.connection()
            .execute_batch(
                "CREATE TABLE approval_commit_failure (
                run_id TEXT REFERENCES runs(id) DEFERRABLE INITIALLY DEFERRED
             );
             CREATE TRIGGER fail_approval_commit AFTER UPDATE OF status ON tasks
             WHEN NEW.status = 'complete'
             BEGIN
                INSERT INTO approval_commit_failure VALUES ('missing-approval-run');
             END;",
            )
            .unwrap();
        let tasks_before = project::list_tasks().await.unwrap();
        let source_before = std::fs::read(f.root.join("src/lib.rs")).unwrap();
        let index_before = std::fs::read(f.root.join(".git/index")).unwrap();
        let error = crate::server::tools::approve::run_for_context(
            session.authorize().await.unwrap(),
            f.root.clone(),
            Some(session.scope.clone()),
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("FOREIGN KEY constraint failed"),
            "{error:#}"
        );
        assert_eq!(std::fs::read(&spec).unwrap(), original);
        assert_eq!(project::list_tasks().await.unwrap(), tasks_before);
        assert_eq!(
            std::fs::read(f.root.join("src/lib.rs")).unwrap(),
            source_before
        );
        assert_eq!(
            std::fs::read(f.root.join(".git/index")).unwrap(),
            index_before
        );
        assert!(!f.events().iter().any(|(kind, _)| kind == "approved"));
    }
}
