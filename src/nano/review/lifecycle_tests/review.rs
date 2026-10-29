//! Tests for review.

use super::*;

#[tokio::test]
async fn native_reviewer_source_limit_durably_stops_dispatch() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let executor = git_session(&f, true).await;
    lifecycle::submit(&executor, "Submission".into(), &Cancellation::default())
        .await
        .unwrap();
    // Keep valid frozen-view metadata, but point at an immutable oversized tree.
    std::fs::write(f.root.join("large.bin"), vec![b'x'; 8 * 1024 * 1024 + 1]).unwrap();
    let blob = git(&f, &["hash-object", "-w", "large.bin"]);
    git(&f, &["read-tree", "--empty"]);
    git(
        &f,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{blob},large.bin"),
        ],
    );
    let tree = git(&f, &["write-tree"]);
    f.connection()
        .execute(
            "UPDATE tasks SET repository_view_tree_digest=?1 WHERE id=?2",
            rusqlite::params![tree, TASK],
        )
        .unwrap();
    let tree = crate::repository_graph::source::parse_git_tree_digest(&tree).unwrap();
    crate::repository_graph::source::pin_submitted_tree(&f.root, TASK, &tree).unwrap();
    let task_pin = git(
        &f,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/ferrus/reviews/",
        ],
    );
    assert!(!task_pin.is_empty());
    // Failure must release only this task's pin, not another reviewing task's.
    crate::repository_graph::source::pin_submitted_tree(&f.root, "t-other", &tree).unwrap();
    for id in ["oversized-review", "retry-oversized-review"] {
        project::record_run_started_for_task_with_workspace(
            id,
            "supervisor",
            "supervisor:nano:t-001",
            std::process::id(),
            Some(TASK),
            f.root.to_string_lossy().into_owned(),
        )
        .await
        .unwrap();
        let error = ReviewSession::bind(LaunchContext {
            project_root: f.root.clone(),
            workspace: f.root.clone(),
            agent_id: "supervisor:nano:t-001".into(),
            task_id: TASK.into(),
            run_id: id.into(),
            baseline_tree: None,
        })
        .await
        .err()
        .unwrap();
        if id == "oversized-review" {
            assert!(
                error.to_string().contains("Reviewer source exceeds quota"),
                "{error:#}"
            );
        }

        let task = project::list_tasks()
            .await
            .unwrap()
            .into_iter()
            .find(|task| task.id == TASK)
            .unwrap();
        assert_eq!(task.status, "failed");
        assert_eq!(
            task.failure_reason.as_deref(),
            Some("nano_review_source_limit")
        );
        assert!(task.claimed_by.is_none());
        assert!(!f.data.join("nano/review-sources").join(id).exists());
        let pins = git(
            &f,
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/ferrus/reviews/",
            ],
        );
        assert!(!pins.lines().any(|pin| pin == task_pin));
        assert_eq!(pins.lines().count(), 1);
        project::record_run_finished(id, 1).await.unwrap();
    }

    assert_eq!(
        f.events()
            .iter()
            .filter(|(kind, _)| kind == "nano_supervisor_stopped")
            .count(),
        1
    );
}

#[tokio::test]
async fn native_rejection_revalidates_the_lease_phase_and_submission_at_commit() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let executor = git_session(&f, true).await;
    lifecycle::submit(&executor, "Submission".into(), &Cancellation::default())
        .await
        .unwrap();
    let session = reviewer(&f, "stalled-review", &f.root).await;
    let context = session.authorize().await.unwrap();
    let review_path = f.root.join(".ferrus/runs/t-001/REVIEW.md");
    std::fs::write(&review_path, "Accepted replacement feedback\n").unwrap();
    let db = f.connection();
    let (owner, lease, tree): (String, String, String) = db
        .query_row(
            "SELECT claimed_by, lease_until, repository_view_tree_digest FROM tasks WHERE id=?1",
            [TASK],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    for change in [
        "lease_until='2000-01-01T00:00:00Z'",
        "claimed_by='supervisor:codex:other'",
        "status='complete'",
        "status='addressing'",
        "repository_view_tree_digest='0000000000000000000000000000000000000000'",
    ] {
        db.execute(
            "UPDATE tasks SET status='reviewing', claimed_by=?2, lease_until=?3, repository_view_tree_digest=?4 WHERE id=?1",
            rusqlite::params![TASK, owner, lease, tree],
        ).unwrap();
        db.execute(&format!("UPDATE tasks SET {change} WHERE id=?1"), [TASK])
            .unwrap();
        let before = project::list_tasks().await.unwrap();
        let result = crate::server::tools::reject::run_for_context(
            context.clone(),
            "Stale feedback".into(),
            Config::load_from(&f.root).await.unwrap(),
            Some(session.scope.clone()),
        )
        .await;
        assert!(result.is_err(), "{change}");
        assert_eq!(
            std::fs::read_to_string(&review_path).unwrap(),
            "Accepted replacement feedback\n",
            "{change}"
        );
        assert_eq!(project::list_tasks().await.unwrap(), before, "{change}");
        assert_eq!(db.query_row("SELECT COUNT(*) FROM events WHERE type IN ('task_rejected','task_review_limit_exceeded')", [], |row| row.get::<_,i64>(0)).unwrap(), 0);
    }
}

#[tokio::test]
async fn native_rejection_keeps_review_open_when_feedback_publication_fails() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let executor = git_session(&f, true).await;
    lifecycle::submit(&executor, "Submission".into(), &Cancellation::default())
        .await
        .unwrap();
    let session = reviewer(&f, "publication-failure-review", &f.root).await;
    std::fs::create_dir(f.root.join(".ferrus/runs/t-001/REVIEW.md")).unwrap();
    let before = project::list_tasks().await.unwrap();
    let error = crate::server::tools::reject::run_for_context(
        session.authorize().await.unwrap(),
        "Feedback".into(),
        Config::load_from(&f.root).await.unwrap(),
        Some(session.scope.clone()),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("REVIEW.md"), "{error:#}");
    assert_eq!(project::list_tasks().await.unwrap(), before);
    assert!(!f.events().iter().any(|(kind, _)| kind == "task_rejected"));
}

#[tokio::test]
async fn native_review_reject_addressing_resubmit_approve_preserves_frozen_views() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let executor = git_session(&f, true).await;
    std::fs::write(f.root.join("src/lib.rs"), "pub struct FirstSubmission;\n").unwrap();
    lifecycle::submit(
        &executor,
        "First submission".into(),
        &Cancellation::default(),
    )
    .await
    .unwrap();
    let first = reviewer(&f, "review-one", &f.root).await;
    let frozen = first.status().await.unwrap().repository_view;
    assert!(
        std::fs::read_to_string(first.source.path.join("src/lib.rs"))
            .unwrap()
            .contains("FirstSubmission")
    );
    std::fs::write(f.root.join("src/lib.rs"), "pub struct UnsubmittedEdit;\n").unwrap();
    assert!(
        std::fs::read_to_string(first.source.path.join("src/lib.rs"))
            .unwrap()
            .contains("FirstSubmission")
    );
    let graph = Binding::Supervisor(first.clone()).graph().await.unwrap();
    assert_eq!(graph.repository_view.unwrap(), frozen);
    let context = crate::nano::context::Context::new(Binding::Supervisor(first.clone()));
    let result = context
        .retrieve(
            "repository_search",
            crate::nano::context::Request::parse(
                "repository_search",
                json!({"query":"FirstSubmission"}),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let evidence = serde_json::to_string(&result).unwrap();
    assert!(evidence.contains("FirstSubmission"));
    assert!(!evidence.contains("UnsubmittedEdit"));

    let end = decide(
        &f,
        first.clone(),
        vec![
            tool("read", "review_pending", json!({})),
            tool(
                "reject",
                "reject",
                json!({"notes":"Address the API contract"}),
            ),
            tool("forbidden", "approve", json!({})),
        ],
    )
    .await;
    assert_eq!(end.reason, EndReason::ReviewCompleted);
    project::record_run_finished("review-one", 0).await.unwrap();
    let task = project::list_tasks()
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.id == TASK)
        .unwrap();
    assert_eq!(task.status, "addressing");
    assert_eq!(task.review_cycles, 1);
    assert_eq!(task.check_retries, 0);
    assert_eq!(
        f.connection()
            .query_row(
                "SELECT executor_dispatches FROM tasks WHERE id=?1",
                [TASK],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    let old_tree = frozen.frozen_source_tree.clone();
    let oldview = f
        .connection()
        .query_row(
            "SELECT repository_view_tree_digest FROM runs WHERE id='review-one'",
            [],
            |r| r.get::<_, String>(0),
        )
        .unwrap();
    assert_eq!(oldview, frozen.frozen_source_tree.unwrap().value());
    executor.claim().await.unwrap();
    std::fs::write(f.root.join("src/lib.rs"), "pub struct FinalSubmission;\n").unwrap();
    lifecycle::submit(
        &executor,
        "Final submission".into(),
        &Cancellation::default(),
    )
    .await
    .unwrap();
    let second = reviewer(&f, "review-two", &f.root).await;
    assert_ne!(
        second
            .status()
            .await
            .unwrap()
            .repository_view
            .frozen_source_tree,
        old_tree
    );
    let end = decide(
        &f,
        second,
        vec![
            tool("read", "review_pending", json!({})),
            tool("approve", "approve", json!({})),
            tool("extra", "reject", json!({"notes":"must not run"})),
        ],
    )
    .await;
    assert_eq!(end.reason, EndReason::ReviewCompleted);
    assert_eq!(
        project::list_tasks()
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.id == TASK)
            .unwrap()
            .status,
        "complete"
    );
    assert!(
        std::fs::read_to_string(f.root.join(".ferrus/runs/t-001/REVIEW.md"))
            .unwrap()
            .contains("Address the API")
    );
}

#[tokio::test]
async fn native_reviewer_missing_freeze_and_wrong_role_fail_before_claim() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    project::record_task_status(TASK, ".ferrus/tasks/t-001.md", TaskStatus::Reviewing)
        .await
        .unwrap();
    project::record_run_started_for_task_with_workspace(
        "invalid-review",
        "supervisor",
        "supervisor:nano:t-001",
        std::process::id(),
        Some(TASK),
        f.root.to_string_lossy().into_owned(),
    )
    .await
    .unwrap();
    let launch = LaunchContext {
        project_root: f.root.clone(),
        workspace: f.root.clone(),
        agent_id: "supervisor:nano:t-001".into(),
        task_id: TASK.into(),
        run_id: "invalid-review".into(),
        baseline_tree: None,
    };
    assert!(ReviewSession::bind(launch.clone()).await.is_err());
    f.connection()
        .execute(
            "UPDATE runs SET role='executor' WHERE id='invalid-review'",
            [],
        )
        .unwrap();
    assert!(ReviewSession::bind(launch).await.is_err());
    assert!(
        project::list_tasks()
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.id == TASK)
            .unwrap()
            .claimed_by
            .is_none()
    );
}

#[tokio::test]
async fn native_approval_commits_completion_only_if_the_milestone_update_succeeds() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    for invalid_spec in [false, true] {
        let f = Fixture::new().await;
        let executor = git_session(&f, true).await;
        lifecycle::submit(
            &executor,
            "No source changes".into(),
            &Cancellation::default(),
        )
        .await
        .unwrap();
        let spec = f.root.join("spec.md");
        if invalid_spec {
            std::fs::create_dir(&spec).unwrap();
        } else {
            std::fs::write(&spec, "- [ ] #1.0 Deliver\n  - ID: m1.0\n").unwrap();
        }

        f.connection()
            .execute(
                "UPDATE tasks SET spec_path='spec.md', milestone_id='m1.0' WHERE id=?1",
                [TASK],
            )
            .unwrap();
        let session = reviewer(&f, "milestone-review", &f.root).await;
        let before = project::list_tasks().await.unwrap();
        let result = crate::server::tools::approve::run_for_context(
            session.authorize().await.unwrap(),
            f.root.clone(),
            Some(session.scope.clone()),
        )
        .await;
        if invalid_spec {
            assert!(result.is_err());
            assert_eq!(project::list_tasks().await.unwrap(), before);
        } else {
            result.unwrap();
            let tasks = project::list_tasks().await.unwrap();
            let task = tasks.iter().find(|task| task.id == TASK).unwrap();
            assert_eq!(task.status, "complete");
            assert!(task.claimed_by.is_none());
            assert!(
                std::fs::read_to_string(&spec)
                    .unwrap()
                    .contains("- [x] #1.0")
            );
        }
    }
}


#[tokio::test]
async fn native_approval_rolls_back_when_its_final_review_fence_is_lost() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    for change in ["reject", "lease", "owner", "run", "submission"] {
        let f = Fixture::new().await;
        let spec = "- [ ] #1.0 Deliver\n  - ID: m1.0\n  - Depends on: none\n";
        std::fs::write(f.root.join("spec.md"), spec).unwrap();
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
        f.connection()
            .execute(
                "UPDATE tasks SET spec_path='spec.md', milestone_id='m1.0' WHERE id=?1",
                [TASK],
            )
            .unwrap();
        // Use a relative executable without shell quoting, including on Windows.
        let helper = if cfg!(windows) {
            ".ferrus/approval-check.exe"
        } else {
            ".ferrus/approval-check"
        };
        let executable = std::env::current_exe().unwrap();
        let helper_path = f.root.join(helper);
        if std::fs::hard_link(&executable, &helper_path).is_err() {
            std::fs::copy(&executable, &helper_path).unwrap();
        }

        let helper = if cfg!(windows) {
            helper.replace('/', "\\")
        } else {
            format!("./{helper}")
        };
        let command = format!("{helper} --exact {APPROVAL_CHECK_HELPER} --nocapture");
        configure(&f, &[&command], 3);
        let session = reviewer(&f, "stalled-approval", &f.root).await;
        let context = session.authorize().await.unwrap();
        let before = std::fs::read(f.root.join("src/lib.rs")).unwrap();
        let index_before = std::fs::read(f.root.join(".git/index")).unwrap();
        let approval = crate::server::tools::approve::run_for_context(
            context,
            f.root.clone(),
            Some(session.scope.clone()),
        );
        let lose_fence = async {
            tokio::time::timeout(Duration::from_secs(20), async {
                while !f.root.join(".ferrus/approval-check-started").exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("approval never reached integration checks");
            assert!(
                std::fs::read_to_string(f.root.join("src/lib.rs"))
                    .unwrap()
                    .contains("ProposedChange")
            );
            let db = f.connection();
            match change {
                "reject" => {
                    db.execute(
                        "UPDATE tasks SET lease_until='2000-01-01T00:00:00Z' WHERE id=?1",
                        [TASK],
                    )
                    .unwrap();
                    project::claim_task(
                        TASK,
                        ".ferrus/tasks/t-001.md",
                        "supervisor:codex:other",
                        60,
                    )
                    .await
                    .unwrap();
                    project::record_task_review_rejected(TASK, 3).await.unwrap();
                }
                "lease" => {
                    db.execute(
                        "UPDATE tasks SET lease_until='2000-01-01T00:00:00Z' WHERE id=?1",
                        [TASK],
                    )
                    .unwrap();
                }
                "owner" => {
                    db.execute(
                        "UPDATE tasks SET claimed_by='supervisor:codex:other' WHERE id=?1",
                        [TASK],
                    )
                    .unwrap();
                }
                "run" => {
                    project::record_run_finished(&session.scope.run_id, 1)
                        .await
                        .unwrap();
                }
                "submission" => {
                    db.execute("UPDATE tasks SET repository_view_tree_digest='0000000000000000000000000000000000000000' WHERE id=?1", [TASK]).unwrap();
                }
                _ => unreachable!(),
            }

            let tasks = project::list_tasks().await.unwrap();
            std::fs::write(f.root.join(".ferrus/approval-check-release"), "done").unwrap();
            tasks
        };
        let (result, tasks_after_loss) = tokio::join!(approval, lose_fence);
        assert!(result.is_err(), "accepted stale approval after {change}");
        assert_eq!(
            project::list_tasks().await.unwrap(),
            tasks_after_loss,
            "{change}"
        );
        assert_eq!(
            std::fs::read(f.root.join("src/lib.rs")).unwrap(),
            before,
            "{change}"
        );
        assert_eq!(
            std::fs::read(f.root.join(".git/index")).unwrap(),
            index_before,
            "{change}"
        );
        assert_eq!(
            std::fs::read_to_string(f.root.join("spec.md")).unwrap(),
            spec,
            "{change}"
        );
        assert!(!f.events().iter().any(|(kind, _)| kind == "approved"));
    }
}
