use super::*;

#[tokio::test]
async fn sqlite_task_claim_is_exclusive_and_renewable() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;

    let first = claim_task("t-001", ".ferrus/tasks/t-001.md", "executor:codex:1", 60)
        .await
        .unwrap();
    assert!(matches!(first, TaskClaim::Claimed));

    let second = claim_task("t-001", ".ferrus/tasks/t-001.md", "executor:codex:1", 60)
        .await
        .unwrap();
    assert!(matches!(second, TaskClaim::AlreadyClaimed));

    let other = claim_task("t-001", ".ferrus/tasks/t-001.md", "executor:codex:2", 60)
        .await
        .unwrap();
    match other {
        TaskClaim::ClaimedByOther { claimed_by } => {
            assert_eq!(claimed_by, "executor:codex:1");
        }
        _ => panic!("expected claimed_by_other"),
    }

    let renewed = renew_claimed_task_lease("executor:codex:1", 60)
        .await
        .unwrap();
    assert!(matches!(renewed, LeaseRenewal::Renewed { .. }));

    teardown(previous);
}

#[tokio::test]
async fn sqlite_task_claim_can_target_non_current_task() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;

    let first = claim_task("t-002", ".ferrus/tasks/t-002.md", "executor:codex:2", 60)
        .await
        .unwrap();
    assert!(matches!(first, TaskClaim::Claimed));

    let second = claim_task("t-002", ".ferrus/tasks/t-002.md", "executor:codex:3", 60)
        .await
        .unwrap();
    match second {
        TaskClaim::ClaimedByOther { claimed_by } => {
            assert_eq!(claimed_by, "executor:codex:2");
        }
        _ => panic!("expected claimed_by_other"),
    }

    let tasks = list_tasks().await.unwrap();
    let current = tasks.iter().find(|task| task.id == "t-001").unwrap();
    let targeted = tasks.iter().find(|task| task.id == "t-002").unwrap();
    assert_eq!(current.claimed_by, None);
    assert_eq!(targeted.path, ".ferrus/tasks/t-002.md");
    assert_eq!(targeted.status, "unknown");
    assert_eq!(targeted.claimed_by.as_deref(), Some("executor:codex:2"));

    teardown(previous);
}

#[tokio::test]
async fn sqlite_task_lease_can_be_renewed_by_claiming_agent() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;

    let first = claim_task("t-002", ".ferrus/tasks/t-002.md", "executor:codex:2", 60)
        .await
        .unwrap();
    assert!(matches!(first, TaskClaim::Claimed));

    let renewed = renew_claimed_task_lease("executor:codex:2", 60)
        .await
        .unwrap();
    match renewed {
        LeaseRenewal::Renewed {
            task_id,
            task_path,
            claimed_by,
            ..
        } => {
            assert_eq!(task_id, "t-002");
            assert_eq!(task_path, ".ferrus/tasks/t-002.md");
            assert_eq!(claimed_by, "executor:codex:2");
        }
        _ => panic!("expected claimed task lease to renew"),
    }

    let missing = renew_claimed_task_lease("executor:codex:3", 60)
        .await
        .unwrap();
    assert!(matches!(missing, LeaseRenewal::NotClaimed));

    teardown(previous);
}

#[tokio::test]
async fn runtime_task_context_resolves_claimed_task_by_agent() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;

    record_task_status(
        "t-002",
        ".ferrus/tasks/t-002.md",
        crate::project::TaskStatus::Executing,
    )
    .await
    .unwrap();
    claim_task("t-002", ".ferrus/tasks/t-002.md", "executor:codex:2", 60)
        .await
        .unwrap();

    let context = runtime_task_context_for_agent("executor:codex:2")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(context.task_id, "t-002");
    assert_eq!(context.task_path, ".ferrus/tasks/t-002.md");
    assert_eq!(context.run_dir, ".ferrus/runs/t-002");
    assert_eq!(context.status, "executing");
    assert!(context.run_id.is_none());

    teardown(previous);
}

#[tokio::test]
async fn read_only_runtime_context_does_not_import_legacy_metadata() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    record_task_status(
        "t-002",
        ".ferrus/tasks/t-002.md",
        crate::project::TaskStatus::Executing,
    )
    .await
    .unwrap();
    claim_task("t-002", ".ferrus/tasks/t-002.md", "executor:codex:2", 60)
        .await
        .unwrap();
    let database_path = current_database_path().await.unwrap();
    {
        let connection = Connection::open(&database_path).unwrap();
        connection
            .execute_batch(
                r#"
                CREATE TABLE runtime_metadata (
                    key TEXT PRIMARY KEY,
                    value TEXT,
                    updated_at TEXT NOT NULL
                );
                INSERT INTO runtime_metadata (key, value, updated_at)
                VALUES ('selected_spec', 'docs/specs/legacy.md', 'legacy');
                UPDATE project_runtime_state
                SET selected_spec = NULL,
                    updated_at = 'read-only-sentinel'
                WHERE row_id = 1;
                "#,
            )
            .unwrap();
    }
    let original_permissions = std::fs::metadata(&database_path).unwrap().permissions();
    let mut read_only_permissions = original_permissions.clone();
    read_only_permissions.set_readonly(true);
    std::fs::set_permissions(&database_path, read_only_permissions).unwrap();

    let context = runtime_task_context_for_agent_read_only("executor:codex:2")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(context.task_id, "t-002");
    let connection =
        Connection::open_with_flags(&database_path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let state = connection
        .query_row(
            "SELECT selected_spec, updated_at FROM project_runtime_state WHERE row_id = 1",
            [],
            |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?)),
        )
        .unwrap();
    assert_eq!(state, (None, "read-only-sentinel".to_string()));
    std::fs::set_permissions(&database_path, original_permissions).unwrap();

    teardown(previous);
}

#[tokio::test]
async fn sqlite_claim_next_ready_task_skips_active_claims_and_preserves_agent_lease() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    record_task_status(
        "t-002",
        ".ferrus/tasks/t-002.md",
        crate::project::TaskStatus::Executing,
    )
    .await
    .unwrap();
    record_task_status(
        "t-003",
        ".ferrus/tasks/t-003.md",
        crate::project::TaskStatus::Reviewing,
    )
    .await
    .unwrap();

    let first = claim_next_ready_task("executor:codex:1", 60).await.unwrap();
    match first {
        ReadyTaskClaim::Claimed(task) => {
            assert_eq!(task.task_id, "t-001");
            assert_eq!(task.task_path, ".ferrus/tasks/t-001.md");
            assert_eq!(task.status, "executing");
            assert_eq!(task.claimed_by, "executor:codex:1");
        }
        _ => panic!("expected first ready task to be claimed"),
    }

    let same_agent = claim_next_ready_task("executor:codex:1", 60).await.unwrap();
    match same_agent {
        ReadyTaskClaim::AlreadyClaimed(task) => {
            assert_eq!(task.task_id, "t-001");
            assert_eq!(task.claimed_by, "executor:codex:1");
        }
        _ => panic!("expected existing agent lease"),
    }

    let other_agent = claim_next_ready_task("executor:codex:2", 60).await.unwrap();
    match other_agent {
        ReadyTaskClaim::Claimed(task) => {
            assert_eq!(task.task_id, "t-002");
            assert_eq!(task.task_path, ".ferrus/tasks/t-002.md");
            assert_eq!(task.status, "executing");
            assert_eq!(task.claimed_by, "executor:codex:2");
        }
        _ => panic!("expected second ready task to be claimed"),
    }

    let no_available = claim_next_ready_task("executor:codex:3", 60).await.unwrap();
    assert!(matches!(no_available, ReadyTaskClaim::NoAvailable));

    let tasks = list_tasks().await.unwrap();
    let reviewing = tasks.iter().find(|task| task.id == "t-003").unwrap();
    assert_eq!(reviewing.claimed_by, None);

    teardown(previous);
}

#[tokio::test]
async fn sqlite_claim_ready_task_by_id_promotes_pending_task() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    record_task_status(
        "t-002",
        ".ferrus/tasks/t-002.md",
        crate::project::TaskStatus::Pending,
    )
    .await
    .unwrap();

    let claim = claim_ready_task_by_id("t-002", "executor:codex:t-002", 60)
        .await
        .unwrap();

    match claim {
        ReadyTaskClaim::Claimed(task) => {
            assert_eq!(task.task_id, "t-002");
            assert_eq!(task.task_path, ".ferrus/tasks/t-002.md");
            assert_eq!(task.status, "executing");
            assert_eq!(task.claimed_by, "executor:codex:t-002");
        }
        _ => panic!("expected pending task to be promoted and claimed"),
    }

    let tasks = list_tasks().await.unwrap();
    let task = tasks.iter().find(|task| task.id == "t-002").unwrap();
    assert_eq!(task.status, "executing");
    assert_eq!(task.claimed_by.as_deref(), Some("executor:codex:t-002"));

    teardown(previous);
}

#[tokio::test]
async fn sqlite_claim_next_review_task_claims_reviewing_rows_only() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    record_task_status(
        "t-002",
        ".ferrus/tasks/t-002.md",
        crate::project::TaskStatus::Executing,
    )
    .await
    .unwrap();
    record_task_status(
        "t-003",
        ".ferrus/tasks/t-003.md",
        crate::project::TaskStatus::Reviewing,
    )
    .await
    .unwrap();

    let claim = claim_next_review_task("supervisor:codex:1", 60)
        .await
        .unwrap();

    match claim {
        ReadyTaskClaim::Claimed(task) => {
            assert_eq!(task.task_id, "t-003");
            assert_eq!(task.task_path, ".ferrus/tasks/t-003.md");
            assert_eq!(task.status, "reviewing");
            assert_eq!(task.claimed_by, "supervisor:codex:1");
        }
        _ => panic!("expected reviewing task to be claimed"),
    }

    let tasks = list_tasks().await.unwrap();
    let executing = tasks.iter().find(|task| task.id == "t-002").unwrap();
    let reviewing = tasks.iter().find(|task| task.id == "t-003").unwrap();
    assert_eq!(executing.claimed_by, None);
    assert_eq!(reviewing.claimed_by.as_deref(), Some("supervisor:codex:1"));

    teardown(previous);
}

#[tokio::test]
async fn sqlite_claim_review_task_by_id_does_not_steal_another_review() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    record_task_status(
        "t-002",
        ".ferrus/tasks/t-002.md",
        crate::project::TaskStatus::Reviewing,
    )
    .await
    .unwrap();
    record_task_status(
        "t-003",
        ".ferrus/tasks/t-003.md",
        crate::project::TaskStatus::Reviewing,
    )
    .await
    .unwrap();

    let missing = claim_review_task_by_id("t-999", "supervisor:codex:t-999", 60)
        .await
        .unwrap();
    assert!(matches!(missing, ReadyTaskClaim::NoAvailable));

    let claim = claim_review_task_by_id("t-003", "supervisor:codex:t-003", 60)
        .await
        .unwrap();
    match claim {
        ReadyTaskClaim::Claimed(task) => {
            assert_eq!(task.task_id, "t-003");
            assert_eq!(task.task_path, ".ferrus/tasks/t-003.md");
            assert_eq!(task.status, "reviewing");
            assert_eq!(task.claimed_by, "supervisor:codex:t-003");
        }
        _ => panic!("expected targeted reviewing task to be claimed"),
    }

    let tasks = list_tasks().await.unwrap();
    let other = tasks.iter().find(|task| task.id == "t-002").unwrap();
    let targeted = tasks.iter().find(|task| task.id == "t-003").unwrap();
    assert_eq!(other.claimed_by, None);
    assert_eq!(
        targeted.claimed_by.as_deref(),
        Some("supervisor:codex:t-003")
    );

    teardown(previous);
}

#[tokio::test]
async fn list_human_questions_reads_scoped_awaiting_human_tasks() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    for task_id in ["t-010", "t-002"] {
        record_task_status(
            task_id,
            &format!(".ferrus/tasks/{task_id}.md"),
            crate::project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        record_task_human_question_requested(
            task_id,
            crate::project::TaskStatus::Executing,
            &format!("executor:codex:{task_id}"),
        )
        .await
        .unwrap();
        crate::state::store::write_question_for_run_dir(
            &format!(".ferrus/runs/{task_id}"),
            &format!("Question for {task_id}"),
        )
        .await
        .unwrap();
    }

    let questions = list_human_questions().await.unwrap();

    assert_eq!(questions.len(), 2);
    assert_eq!(questions[0].task_id, "t-010");
    assert_eq!(questions[1].task_id, "t-002");

    record_task_human_answer("t-010").await.unwrap();
    let questions = list_human_questions().await.unwrap();
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].task_id, "t-002");
    let waiters = list_answered_human_waiters().await.unwrap();
    assert_eq!(waiters.len(), 1);
    assert_eq!(waiters[0].task_id, "t-010");
    assert_eq!(waiters[0].awaiting_human_by, "executor:codex:t-010");

    crate::state::store::write_answer_for_run_dir(
        ".ferrus/runs/t-002",
        "answer written before database update",
    )
    .await
    .unwrap();
    recover_runtime_state().await.unwrap();
    assert!(list_human_questions().await.unwrap().is_empty());
    let waiters = list_answered_human_waiters().await.unwrap();
    assert_eq!(waiters.len(), 2);
    assert_eq!(waiters[1].task_id, "t-002");

    teardown(previous);
}

#[tokio::test]
async fn record_scoped_human_answer_rolls_back_flag_when_file_write_fails() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    record_task_human_question_requested(
        "t-001",
        crate::project::TaskStatus::Executing,
        "executor:codex:t-001",
    )
    .await
    .unwrap();
    std::fs::create_dir_all(".ferrus/runs").unwrap();
    tokio::fs::write(".ferrus/runs/t-001", "not a directory")
        .await
        .unwrap();

    let question = HumanQuestion {
        task_id: "t-001".to_string(),
        task_path: ".ferrus/tasks/t-001.md".to_string(),
        run_dir: ".ferrus/runs/t-001".to_string(),
        question: "Which path?".to_string(),
    };
    let error = record_scoped_human_answer(&question, "Use the stable path.")
        .await
        .unwrap_err()
        .to_string();

    assert!(error.contains(".ferrus/runs/t-001"));
    let questions = list_human_questions().await.unwrap();
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].task_id, "t-001");
    assert!(list_answered_human_waiters().await.unwrap().is_empty());

    teardown(previous);
}

#[tokio::test]
async fn list_tasks_reads_runtime_rows() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    claim_task("t-001", ".ferrus/tasks/t-001.md", "executor:codex:1", 60)
        .await
        .unwrap();

    let tasks = list_tasks().await.unwrap();

    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].id, "t-001");
    assert_eq!(tasks[0].path, ".ferrus/tasks/t-001.md");
    assert_eq!(tasks[0].status, "executing");
    assert_eq!(tasks[0].claimed_by.as_deref(), Some("executor:codex:1"));
    assert!(tasks[0].lease_until.is_some());

    teardown(previous);
}

#[tokio::test]
async fn current_task_status_does_not_read_legacy_state_origin() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;

    record_current_task_status(crate::project::TaskStatus::Executing)
        .await
        .unwrap();

    let tasks = list_tasks().await.unwrap();
    let task = tasks.iter().find(|task| task.id == "t-001").unwrap();
    assert!(task.spec_path.is_none());
    assert!(task.milestone_id.is_none());

    teardown(previous);
}

#[tokio::test]
async fn task_status_update_preserves_existing_origin_metadata() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    record_task_status_with_origin(
        "t-001",
        ".ferrus/tasks/t-001.md",
        crate::project::TaskStatus::Executing,
        Some("docs/specs/spec.md"),
        Some("m1.0"),
    )
    .await
    .unwrap();

    record_task_status(
        "t-001",
        ".ferrus/tasks/t-001.md",
        crate::project::TaskStatus::Reviewing,
    )
    .await
    .unwrap();

    let tasks = list_tasks().await.unwrap();
    let task = tasks.iter().find(|task| task.id == "t-001").unwrap();
    assert_eq!(task.status, "reviewing");
    assert_eq!(task.spec_path.as_deref(), Some("docs/specs/spec.md"));
    assert_eq!(task.milestone_id.as_deref(), Some("m1.0"));

    teardown(previous);
}

#[tokio::test]
async fn finds_non_terminal_task_by_origin() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    record_task_status_with_origin(
        "t-002",
        ".ferrus/tasks/t-002.md",
        crate::project::TaskStatus::Pending,
        Some("docs/specs/spec.md"),
        Some("m1.1"),
    )
    .await
    .unwrap();

    let task = find_non_terminal_task_by_origin("docs/specs/spec.md", "m1.1")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(task.id, "t-002");

    record_task_status(
        "t-002",
        ".ferrus/tasks/t-002.md",
        crate::project::TaskStatus::Complete,
    )
    .await
    .unwrap();
    let task = find_non_terminal_task_by_origin("docs/specs/spec.md", "m1.1")
        .await
        .unwrap();
    assert!(task.is_none());

    teardown(previous);
}

#[tokio::test]
async fn sqlite_task_check_failures_use_per_task_retry_budget() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    claim_task("t-001", ".ferrus/tasks/t-001.md", "executor:codex:1", 60)
        .await
        .unwrap();

    let first = record_task_check_failed("t-001", "fmt failed", 2)
        .await
        .unwrap();
    assert!(matches!(first, TaskCheckFailure::Failed { retries: 1 }));

    let tasks = list_tasks().await.unwrap();
    assert_eq!(tasks[0].status, "executing");
    assert_eq!(tasks[0].check_retries, 1);
    assert_eq!(tasks[0].failure_reason.as_deref(), Some("fmt failed"));
    assert_eq!(tasks[0].claimed_by.as_deref(), Some("executor:codex:1"));

    let second = record_task_check_failed("t-001", "tests failed", 2)
        .await
        .unwrap();
    assert!(matches!(
        second,
        TaskCheckFailure::LimitExceeded { retries: 2 }
    ));

    let tasks = list_tasks().await.unwrap();
    assert_eq!(tasks[0].status, "failed");
    assert_eq!(tasks[0].check_retries, 2);
    assert_eq!(tasks[0].claimed_by, None);
    assert_eq!(tasks[0].lease_until, None);
    assert!(
        tasks[0]
            .failure_reason
            .as_deref()
            .unwrap_or_default()
            .contains("Last failure:\ntests failed")
    );

    teardown(previous);
}

#[tokio::test]
async fn mirrored_check_state_can_fail_task_and_clear_lease() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    record_task_status(
        "t-001",
        ".ferrus/tasks/t-001.md",
        crate::project::TaskStatus::Executing,
    )
    .await
    .unwrap();
    claim_task("t-001", ".ferrus/tasks/t-001.md", "executor:codex:1", 60)
        .await
        .unwrap();

    mirror_task_check_state(
        "t-001",
        crate::project::TaskStatus::Failed,
        2,
        Some("tests failed"),
    )
    .await
    .unwrap();

    let tasks = list_tasks().await.unwrap();
    let task = tasks.iter().find(|task| task.id == "t-001").unwrap();
    assert_eq!(task.status, "failed");
    assert_eq!(task.check_retries, 2);
    assert_eq!(task.failure_reason.as_deref(), Some("tests failed"));
    assert_eq!(task.claimed_by, None);

    teardown(previous);
}

#[tokio::test]
async fn sqlite_task_review_rejections_use_per_task_cycle_budget() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    record_task_status(
        "t-001",
        ".ferrus/tasks/t-001.md",
        crate::project::TaskStatus::Reviewing,
    )
    .await
    .unwrap();
    claim_task("t-001", ".ferrus/tasks/t-001.md", "supervisor:codex:1", 60)
        .await
        .unwrap();

    let first = record_task_review_rejected("t-001", 2).await.unwrap();
    assert!(matches!(
        first,
        TaskReviewRejection::Addressing { cycles: 1 }
    ));

    let tasks = list_tasks().await.unwrap();
    assert_eq!(tasks[0].status, "addressing");
    assert_eq!(tasks[0].review_cycles, 1);
    assert_eq!(tasks[0].check_retries, 0);
    assert_eq!(tasks[0].claimed_by, None);

    record_task_status(
        "t-001",
        ".ferrus/tasks/t-001.md",
        crate::project::TaskStatus::Reviewing,
    )
    .await
    .unwrap();
    let second = record_task_review_rejected("t-001", 2).await.unwrap();
    assert!(matches!(
        second,
        TaskReviewRejection::LimitExceeded { cycles: 2 }
    ));

    let tasks = list_tasks().await.unwrap();
    assert_eq!(tasks[0].status, "failed");
    assert_eq!(tasks[0].review_cycles, 2);
    assert_eq!(tasks[0].claimed_by, None);
    assert!(
        tasks[0]
            .failure_reason
            .as_deref()
            .unwrap_or_default()
            .contains("Task rejected 2 times")
    );

    teardown(previous);
}

#[tokio::test]
async fn executor_dispatch_budget_fails_task_after_limit() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;

    // Each attempt gates first, then records a started session.
    for expected in 1..=3 {
        match enforce_executor_dispatch_limit("t-001", 3).await.unwrap() {
            ExecutorDispatchOutcome::Proceed => {}
            other => panic!("expected Proceed, got {other:?}"),
        }
        assert_eq!(record_executor_dispatch("t-001").await.unwrap(), expected);
    }

    // The fourth attempt exhausts the per-phase budget: the task fails and is
    // not dispatched again.
    match enforce_executor_dispatch_limit("t-001", 3).await.unwrap() {
        ExecutorDispatchOutcome::LimitExceeded { dispatches } => assert_eq!(dispatches, 3),
        other => panic!("expected LimitExceeded, got {other:?}"),
    }

    let tasks = list_tasks().await.unwrap();
    assert_eq!(tasks[0].status, "failed");
    assert_eq!(tasks[0].claimed_by, None);
    assert!(
        tasks[0]
            .failure_reason
            .as_deref()
            .unwrap_or_default()
            .contains("dispatched 3 times")
    );

    teardown(previous);
}

#[tokio::test]
async fn executor_dispatch_gate_does_not_consume_budget() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;

    // Gating without recording models a spawn that fails during setup: the
    // budget must not be burned, so the task keeps getting Proceed.
    for _ in 0..5 {
        match enforce_executor_dispatch_limit("t-001", 3).await.unwrap() {
            ExecutorDispatchOutcome::Proceed => {}
            other => panic!("expected Proceed, got {other:?}"),
        }
    }

    let tasks = list_tasks().await.unwrap();
    assert_eq!(tasks[0].status, "executing");

    teardown(previous);
}

#[tokio::test]
async fn executor_dispatch_budget_resets_on_new_review_cycle() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;

    for expected in 1..=2 {
        match enforce_executor_dispatch_limit("t-001", 6).await.unwrap() {
            ExecutorDispatchOutcome::Proceed => {}
            other => panic!("expected Proceed, got {other:?}"),
        }
        assert_eq!(record_executor_dispatch("t-001").await.unwrap(), expected);
    }

    // A fresh rejection back to Addressing begins a new work phase, which
    // restores the full dispatch budget.
    record_task_status(
        "t-001",
        ".ferrus/tasks/t-001.md",
        crate::project::TaskStatus::Reviewing,
    )
    .await
    .unwrap();
    record_task_review_rejected("t-001", 5).await.unwrap();

    match enforce_executor_dispatch_limit("t-001", 6).await.unwrap() {
        ExecutorDispatchOutcome::Proceed => {}
        other => panic!("expected dispatch budget reset, got {other:?}"),
    }
    assert_eq!(record_executor_dispatch("t-001").await.unwrap(), 1);

    teardown(previous);
}

#[tokio::test]
async fn executor_dispatch_budget_disabled_when_zero() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;

    for expected in 1..=10 {
        match enforce_executor_dispatch_limit("t-001", 0).await.unwrap() {
            ExecutorDispatchOutcome::Proceed => {}
            other => panic!("expected Proceed with guard disabled, got {other:?}"),
        }
        assert_eq!(record_executor_dispatch("t-001").await.unwrap(), expected);
    }

    let tasks = list_tasks().await.unwrap();
    assert_eq!(tasks[0].status, "executing");

    teardown(previous);
}

#[tokio::test]
async fn handoff_task_statuses_clear_database_lease() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let (_dir, previous) = setup_project().await;
    claim_task("t-001", ".ferrus/tasks/t-001.md", "executor:codex:1", 60)
        .await
        .unwrap();

    record_task_status(
        "t-001",
        ".ferrus/tasks/t-001.md",
        crate::project::TaskStatus::Reviewing,
    )
    .await
    .unwrap();
    let tasks = list_tasks().await.unwrap();
    assert_eq!(tasks[0].status, "reviewing");
    assert_eq!(tasks[0].claimed_by, None);
    assert_eq!(tasks[0].lease_until, None);
    assert_eq!(tasks[0].last_heartbeat, None);

    claim_task("t-001", ".ferrus/tasks/t-001.md", "executor:codex:2", 60)
        .await
        .unwrap();
    record_task_status(
        "t-001",
        ".ferrus/tasks/t-001.md",
        crate::project::TaskStatus::Addressing,
    )
    .await
    .unwrap();
    let tasks = list_tasks().await.unwrap();
    assert_eq!(tasks[0].status, "addressing");
    assert_eq!(tasks[0].claimed_by, None);
    assert_eq!(tasks[0].lease_until, None);
    assert_eq!(tasks[0].last_heartbeat, None);

    teardown(previous);
}
