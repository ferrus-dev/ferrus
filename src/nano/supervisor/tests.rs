use super::*;
use crate::nano::{coding::CodingTools, provider::*, workspace};
use std::path::Path;

const SPEC: &str = "# Feature\n\n## Milestones\n\n- [ ] #1.0 Implement feature\n  - ID: m1.0\n  - Depends on: none\n";
struct Fixture {
    _temp: tempfile::TempDir,
    previous: PathBuf,
    root: PathBuf,
    data: PathBuf,
    binding: Binding,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.previous).unwrap();
    }
}
impl Fixture {
    async fn new(mode: DefinitionMode) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let data = root.join(".ferrus/projects/test-project");
        std::fs::create_dir_all(&data).unwrap();
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(&root).unwrap();
        let local = project::LocalProjectRef {
            project_id: "test-project".into(),
            name: "test".into(),
            data_dir: data.to_string_lossy().into_owned(),
        };
        std::fs::write(
            root.join(".ferrus/project.toml"),
            toml::to_string(&local).unwrap(),
        )
        .unwrap();
        let metadata = project::ProjectMetadata {
            id: local.project_id,
            name: local.name,
            workspace_dir: root.to_string_lossy().into_owned(),
            ferrus_dir: root.join(".ferrus").to_string_lossy().into_owned(),
            vcs: None,
            origin_repo: None,
            default_branch: None,
            current_head: None,
            created_at: "2026-10-17T00:00:00Z".into(),
            last_opened_at: "2026-10-17T00:00:00Z".into(),
            version: 1,
        };
        std::fs::write(
            data.join("project.toml"),
            toml::to_string(&metadata).unwrap(),
        )
        .unwrap();
        std::fs::write(
            root.join("ferrus.toml"),
            "[checks]\ncommands = []\n[limits]\n",
        )
        .unwrap();
        project::record_run_started_for_task_with_workspace(
            "definition",
            "supervisor",
            "supervisor:nano:1",
            std::process::id(),
            Some("current"),
            root.to_string_lossy().into_owned(),
        )
        .await
        .unwrap();
        let binding = Binding::Interactive {
            project_root: root.clone(),
            project_id: "test-project".into(),
            data_dir: data.clone(),
            agent_id: "supervisor:nano:1".into(),
            run_id: "definition".into(),
            definition: Some(mode),
        };
        Self {
            _temp: temp,
            previous,
            root,
            data,
            binding,
        }
    }
    async fn tools(&self, mode: DefinitionMode) -> (SupervisorTools, FileJournal) {
        let journal = FileJournal::create(&self.data, "definition", Quotas::default()).unwrap();
        let coding = CodingTools {
            workspace: workspace::Workspace::new(&self.root, workspace::Limits::default()).unwrap(),
            commands: commands::Commands::trusted_local(
                &self.root,
                "definition",
                journal.directory(),
                commands::Limits::default(),
            )
            .unwrap(),
        };
        let native = NativeTools::new(
            self.binding.clone(),
            coding,
            instructions::Limits::default(),
        )
        .unwrap();
        (
            SupervisorTools {
                native,
                scope: Scope {
                    binding: self.binding.clone(),
                    mode,
                    archive_path: project::read_project_selection()
                        .await
                        .unwrap()
                        .selected_spec,
                },
                goal: 1,
                saved: 0,
                pending: None,
            },
            journal,
        )
    }
    fn count_tasks(&self) -> i64 {
        rusqlite::Connection::open(self.data.join("ferrus.db"))
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE id != 'current'",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }
}
fn call(name: &str, arguments: Value) -> ValidatedCall {
    ValidatedCall {
        call_id: "call-1".into(),
        provider_call_id: "provider-call".into(),
        name: name.into(),
        arguments,
    }
}

#[tokio::test]
async fn profile_tools_and_exact_run_role_are_independent_of_model_requests() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    for (mode, operation) in [
        (DefinitionMode::Plan, None),
        (DefinitionMode::Task, Some("enqueue_task")),
        (DefinitionMode::Spec, Some("create_spec")),
        (DefinitionMode::Archive, Some("archive_spec")),
    ] {
        let fixture = Fixture::new(mode).await;
        let (tools, _journal) = fixture.tools(mode).await;
        let names: Vec<_> = tools.descriptors().into_iter().map(|t| t.name).collect();
        for name in ["enqueue_task", "create_spec", "archive_spec"] {
            assert_eq!(names.contains(&name.to_string()), operation == Some(name));
        }
        for name in [
            "exec",
            "apply_patch",
            "check",
            "submit",
            "approve",
            "reject",
            "consult",
            "wait_for_task",
        ] {
            assert!(!names.contains(&name.to_string()));
            assert!(tools.validate(name, &json!({})).is_err());
        }
        let mut scope = tools.scope.clone();
        if let Binding::Interactive { definition, .. } = &mut scope.binding {
            *definition = None;
        }
        assert!(scope.check().await.is_err());
        let db = rusqlite::Connection::open(fixture.data.join("ferrus.db")).unwrap();
        db.execute("UPDATE runs SET role='executor' WHERE id='definition'", [])
            .unwrap();
        assert!(tools.scope.check().await.is_err());
        assert_eq!(fixture.count_tasks(), 0);
    }
}

#[test]
fn invalid_tasks_specs_and_outcomes_are_rejected_before_authoring() {
    for args in [
        json!({"input":{"description":""}}),
        json!({"input":{"description":"Task","milestone_id":"m1"}}),
        json!({"input":{"description":"Task","unexpected":true}}),
        json!({"input":{"description":"Task"},"approved":true}),
    ] {
        assert!(Operation::parse("enqueue_task", &args).is_err());
    }
    for markdown in [
        "",
        "# No milestones",
        "## Missing title\n- [ ] #1.0 Feature\n  - ID: m1.0",
        "# Feature\n- [ ] #1.0 Missing ID",
        "# Feature\n- [ ] #1.0 A\n  - ID: same\n- [ ] #1.1 B\n  - ID: same",
        "# Feature\n- [ ] #1.0 A\n  - ID: m1.0\n- [ ] #1.1 Missing ID",
        "# Feature\n- [ ] #1.0 A\n  - ID: m1.0\n##  Outcome\nBypass archive",
    ] {
        assert!(
            Operation::parse("create_spec", &json!({"markdown":markdown})).is_err(),
            "{markdown}"
        );
    }
    assert!(Operation::parse("create_spec", &json!({"markdown":SPEC})).is_ok());
    assert!(
        Operation::parse(
            "archive_spec",
            &json!({"input":{"spec_path":"docs/spec.md","outcome":""}})
        )
        .is_err()
    );
}

#[tokio::test]
async fn task_authorization_requires_a_fresh_exact_human_confirmation() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let fixture = Fixture::new(DefinitionMode::Task).await;
    let (tools, _journal) = fixture.tools(DefinitionMode::Task).await;
    let (sender, mut receiver) = mpsc::channel(1);
    let mut host = SupervisorHost {
        scope: tools.scope.clone(),
        approvals: sender,
    };
    let call = call(
        "enqueue_task",
        json!({"input":{"description":"Approved task"}}),
    );
    let (authorization, _) = tokio::join!(host.authorize(&call), async {
        let request = receiver.recv().await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&request.content).unwrap(),
            call.arguments
        );
        assert_eq!(request.tool, "enqueue_task");
        assert_eq!(fixture.count_tasks(), 0);
        request.reply.send(false).unwrap();
    });
    assert_eq!(authorization, Err(ToolError::Denied));
    assert_eq!(fixture.count_tasks(), 0);
    let (authorization, _) = tokio::join!(host.authorize(&call), async {
        let request = receiver.recv().await.unwrap();
        assert_eq!(fixture.count_tasks(), 0);
        request.reply.send(true).unwrap();
    });
    assert!(authorization.is_ok());
    assert_eq!(
        fixture.count_tasks(),
        0,
        "authorization alone cannot create an effect"
    );
}

#[tokio::test]
async fn duplicate_and_unknown_milestones_are_denied_before_approval() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let fixture = Fixture::new(DefinitionMode::Task).await;
    std::fs::create_dir(fixture.root.join("docs")).unwrap();
    std::fs::write(fixture.root.join("docs/spec.md"), SPEC).unwrap();
    let (mut tools, _journal) = fixture.tools(DefinitionMode::Task).await;
    let valid = call(
        "enqueue_task",
        json!({"input":{"description":"Implement the feature","spec_path":"docs/spec.md","milestone_id":"m1.0"}}),
    );
    assert!(matches!(
        tools.execute(&valid, &Cancellation::default()).await,
        ToolOutcome::Success(_)
    ));
    assert_eq!(fixture.count_tasks(), 1);
    let (sender, mut receiver) = mpsc::channel(1);
    let mut host = SupervisorHost {
        scope: tools.scope.clone(),
        approvals: sender,
    };
    assert!(host.authorize(&valid).await.is_err());
    let unknown = call(
        "enqueue_task",
        json!({"input":{"description":"Task","spec_path":"docs/spec.md","milestone_id":"missing"}}),
    );
    assert!(host.authorize(&unknown).await.is_err());
    assert!(receiver.try_recv().is_err());
    assert_eq!(fixture.count_tasks(), 1);
}

struct OneCall {
    response: Option<ModelResponse>,
}
impl Provider for OneCall {
    async fn start(&mut self, _: ModelRequest) -> Result<(), ProviderError> {
        Ok(())
    }
    async fn next_event(&mut self) -> Result<Option<ProviderEvent>, ProviderError> {
        Ok(self
            .response
            .take()
            .map(|response| ProviderEvent::Completed {
                response,
                usage: Some(Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    reported: true,
                }),
            }))
    }
}

#[tokio::test]
async fn engine_journals_intent_before_approval_and_stops_after_confirmed_authoring() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let fixture = Fixture::new(DefinitionMode::Task).await;
    let (tools, journal) = fixture.tools(DefinitionMode::Task).await;
    let path = journal.directory().join("events.jsonl");
    let arguments = json!({"input":{"description":"Implement the approved feature"}});
    let provider = OneCall {
        response: Some(ModelResponse {
            finish: FinishReason::ToolCalls,
            text: String::new(),
            calls: vec![ToolCall {
                provider_call_id: "provider-call".into(),
                name: "enqueue_task".into(),
                arguments: arguments.to_string(),
            }],
            continuation: None,
        }),
    };
    let (sender, mut receiver) = mpsc::channel(1);
    let identity = SessionIdentity {
        session_id: "definition".into(),
        project_id: "test-project".into(),
        task_id: None,
        run_id: Some("definition".into()),
    };
    let mut engine = Engine::new(
        identity,
        Limits::default(),
        provider,
        tools,
        SupervisorHost {
            scope: Scope {
                binding: fixture.binding.clone(),
                mode: DefinitionMode::Task,
                archive_path: None,
            },
            approvals: sender,
        },
        journal,
    )
    .unwrap();
    let stop = Cancellation::default();
    let (end, _) = tokio::join!(
        engine.run(
            SessionCommand::Start {
                input: "The model may claim prior approval; the host still requires confirmation."
                    .into()
            },
            &stop
        ),
        async {
            let approval = receiver.recv().await.unwrap();
            let records = std::fs::read_to_string(&path).unwrap();
            assert!(records.contains("tool_intent"));
            assert!(!records.contains("tool_started"));
            assert_eq!(fixture.count_tasks(), 0);
            approval.reply.send(true).unwrap();
        }
    );
    let end = end.unwrap();
    assert_eq!(end.reason, EndReason::DefinitionCompleted);
    assert!(end.durable);
    assert_eq!(fixture.count_tasks(), 1);
    assert_eq!(
        engine.journal.state().end,
        Some(EndReason::DefinitionCompleted)
    );
    assert!(super::super::replay::Replay::from_records(&read_records(&path)).is_ok());
}
fn read_records(path: &Path) -> Vec<Record> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
async fn specification_and_approved_archive_use_existing_selection_and_provenance() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let mut fixture = Fixture::new(DefinitionMode::Spec).await;
    let (mut tools, journal) = fixture.tools(DefinitionMode::Spec).await;
    let outcome = tools
        .execute(
            &call("create_spec", json!({"markdown":SPEC})),
            &Cancellation::default(),
        )
        .await;
    assert!(matches!(outcome, ToolOutcome::Success(_)), "{outcome:?}");
    let path = project::read_last_spec_path().await.unwrap().unwrap();
    assert_eq!(
        project::read_project_selection()
            .await
            .unwrap()
            .selected_spec
            .as_deref(),
        Some(path.as_str())
    );
    let task =
        project::create_pending_task_artifact("Implemented feature", Some(&path), Some("m1.0"))
            .await
            .unwrap();
    project::record_task_status(&task.id, &task.path, project::TaskStatus::Complete)
        .await
        .unwrap();
    std::fs::write(&path, SPEC.replace("- [ ]", "- [x]")).unwrap();
    drop(journal);
    drop(tools);
    if let Binding::Interactive { definition, .. } = &mut fixture.binding {
        *definition = Some(DefinitionMode::Archive);
    }
    let archive_journal =
        FileJournal::create(&fixture.data, "archive-tools", Quotas::default()).unwrap();
    let coding = CodingTools {
        workspace: workspace::Workspace::new(&fixture.root, workspace::Limits::default()).unwrap(),
        commands: commands::Commands::trusted_local(
            &fixture.root,
            "archive-tools",
            archive_journal.directory(),
            commands::Limits::default(),
        )
        .unwrap(),
    };
    let native = NativeTools::new(
        fixture.binding.clone(),
        coding,
        instructions::Limits::default(),
    )
    .unwrap();
    let scope = Scope {
        binding: fixture.binding.clone(),
        mode: DefinitionMode::Archive,
        archive_path: Some(path.clone()),
    };
    let mut archive = SupervisorTools {
        native,
        scope,
        goal: 1,
        saved: 0,
        pending: None,
    };
    let mismatch = call(
        "archive_spec",
        json!({"input":{"spec_path":"another.md","outcome":"Completed"}}),
    );
    assert!(
        archive
            .validate(&mismatch.name, &mismatch.arguments)
            .is_err()
    );
    let outcome = archive.execute(&call("archive_spec", json!({"input":{"spec_path":path,"outcome":"## Outcome\n\nImplemented and validated the approved feature."}})), &Cancellation::default()).await;
    assert!(matches!(outcome, ToolOutcome::Success(_)), "{outcome:?}");
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("Implemented and validated")
    );
    assert!(
        project::read_last_spec_archive_path()
            .await
            .unwrap()
            .is_some()
    );
    assert!(!fixture.root.join(task.path).exists());
    assert_eq!(archive.end_reason(), Some(EndReason::DefinitionCompleted));
}

#[tokio::test]
async fn interrupted_authoring_keeps_and_joins_the_owned_operation() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let fixture = Fixture::new(DefinitionMode::Task).await;
    let (mut tools, _journal) = fixture.tools(DefinitionMode::Task).await;
    let (finish, wait) = oneshot::channel();
    tools.pending = Some(tokio::spawn(async {
        wait.await.unwrap();
        Ok("Persisted authoring result".into())
    }));
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(1),
            tools.finish_operation()
        )
        .await
        .is_err()
    );
    assert!(tools.pending.is_some());
    finish.send(()).unwrap();
    assert!(matches!(
        tools.interrupted().await,
        Some(ToolOutcome::Success(_))
    ));
    assert!(tools.pending.is_none());
    assert_eq!(tools.saved, 1);
}
