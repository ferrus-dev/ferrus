//! Managed lifecycle regressions use temporary SQLite projects and scripted inference.

use super::*;
use crate::nano::{
    coding::CodingTools,
    commands::{self, TrustedLocal},
    instructions,
    journal::{FileJournal, Quotas},
    lifecycle,
    managed::{self, ManagedTools},
    native::NativeTools,
    provider::*,
    session::*,
    tools::*,
    workspace,
};
use serde_json::json;
use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

fn configure(f: &Fixture, commands: &[&str], retries: u32) {
    let commands = toml::Value::Array(
        commands
            .iter()
            .map(|v| toml::Value::String(v.to_string()))
            .collect(),
    );

    std::fs::write(f.root.join("ferrus.toml"), format!("[checks]\ncommands = {commands}\n[limits]\nmax_check_retries = {retries}\nmax_review_cycles = 3\nmax_feedback_lines = 30\nwait_timeout_secs = 1\n[lease]\nttl_secs = 30\nheartbeat_interval_secs = 1\n")).unwrap();
}

fn native(
    f: &Fixture,
    session: FerrusSession,
    id: &str,
) -> (NativeTools<TrustedLocal>, FileJournal) {
    let journal = FileJournal::create(&f.data, id, Quotas::default()).unwrap();
    let coding = CodingTools {
        workspace: workspace::Workspace::new(session.workspace(), workspace::Limits::default())
            .unwrap(),
        commands: commands::Commands::trusted_local(
            session.workspace(),
            id,
            journal.directory(),
            commands::Limits::default(),
        )
        .unwrap(),
    };
    (
        NativeTools::new(session, coding, instructions::Limits::default()).unwrap(),
        journal,
    )
}

fn identity(id: &str) -> SessionIdentity {
    SessionIdentity {
        session_id: id.into(),
        project_id: "test-project".into(),
        task_id: Some(TASK.into()),
        run_id: Some(RUN.into()),
    }
}

fn call(name: &str, args: Value) -> ValidatedCall {
    ValidatedCall {
        call_id: "call-1".into(),
        provider_call_id: "provider-1".into(),
        name: name.into(),
        arguments: args,
    }
}

const QUESTION: &str = "## Problem\nNeed guidance\n## What I tried\nRead code\n## Options (if any)\nTwo options\n## Question\nWhich one?";

struct Script {
    responses: VecDeque<ProviderEvent>,
    starts: Arc<AtomicUsize>,
    stall: bool,
}

impl Provider for Script {
    async fn start(&mut self, _: ModelRequest) -> std::result::Result<(), ProviderError> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn next_event(&mut self) -> std::result::Result<Option<ProviderEvent>, ProviderError> {
        if self.stall {
            std::future::pending::<()>().await;
        }
        Ok(self.responses.pop_front())
    }
}

fn script(calls: Vec<ToolCall>) -> Script {
    Script {
        responses: VecDeque::from([ProviderEvent::Completed {
            response: ModelResponse {
                finish: if calls.is_empty() {
                    FinishReason::Stop
                } else {
                    FinishReason::ToolCalls
                },
                text: "Done".into(),
                calls,
                continuation: None,
            },
            usage: Some(Usage {
                input_tokens: 10,
                output_tokens: 10,
                reported: true,
            }),
        }]),
        starts: Arc::new(AtomicUsize::new(0)),
        stall: false,
    }
}

#[tokio::test]
async fn managed_checks_share_retry_counts_and_stop_at_exhaustion() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    configure(&f, &["exit 1"], 2);
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    assert!(
        lifecycle::check(&session, &Cancellation::default())
            .await
            .is_err()
    );
    session.claim().await.unwrap();
    let result = lifecycle::check(&session, &Cancellation::default())
        .await
        .unwrap();
    assert_eq!(result["retries"], 1);
    assert_eq!(session.status().await.unwrap().status, "executing");
    let result = lifecycle::check(&session, &Cancellation::default())
        .await
        .unwrap();
    assert_eq!(result["task_state"], "failed");
    assert!(
        lifecycle::check(&session, &Cancellation::default())
            .await
            .is_err()
    );
    assert_eq!(session.status().await.unwrap().check_retries, 2);
    assert_eq!(
        std::fs::read_dir(f.root.join(".ferrus/logs"))
            .unwrap()
            .count(),
        2
    );
}

#[tokio::test]
async fn managed_check_pass_clears_retries_and_uses_bound_workspace() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    configure(&f, &["exit 1"], 3);
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    lifecycle::check(&session, &Cancellation::default())
        .await
        .unwrap();
    configure(&f, &["echo checked > native-check.txt"], 3);
    let elsewhere = tempfile::tempdir().unwrap();
    std::env::set_current_dir(elsewhere.path()).unwrap();
    let value = lifecycle::check(&session, &Cancellation::default())
        .await
        .unwrap();
    assert_eq!(value["status"], "passed");
    assert_eq!(session.status().await.unwrap().check_retries, 0);
    assert!(f.root.join("native-check.txt").exists());
    assert!(!elsewhere.path().join("native-check.txt").exists());
}

#[tokio::test]
async fn managed_waits_restore_addressing_once_and_deliver_actual_answers() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    f.connection()
        .execute("UPDATE tasks SET status = 'addressing'", [])
        .unwrap();
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    for human in [false, true] {
        lifecycle::ask(&session, human, QUESTION.into())
            .await
            .unwrap();
        assert!(
            lifecycle::poll_answer(&session, human)
                .await
                .unwrap()
                .is_none()
        );
        let directory = f.root.join(".ferrus/runs/t-001");
        std::fs::write(
            directory.join(if human {
                "ANSWER.md"
            } else {
                "CONSULT_RESPONSE.md"
            }),
            "Use the bounded implementation.",
        )
        .unwrap();
        let result = lifecycle::poll_answer(&session, human)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result["answer"], "Use the bounded implementation.");
        assert_eq!(result["resumed_state"], "addressing");
        assert!(lifecycle::poll_answer(&session, human).await.is_err());
        assert_eq!(session.status().await.unwrap().status, "addressing");
    }
}

#[tokio::test]
async fn managed_operations_reject_foreign_expired_and_wrong_role_bindings() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    for sql in [
        "UPDATE tasks SET claimed_by = 'executor:other:1'",
        "UPDATE tasks SET claimed_by = 'executor:nano:1', lease_until = '2000-01-01T00:00:00Z'",
        "UPDATE runs SET role = 'supervisor'",
    ] {
        f.connection().execute(sql, []).unwrap();
        let events = f.events();
        assert!(
            lifecycle::check(&session, &Cancellation::default())
                .await
                .is_err()
        );
        assert!(
            lifecycle::submit(&session, "notes".into(), &Cancellation::default())
                .await
                .is_err()
        );
        assert!(
            lifecycle::ask(&session, true, "question".into())
                .await
                .is_err()
        );
        assert_eq!(f.events(), events);
    }
}

#[tokio::test]
async fn managed_submit_runs_both_gates_and_never_repeats_a_handoff() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    // Runtime output is deliberately outside the source tree checked for changes.
    configure(&f, &["echo gate >> .ferrus/gates.txt"], 3);
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    let result = lifecycle::submit(
        &session,
        "## Summary\nImplemented".into(),
        &Cancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(result["status"], "submitted");
    assert_eq!(
        std::fs::read_to_string(f.root.join(".ferrus/gates.txt"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    assert_eq!(session.status().await.unwrap().status, "reviewing");
    let events = f.events();
    assert!(
        lifecycle::submit(&session, "replacement".into(), &Cancellation::default())
            .await
            .is_err()
    );
    assert_eq!(f.events(), events);
    assert!(
        std::fs::read_to_string(f.root.join(".ferrus/runs/t-001/SUBMISSION.md"))
            .unwrap()
            .contains("Implemented")
    );
}

#[tokio::test]
async fn managed_submit_failure_keeps_work_open_without_submission_artifacts() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    configure(&f, &["exit 1"], 3);
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    let result = lifecycle::submit(&session, "notes".into(), &Cancellation::default())
        .await
        .unwrap();
    assert_eq!(result["status"], "failed");
    assert_eq!(session.status().await.unwrap().status, "executing");
    assert_eq!(session.status().await.unwrap().check_retries, 1);
    assert!(!f.root.join(".ferrus/runs/t-001/SUBMISSION.md").exists());
}

#[tokio::test]
async fn managed_engine_stops_at_submit_before_later_calls_or_inference() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    let (tools, journal) = native(&f, session.clone(), "managed-submit");
    let provider = script(vec![
        ToolCall {
            provider_call_id: "submit".into(),
            name: "submit".into(),
            arguments: json!({"content":"## Summary\nDone"}).to_string(),
        },
        ToolCall {
            provider_call_id: "later".into(),
            name: "ask_human".into(),
            arguments: json!({"question":"Must never execute"}).to_string(),
        },
    ]);
    let starts = provider.starts.clone();
    let result = managed::run(
        session.clone(),
        identity("managed-submit"),
        Limits::default(),
        provider,
        tools,
        journal,
        &Cancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(result.reason, EndReason::Submitted);
    assert!(result.durable);
    assert_eq!(starts.load(Ordering::SeqCst), 1);
    assert_eq!(result.budget.tool_calls, 1);
    assert_eq!(session.status().await.unwrap().status, "reviewing");
    assert!(!f.root.join(".ferrus/runs/t-001/QUESTION.md").exists());
}

#[tokio::test]
async fn managed_model_final_is_incomplete_and_never_completes_the_task() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    let (tools, journal) = native(&f, session.clone(), "managed-final");
    let result = managed::run(
        session.clone(),
        identity("managed-final"),
        Limits::default(),
        script(vec![]),
        tools,
        journal,
        &Cancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(result.reason, EndReason::ModelFinished);
    assert_eq!(session.status().await.unwrap().status, "executing");
}

#[tokio::test]
async fn managed_heartbeat_renews_during_stalled_inference_then_stops_on_loss() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    configure(&f, &[], 3);
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    let (tools, journal) = native(&f, session.clone(), "managed-heartbeat");
    let mut provider = script(vec![]);
    provider.stall = true;
    let cancellation = Cancellation::default();
    let execution = managed::run(
        session.clone(),
        identity("managed-heartbeat"),
        Limits::default(),
        provider,
        tools,
        journal,
        &cancellation,
    );
    let lose = async {
        wait_for_renewal(&f).await;
        assert!(
            f.events()
                .iter()
                .any(|(kind, _)| kind == "task_lease_renewed")
        );
        f.connection()
            .execute("UPDATE tasks SET claimed_by = 'executor:other:1'", [])
            .unwrap();
    };
    let (result, _) = tokio::join!(execution, lose);
    assert_eq!(result.unwrap().reason, EndReason::AuthorityLost);
    assert!(session.authorize().await.is_err());
}

#[tokio::test]
async fn managed_waits_keep_heartbeat_and_cancel_as_paused() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    configure(&f, &[], 3);
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    let (tools, journal) = native(&f, session.clone(), "managed-wait");
    let provider = script(vec![ToolCall {
        provider_call_id: "ask".into(),
        name: "ask_human".into(),
        arguments: json!({"question":"Need a decision"}).to_string(),
    }]);
    let starts = provider.starts.clone();
    let cancellation = Cancellation::default();
    let execution = managed::run(
        session.clone(),
        identity("managed-wait"),
        Limits::default(),
        provider,
        tools,
        journal,
        &cancellation,
    );
    let cancel = async {
        wait_for_renewal(&f).await;
        assert_eq!(session.status().await.unwrap().status, "awaiting_human");
        assert!(
            f.events()
                .iter()
                .any(|(kind, _)| kind == "task_lease_renewed")
        );
        cancellation.cancel();
    };
    let (result, _) = tokio::join!(execution, cancel);
    assert_eq!(result.unwrap().reason, EndReason::Paused);
    assert_eq!(starts.load(Ordering::SeqCst), 1);
    assert!(f.root.join(".ferrus/runs/t-001/QUESTION.md").exists());
}

#[tokio::test]
async fn managed_tool_surface_has_no_approval_or_identity_arguments() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    let (native, _journal) = native(&f, session.clone(), "managed-tools");
    let mut tools = ManagedTools::new(session, native, Cancellation::default());
    assert!(
        !tools
            .descriptors()
            .iter()
            .any(|tool| tool.name == "approve")
    );
    assert!(
        tools
            .validate("submit", &json!({"content":"done", "task_id":"foreign"}))
            .is_err()
    );
    assert!(
        tools
            .validate("check", &json!({"workspace":"elsewhere"}))
            .is_err()
    );
    assert!(
        tools
            .validate("consult", &json!({"question":"Missing template"}))
            .is_err()
    );
    assert!(matches!(
        tools
            .execute(&call("check", json!({})), &Cancellation::default())
            .await,
        ToolOutcome::Success(_)
    ));
    assert!(tools.shutdown().await);
}

fn git(f: &Fixture, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .current_dir(&f.root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

async fn git_session(f: &Fixture, index: bool) -> FerrusSession {
    use crate::repository_graph::{
        domain::{BuildId, PublishedViewName},
        extractors::builtin_extractor_identities,
        index::{IndexCoordinator, IndexRequest},
        source::{SourceDiscoveryContext, TaskBaselineSource, capture_worktree_tree},
        sqlite::{OpenSidecarResult, open_for_build_at},
    };
    let config = std::fs::read_to_string(f.root.join("ferrus.toml")).unwrap();
    std::fs::write(
        f.root.join("ferrus.toml"),
        format!("{config}\n[repository_graph]\nenabled = true\n"),
    )
    .unwrap();
    std::fs::create_dir(f.root.join("src")).unwrap();
    std::fs::write(f.root.join("src/lib.rs"), "pub struct Baseline;\n").unwrap();
    git(f, &["init", "--quiet"]);
    git(f, &["config", "core.autocrlf", "false"]);
    let baseline = capture_worktree_tree(&f.root).unwrap();
    std::fs::create_dir_all(f.data.join("worktrees/.baseline-trees")).unwrap();
    std::fs::write(
        f.data.join("worktrees/.baseline-trees/t-001.txt"),
        baseline.value(),
    )
    .unwrap();
    let mut launch = f.launch();
    launch.baseline_tree = Some(baseline.value().into());
    let session = FerrusSession::bind(launch).await.unwrap();
    session.claim().await.unwrap();
    if index {
        let context = crate::repository_graph_runtime::LocalGraphContext::load_for_runtime(
            &f.root,
            session.project_id(),
            &f.data,
            &session.status().await.unwrap(),
        )
        .await
        .unwrap();
        let discovery = SourceDiscoveryContext::from_config(
            context.repository,
            &context.config,
            &builtin_extractor_identities(),
        )
        .unwrap();
        let source = TaskBaselineSource::discover(&f.root, discovery, baseline).unwrap();
        let OpenSidecarResult::Ready(mut sidecar) =
            open_for_build_at(&f.data.join("repo-graph.db")).unwrap()
        else {
            panic!("sidecar")
        };
        let snapshot = IndexCoordinator::new(&mut sidecar)
            .index(
                &source,
                &context.config,
                IndexRequest {
                    build_id: BuildId::new("nano-lifecycle-baseline").unwrap(),
                    view_name: PublishedViewName::new("canonical").unwrap(),
                    force_full: false,
                },
            )
            .unwrap()
            .snapshot
            .id;
        drop(sidecar);
        project::record_task_repository_view(
            TASK,
            &project::RepositoryViewReference::materialized(
                snapshot.clone(),
                None,
                snapshot,
                project::RepositoryViewStatus::Available,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    }
    session
}

#[tokio::test]
async fn managed_submit_freezes_and_pins_the_exact_graph_tree() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = git_session(&f, true).await;
    std::fs::write(f.root.join("src/lib.rs"), "pub struct Submitted;\n").unwrap();
    let checked = crate::repository_graph::source::capture_worktree_tree(&f.root).unwrap();
    let result = lifecycle::submit(
        &session,
        "## Summary\nChanges".into(),
        &Cancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(result["status"], "submitted");
    let context = session.status().await.unwrap();
    assert_eq!(
        context.repository_view.lifecycle,
        crate::repository_graph::domain::TaskViewLifecycle::FrozenSubmitted
    );
    assert_eq!(
        context.repository_view.frozen_source_tree,
        Some(checked.clone())
    );
    assert!(
        f.events()
            .iter()
            .any(|(kind, _)| kind == "repository_view_frozen")
    );
    let refs = git(&f, &["for-each-ref", "--format=%(refname) %(objectname)"]);
    assert!(refs.contains("refs/ferrus/reviews/"));
    assert!(refs.contains(checked.value()));
    std::fs::write(f.root.join("src/lib.rs"), "pub struct Later;\n").unwrap();
    assert_eq!(
        session.status().await.unwrap().repository_view,
        context.repository_view
    );
}

#[tokio::test]
async fn managed_graph_freeze_failure_is_diagnostic_only() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = git_session(&f, false).await;
    let result = lifecycle::submit(&session, "notes".into(), &Cancellation::default())
        .await
        .unwrap();
    assert_eq!(result["status"], "submitted");
    assert_eq!(session.status().await.unwrap().status, "reviewing");
    assert!(
        f.events()
            .iter()
            .any(|(kind, _)| kind == "repository_view_freeze_failed")
    );
    assert_eq!(session.status().await.unwrap().check_retries, 0);
}

#[tokio::test]
async fn managed_submit_rejects_source_changes_during_gates() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    for with_git in [false, true] {
        let f = Fixture::new().await;
        configure(&f, &["echo changed >> source.txt"], 3);
        let session = if with_git {
            git_session(&f, false).await
        } else {
            let session = FerrusSession::bind(f.launch()).await.unwrap();
            session.claim().await.unwrap();
            session
        };
        std::fs::write(f.root.join("source.txt"), "before").unwrap();
        let error = lifecycle::submit(&session, "notes".into(), &Cancellation::default())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Workspace changed"), "{error:#}");
        assert_eq!(session.status().await.unwrap().status, "executing");
        assert!(!f.root.join(".ferrus/runs/t-001/SUBMISSION.md").exists());
    }
}

#[tokio::test]
async fn managed_check_quiesces_writers_and_allows_later_commands() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    let (native, _journal) = native(&f, session.clone(), "managed-writers");
    let mut tools = ManagedTools::new(session, native, Cancellation::default());
    let command = if cfg!(windows) {
        "ping -n 30 127.0.0.1 > nul"
    } else {
        "sleep 30"
    };
    let started = tools
        .native
        .coding
        .commands
        .exec(
            commands::ExecRequest {
                command: command.into(),
                cwd: ".".into(),
                timeout_ms: 30_000,
            },
            &Cancellation::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        tools
            .execute(&call("check", json!({})), &Cancellation::default())
            .await,
        ToolOutcome::Success(_)
    ));
    let stopped = tools
        .native
        .coding
        .commands
        .read_process(&started.process_id, 0)
        .await
        .unwrap();
    assert!(!stopped.potentially_writing());
    assert!(
        tools
            .native
            .coding
            .commands
            .exec(
                commands::ExecRequest {
                    command: "echo still-open".into(),
                    cwd: ".".into(),
                    timeout_ms: 1000
                },
                &Cancellation::default()
            )
            .await
            .is_ok()
    );
    assert!(tools.shutdown().await);
}

#[tokio::test]
async fn managed_claim_failure_prevents_the_first_inference() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    f.connection()
        .execute("UPDATE tasks SET claimed_by = 'executor:other:1'", [])
        .unwrap();
    let (tools, journal) = native(&f, session.clone(), "managed-unavailable");
    let provider = script(vec![]);
    let starts = provider.starts.clone();
    assert!(
        managed::run(
            session,
            identity("managed-unavailable"),
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
}

#[tokio::test]
async fn managed_interrupt_reconciles_committed_submit_before_delivery() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    let (native, _journal) = native(&f, session.clone(), "managed-race");
    let stop = Cancellation::default();
    let mut tools = ManagedTools::new(session.clone(), native, stop.clone());
    let committing = session.clone();
    tools.pending = Some(tokio::spawn(async move {
        let result = lifecycle::submit(&committing, "Original submission".into(), &stop).await?;
        // Hold delivery after the real SQLite commit until the engine interrupts.
        stop.cancelled().await;
        Ok(result)
    }));
    tokio::time::timeout(Duration::from_secs(5), async {
        while session.status().await.unwrap().status != "reviewing" {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let events = f.events();
    assert!(
        matches!(tools.interrupted().await, Some(ToolOutcome::Success(value)) if value["status"] == "submitted")
    );
    assert_eq!(tools.end_reason(), Some(EndReason::Submitted));
    assert!(tools.shutdown().await);
    assert_eq!(f.events(), events);
    assert_eq!(
        std::fs::read_to_string(f.root.join(".ferrus/runs/t-001/SUBMISSION.md")).unwrap(),
        "Original submission"
    );
}

#[tokio::test]
async fn managed_wait_returns_the_real_response_without_inference_polling() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    let (native, _journal) = native(&f, session.clone(), "managed-response");
    let mut tools = ManagedTools::new(session.clone(), native, Cancellation::default());
    let request = call("consult", json!({"question":QUESTION}));
    let stop = Cancellation::default();
    let execute = tools.execute(&request, &stop);
    let response = async {
        tokio::time::timeout(Duration::from_secs(5), async {
            while session.status().await.unwrap().status != "consultation" {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        std::fs::write(
            f.root.join(".ferrus/runs/t-001/CONSULT_RESPONSE.md"),
            "Preserve both check gates.",
        )
        .unwrap();
    };
    let (outcome, _) = tokio::join!(execute, response);
    assert!(
        matches!(outcome, ToolOutcome::Success(value) if value["answer"] == "Preserve both check gates.")
    );
    assert_eq!(session.status().await.unwrap().status, "executing");
    assert!(tools.shutdown().await);
}

#[tokio::test]
async fn managed_final_gate_failure_preserves_submit_retry_accounting() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let command = if cfg!(windows) {
        "if exist .ferrus\\first-gate.txt (exit /b 1) else (echo passed > .ferrus\\first-gate.txt)"
    } else {
        "if test -f .ferrus/first-gate.txt; then exit 1; else echo passed > .ferrus/first-gate.txt; fi"
    };
    configure(&f, &[command], 3);
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    let result = lifecycle::submit(&session, "notes".into(), &Cancellation::default())
        .await
        .unwrap();
    assert_eq!(result["status"], "failed");
    assert_eq!(result["retries"], 1);
    let context = session.status().await.unwrap();
    assert_eq!(context.status, "executing");
    assert_eq!(
        context.failure_reason,
        Some(format!("Commands failed: {command}"))
    );
    let events = f.events();
    assert_eq!(
        events
            .iter()
            .filter(|(kind, _)| kind == "check_passed")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|(kind, _)| kind == "submit_check_failed")
            .count(),
        1
    );
    assert!(!f.root.join(".ferrus/runs/t-001/SUBMISSION.md").exists());
}

#[tokio::test]
async fn managed_lease_loss_stops_a_long_check_without_recording_a_pass() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let command = if cfg!(windows) {
        "ping -n 30 127.0.0.1 > nul"
    } else {
        "sleep 30"
    };
    configure(&f, &[command], 3);
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    let (tools, journal) = native(&f, session.clone(), "managed-long-check");
    let provider = script(vec![ToolCall {
        provider_call_id: "check".into(),
        name: "check".into(),
        arguments: "{}".into(),
    }]);
    let cancellation = Cancellation::default();
    let execute = managed::run(
        session.clone(),
        identity("managed-long-check"),
        Limits::default(),
        provider,
        tools,
        journal,
        &cancellation,
    );
    let steal = async {
        wait_for_renewal(&f).await;
        assert!(
            f.events()
                .iter()
                .any(|(kind, _)| kind == "task_lease_renewed")
        );
        f.connection()
            .execute("UPDATE tasks SET claimed_by = 'executor:other:1'", [])
            .unwrap();
    };
    let (result, _) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(execute, steal)
    })
    .await
    .unwrap();
    assert_eq!(result.unwrap().reason, EndReason::AuthorityLost);
    assert!(!f.events().iter().any(|(kind, _)| kind == "check_passed"));
    assert_eq!(session.status().await.unwrap().check_retries, 0);
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

async fn wait_for_renewal(f: &Fixture) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !f
            .events()
            .iter()
            .any(|(kind, _)| kind == "task_lease_renewed")
        {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn managed_engine_ends_failed_when_check_retries_are_exhausted() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    configure(&f, &["exit 1"], 1);
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    let (tools, journal) = native(&f, session.clone(), "managed-exhausted");
    let provider = script(vec![ToolCall {
        provider_call_id: "check".into(),
        name: "check".into(),
        arguments: "{}".into(),
    }]);
    let result = managed::run(
        session.clone(),
        identity("managed-exhausted"),
        Limits::default(),
        provider,
        tools,
        journal,
        &Cancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(result.reason, EndReason::TaskFailed);
    assert_eq!(result.budget.model_turns, 1);
    assert_eq!(session.status().await.unwrap().status, "failed");
}

#[tokio::test]
async fn managed_isolated_submit_writes_canonical_artifacts_from_the_checked_worktree() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let original = git_session(&f, false).await;
    git(&f, &["add", "ferrus.toml", "src/lib.rs"]);
    git(
        &f,
        &[
            "-c",
            "user.name=Nano Test",
            "-c",
            "user.email=nano@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "baseline",
        ],
    );
    let workspace = f.data.join("worktrees").join(TASK);
    // Git for Windows rejects verbatim absolute paths when creating a worktree.
    // The git helper runs in f.root; keep the runtime binding absolute below.
    let relative_workspace = workspace.strip_prefix(&f.root).unwrap();
    git(
        &f,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            relative_workspace.to_str().unwrap(),
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
            "UPDATE runs SET workspace_path = ?1 WHERE id = ?2",
            [workspace.to_str().unwrap(), RUN],
        )
        .unwrap();
    let mut launch = f.launch();
    launch.workspace = workspace.clone();
    launch.baseline_tree = original.baseline_tree().map(str::to_owned);
    let session = FerrusSession::bind(launch).await.unwrap();
    std::fs::write(workspace.join("src/lib.rs"), "pub struct IsolatedChange;\n").unwrap();
    let unrelated = tempfile::tempdir().unwrap();
    std::env::set_current_dir(unrelated.path()).unwrap();
    let result = lifecycle::submit(
        &session,
        "Isolated submission".into(),
        &Cancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(result["status"], "submitted");
    let patch = std::fs::read_to_string(f.root.join(".ferrus/runs/t-001/PATCH.diff")).unwrap();
    assert!(patch.contains("+pub struct IsolatedChange;"));
    assert!(!patch.contains("project.toml"));
    assert!(!workspace.join(".ferrus/runs/t-001/SUBMISSION.md").exists());
    assert!(
        std::fs::read_to_string(f.root.join("src/lib.rs"))
            .unwrap()
            .contains("Baseline")
    );
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
