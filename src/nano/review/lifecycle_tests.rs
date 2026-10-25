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




const APPROVAL_CHECK_HELPER: &str =
    "nano::ferrus::tests::lifecycle_tests::reviewer_profiles::approval_check_barrier";





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



#[path = "lifecycle_tests/review.rs"]
mod review_tests;

#[path = "lifecycle_tests/recovery.rs"]
mod recovery_tests;



#[path = "lifecycle_tests/rollback.rs"]
mod rollback_tests;

#[path = "lifecycle_tests/cancellation.rs"]
mod cancellation_tests;

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

#[test]
fn approval_check_barrier() {
    // Only wait when invoked as the integration check subprocess, not in the suite.
    if !std::env::args().any(|arg| arg == APPROVAL_CHECK_HELPER) {
        return;
    }

    std::fs::write(".ferrus/approval-check-started", "ready").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !std::path::Path::new(".ferrus/approval-check-release").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "check was not released"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
