use crate::nano::{
    binding::Binding,
    review::{self, Session as ReviewSession},
};

async fn reviewer(f: &Fixture, id: &str, workspace: &std::path::Path) -> ReviewSession {
    project::record_run_started_for_task_with_workspace(
        id,
        "supervisor",
        "supervisor:nano:t-001",
        std::process::id(),
        Some(TASK),
        workspace.to_string_lossy().into_owned(),
    )
    .await
    .unwrap();
    ReviewSession::bind(LaunchContext {
        project_root: f.root.clone(),
        workspace: workspace.into(),
        agent_id: "supervisor:nano:t-001".into(),
        task_id: TASK.into(),
        run_id: id.into(),
        baseline_tree: None,
    })
    .await
    .unwrap()
}
fn review_native(f: &Fixture, session: &ReviewSession) -> (NativeTools<TrustedLocal>, FileJournal) {
    let journal = FileJournal::create(&f.data, &session.scope.run_id, Quotas::default()).unwrap();
    let coding = CodingTools {
        workspace: workspace::Workspace::new(&session.source.path, workspace::Limits::default())
            .unwrap(),
        commands: commands::Commands::trusted_local(
            &session.source.path,
            &session.scope.run_id,
            journal.directory(),
            commands::Limits::default(),
        )
        .unwrap(),
    };
    (
        NativeTools::new(
            Binding::Supervisor(session.clone()),
            coding,
            instructions::Limits::default(),
        )
        .unwrap(),
        journal,
    )
}
fn tool(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        provider_call_id: id.into(),
        name: name.into(),
        arguments: args.to_string(),
    }
}
async fn decide(f: &Fixture, session: ReviewSession, calls: Vec<ToolCall>) -> SessionEnd {
    let (native, journal) = review_native(f, &session);
    review::run(
        session,
        Limits::default(),
        script(calls),
        native,
        journal,
        &Cancellation::default(),
    )
    .await
    .unwrap()
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
async fn native_consultation_responds_once_without_taking_the_executor_lease() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let executor = FerrusSession::bind(f.launch()).await.unwrap();
    executor.claim().await.unwrap();
    lifecycle::ask(&executor, false, QUESTION.into())
        .await
        .unwrap();
    let consultant = reviewer(&f, "consult-one", &f.root).await;
    assert_eq!(consultant.mode, review::Mode::Consult);
    assert_eq!(consultant.source.path, executor.workspace());
    let end = decide(
        &f,
        consultant,
        vec![
            tool("read", "consultation_context", json!({})),
            tool(
                "response",
                "respond_consult",
                json!({"response":"Use the stable API"}),
            ),
            tool(
                "again",
                "respond_consult",
                json!({"response":"must not overwrite"}),
            ),
        ],
    )
    .await;
    assert_eq!(end.reason, EndReason::ConsultationResponded);
    let task = project::list_tasks()
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.id == TASK)
        .unwrap();
    assert_eq!(task.claimed_by.as_deref(), Some(AGENT));
    assert_eq!(task.status, "consultation");
    assert_eq!(
        std::fs::read_to_string(f.root.join(".ferrus/runs/t-001/CONSULT_RESPONSE.md")).unwrap(),
        "Use the stable API"
    );
    let answer = lifecycle::poll_answer(&executor, false)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(answer["answer"], "Use the stable API");
    assert_eq!(executor.status().await.unwrap().status, "executing");
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

async fn isolated(f: &Fixture) -> FerrusSession {
    std::fs::write(f.root.join(".gitignore"), ".ferrus/\n").unwrap();
    let original = git_session(f, true).await;
    project::pin_executor_baseline_tree(&f.root, TASK, original.baseline_tree().unwrap())
        .await
        .unwrap();
    git(f, &["add", "ferrus.toml", "src/lib.rs", ".gitignore"]);
    git(
        f,
        &[
            "-c",
            "user.name=Nano Test",
            "-c",
            "user.email=nano@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=.ferrus/disabled-hooks",
            "commit",
            "--quiet",
            "-m",
            "baseline",
        ],
    );
    let workspace = f.data.join("worktrees").join(TASK);
    git(
        f,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            workspace.strip_prefix(&f.root).unwrap().to_str().unwrap(),
            "HEAD",
        ],
    );
    std::fs::create_dir_all(workspace.join(".ferrus")).unwrap();
    std::fs::copy(
        f.root.join(".ferrus/project.toml"),
        workspace.join(".ferrus/project.toml"),
    )
    .unwrap();
    f.connection()
        .execute(
            "UPDATE runs SET workspace_path=?1 WHERE id=?2",
            [workspace.to_str().unwrap(), RUN],
        )
        .unwrap();
    let mut launch = f.launch();
    launch.workspace = workspace;
    launch.baseline_tree = original.baseline_tree().map(str::to_owned);
    FerrusSession::bind(launch).await.unwrap()
}

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
async fn native_consultant_human_wait_cancels_and_relaunches_without_taking_a_lease() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let executor = FerrusSession::bind(f.launch()).await.unwrap();
    executor.claim().await.unwrap();
    lifecycle::ask(&executor, false, QUESTION.into())
        .await
        .unwrap();
    let consultant = reviewer(&f, "human-consult", &f.root).await;
    let (native, journal) = review_native(&f, &consultant);
    let cancel = Cancellation::default();
    let engine = review::run(
        consultant,
        Limits::default(),
        script(vec![tool(
            "question",
            "ask_human",
            json!({"question":"Which public API should be used?"}),
        )]),
        native,
        journal,
        &cancel,
    );
    let interrupt = async {
        for _ in 0..100 {
            if project::task_human_question_owner(TASK)
                .await
                .unwrap()
                .is_some()
            {
                cancel.cancel();
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("No human question");
    };
    let (end, ()) = tokio::join!(engine, interrupt);
    assert_eq!(end.unwrap().reason, EndReason::Cancelled);
    assert_eq!(
        project::list_tasks()
            .await
            .unwrap()
            .into_iter()
            .find(|task| task.id == TASK)
            .unwrap()
            .claimed_by
            .as_deref(),
        Some(AGENT)
    );
    project::record_run_finished("human-consult", 0)
        .await
        .unwrap();
    std::fs::write(
        f.root.join(".ferrus/runs/t-001/ANSWER.md"),
        "Use the stable API",
    )
    .unwrap();
    let resumed = reviewer(&f, "human-resumed", &f.root).await;
    let end = decide(
        &f,
        resumed,
        vec![
            tool("context", "consultation_context", json!({})),
            tool(
                "answer",
                "respond_consult",
                json!({"response":"Use the stable API"}),
            ),
        ],
    )
    .await;
    assert_eq!(end.reason, EndReason::ConsultationResponded);
    assert_eq!(executor.status().await.unwrap().status, "consultation");
    assert_eq!(
        lifecycle::poll_answer(&executor, false)
            .await
            .unwrap()
            .unwrap()["answer"],
        "Use the stable API"
    );
    project::record_run_finished("human-resumed", 0)
        .await
        .unwrap();
    lifecycle::ask(&executor, false, QUESTION.into())
        .await
        .unwrap();
    let fresh = reviewer(&f, "fresh-consult", &f.root).await;
    let end = decide(
        &f,
        fresh,
        vec![
            tool("context", "consultation_context", json!({})),
            tool(
                "response",
                "respond_consult",
                json!({"response":"A new answer"}),
            ),
        ],
    )
    .await;
    assert_eq!(end.reason, EndReason::ConsultationResponded);
    let records =
        std::fs::read_to_string(f.data.join("nano/sessions/fresh-consult/events.jsonl")).unwrap();
    let started: serde_json::Value = serde_json::from_str(records.lines().next().unwrap()).unwrap();
    let input: serde_json::Value =
        serde_json::from_str(started["event"]["input"].as_str().unwrap()).unwrap();
    assert!(
        input.get("human_answer").is_none(),
        "An identical new consultation must not inherit an old human answer"
    );
}

struct InterruptedApproval {
    inner: FileJournal,
    pending: Option<String>,
}
impl Journal for InterruptedApproval {
    fn append(&mut self, event: SessionEvent, budget: &Budget) -> Result<Record> {
        if let SessionEvent::ToolIntent { call_id, call, .. } = &event
            && call.name == "approve"
        {
            self.pending = Some(call_id.clone());
        }
        let started = matches!(&event,SessionEvent::ToolStarted{call_id} if self.pending.as_ref()==Some(call_id));
        let record = self.inner.append(event, budget)?;
        if started {
            return Err(anyhow::anyhow!(
                "Lost acknowledgement after durable ToolStarted"
            ));
        }
        Ok(record)
    }
    fn checkpoint(&mut self) -> Result<()> {
        self.inner.checkpoint()
    }
}
#[tokio::test]
async fn native_recovery_refuses_to_repeat_an_unknown_approval() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let executor = git_session(&f, true).await;
    lifecycle::submit(&executor, "Submission".into(), &Cancellation::default())
        .await
        .unwrap();
    let first = reviewer(&f, "interrupted-review", &f.root).await;
    let (native, journal) = review_native(&f, &first);
    let cancel = Cancellation::default();
    let end = review::run(
        first,
        Limits::default(),
        script(vec![
            tool("context", "review_pending", json!({})),
            tool("approve", "approve", json!({})),
        ]),
        native,
        InterruptedApproval {
            inner: journal,
            pending: None,
        },
        &cancel,
    )
    .await
    .unwrap();
    assert_eq!(end.reason, EndReason::JournalFailed);
    project::record_run_finished("interrupted-review", 0)
        .await
        .unwrap();
    let next = reviewer(&f, "recovered-review", &f.root).await;
    let (native, journal) = review_native(&f, &next);
    let provider = script(vec![tool("approve", "approve", json!({}))]);
    let starts = provider.starts.clone();
    let error = review::run(
        next,
        Limits::default(),
        provider,
        native,
        journal,
        &Cancellation::default(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("manual reconciliation"));
    assert_eq!(starts.load(Ordering::SeqCst), 0);
    assert_eq!(
        project::list_tasks()
            .await
            .unwrap()
            .into_iter()
            .find(|task| task.id == TASK)
            .unwrap()
            .failure_reason
            .as_deref(),
        Some("nano_effect_unknown")
    );
}

struct LostDecisionReceipt {
    inner: FileJournal,
}
impl Journal for LostDecisionReceipt {
    fn append(&mut self, event: SessionEvent, budget: &Budget) -> Result<Record> {
        let decided = matches!(&event,SessionEvent::ToolResult{outcome:ToolOutcome::Success(value),..} if value["status"]=="approved");
        let record = self.inner.append(event, budget)?;
        if decided {
            return Err(anyhow::anyhow!(
                "Lost acknowledgement after committed approval"
            ));
        }
        Ok(record)
    }
    fn checkpoint(&mut self) -> Result<()> {
        self.inner.checkpoint()
    }
}
#[tokio::test]
async fn native_approval_with_lost_receipt_does_not_reopen_or_repeat_the_completed_task() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let executor = isolated(&f).await;
    std::fs::write(
        executor.workspace().join("src/lib.rs"),
        "pub struct Final;\n",
    )
    .unwrap();
    lifecycle::submit(&executor, "Final change".into(), &Cancellation::default())
        .await
        .unwrap();
    let session = reviewer(&f, "lost-receipt-review", &f.root).await;
    let (native, journal) = review_native(&f, &session);
    let end = review::run(
        session,
        Limits::default(),
        script(vec![
            tool("context", "review_pending", json!({})),
            tool("approve", "approve", json!({})),
        ]),
        native,
        LostDecisionReceipt { inner: journal },
        &Cancellation::default(),
    )
    .await
    .unwrap();
    let records = std::fs::read_to_string(
        f.data
            .join("nano/sessions/lost-receipt-review/events.jsonl"),
    )
    .unwrap();
    assert_eq!(end.reason, EndReason::JournalFailed, "{records}");
    assert_eq!(
        project::list_tasks()
            .await
            .unwrap()
            .into_iter()
            .find(|task| task.id == TASK)
            .unwrap()
            .status,
        "complete"
    );
    assert!(
        std::fs::read_to_string(f.root.join("src/lib.rs"))
            .unwrap()
            .contains("Final")
    );
    project::record_run_finished("lost-receipt-review", 1)
        .await
        .unwrap();
    project::record_run_started_for_task_with_workspace(
        "no-replay",
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
        run_id: "no-replay".into(),
        baseline_tree: None,
    };
    assert!(ReviewSession::bind(launch).await.is_err());
}

#[tokio::test]
async fn native_consultant_revalidates_the_executor_workspace_before_authorizing_effects() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let executor = FerrusSession::bind(f.launch()).await.unwrap();
    executor.claim().await.unwrap();
    lifecycle::ask(&executor, false, QUESTION.into())
        .await
        .unwrap();
    let session = reviewer(&f, "bound-consult", &f.root).await;
    let other = tempfile::tempdir().unwrap();
    f.connection()
        .execute(
            "UPDATE runs SET workspace_path=?1 WHERE id=?2",
            rusqlite::params![other.path().to_string_lossy(), RUN],
        )
        .unwrap();
    assert!(session.authorize().await.is_err());
    assert!(
        !f.root
            .join(".ferrus/runs/t-001/CONSULT_RESPONSE.md")
            .exists()
    );
}
