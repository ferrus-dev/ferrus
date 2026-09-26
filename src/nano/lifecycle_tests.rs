//! Managed lifecycle regressions use temporary SQLite projects and scripted inference.

use super::*;
use crate::nano::{
    coding::CodingTools,
    commands::{self, TrustedLocal},
    instructions,
    journal::{FileJournal, Journal, Quotas},
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

fn previous_run(f: &Fixture, id: &str) {
    f.connection().execute(
        "INSERT INTO runs (rowid, id, task_id, role, agent, status, started_at, updated_at, workspace_path) \
         VALUES (0, ?1, ?2, 'executor', ?3, 'failed', '2026-01-01', '2026-01-01', ?4)",
        rusqlite::params![id, TASK, AGENT, f.root.to_string_lossy()],
    ).unwrap();
}

fn previous_intent(f: &Fixture, id: &str, name: &str, plan: Option<EffectPlan>, started: bool) {
    let mut journal = FileJournal::create(&f.data, id, Quotas::default()).unwrap();
    let mut budget = Budget::default();
    journal
        .append(
            SessionEvent::Started {
                identity: SessionIdentity {
                    session_id: id.into(),
                    project_id: "test-project".into(),
                    task_id: Some(TASK.into()),
                    run_id: Some(id.into()),
                },
                limits: Limits::default(),
                input: "task".into(),
                inherited_budget: None,
                provider: None,
            },
            &budget,
        )
        .unwrap();
    budget.model_turns = 1;
    budget.reserved_input_tokens = 10;
    budget.reserved_output_tokens = 10;
    journal
        .append(SessionEvent::ModelStarted { turn: 1 }, &budget)
        .unwrap();
    budget.reserved_input_tokens = 0;
    budget.reserved_output_tokens = 0;
    budget.reported_input_tokens = 1;
    budget.reported_output_tokens = 1;
    let call = ToolCall {
        provider_call_id: "provider-1".into(),
        name: name.into(),
        arguments: if name == "submit" {
            serde_json::json!({"content":"Ready for review."}).to_string()
        } else {
            "{}".into()
        },
    };
    journal
        .append(
            SessionEvent::ModelCompleted {
                response: ModelResponse {
                    finish: FinishReason::ToolCalls,
                    text: String::new(),
                    calls: vec![call.clone()],
                    continuation: None,
                },
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    reported: true,
                },
            },
            &budget,
        )
        .unwrap();
    budget.tool_calls = 1;
    journal
        .append(
            SessionEvent::ToolIntent {
                call_id: "call-1".into(),
                call,
                effect_plan: plan,
                start_recorded: true,
            },
            &budget,
        )
        .unwrap();
    if started {
        journal
            .append(
                SessionEvent::ToolStarted {
                    call_id: "call-1".into(),
                },
                &budget,
            )
            .unwrap();
    }
}

fn finish_previous_submit(f: &Fixture, id: &str) {
    let directory = f.data.join("nano/sessions").join(id);
    let (mut journal, mut records) =
        FileJournal::recover_open(&directory, Quotas::default()).unwrap();
    records.push(
        journal
            .append(
                SessionEvent::ToolResult {
                    call_id: "call-1".into(),
                    outcome: ToolOutcome::Success(json!({
                        "status":"submitted", "task_state":"reviewing", "task_id":TASK
                    })),
                },
                &journal.state().budget.clone(),
            )
            .unwrap(),
    );
    journal
        .seal_with(&mut records, EndReason::Submitted)
        .unwrap();
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
        assert!(
            directory
                .join(if human {
                    "ANSWER.md"
                } else {
                    "CONSULT_RESPONSE.md"
                })
                .exists()
        );
        assert!(lifecycle::poll_answer(&session, human).await.is_err());
        assert_eq!(session.status().await.unwrap().status, "addressing");
    }
}

#[tokio::test]
async fn previous_patch_is_reconciled_from_the_bound_workspace_once() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    let workspace =
        workspace::Workspace::new(session.workspace(), workspace::Limits::default()).unwrap();
    let plan = workspace
        .patch_effect_plan(workspace::patch::PatchRequest {
            edits: vec![workspace::patch::Edit::Create {
                path: "recovered.txt".into(),
                content: "recovered\n".into(),
            }],
        })
        .unwrap();
    previous_run(&f, "old-patch");
    previous_intent(&f, "old-patch", "apply_patch", Some(plan), true);
    std::fs::write(f.root.join("recovered.txt"), "recovered\n").unwrap();

    let first = crate::nano::resume::recover_previous(&session, &workspace)
        .await
        .unwrap()
        .unwrap();
    assert!(first.note.contains("matches its full recorded after-state"));
    assert_eq!(first.budget.model_turns, 1);
    assert_eq!(first.budget.tool_calls, 1);
    assert_eq!(first.budget.tokens(), 2);
    assert_eq!(session.status().await.unwrap().status, "executing");
    let old_dir = f.data.join("nano/sessions/old-patch");
    let old_size = std::fs::metadata(old_dir.join("events.jsonl"))
        .unwrap()
        .len();
    let second = crate::nano::resume::recover_previous(&session, &workspace)
        .await
        .unwrap()
        .unwrap();
    assert!(second.note.contains("old-patch"));
    assert_eq!(
        std::fs::metadata(old_dir.join("events.jsonl"))
            .unwrap()
            .len(),
        old_size
    );
    assert_eq!(
        std::fs::read_to_string(f.root.join("recovered.txt")).unwrap(),
        "recovered\n"
    );

    let (tools, journal) = native(&f, session.clone(), RUN);
    let end = managed::run(
        session.clone(),
        identity(RUN),
        Limits::default(),
        script(vec![]),
        tools,
        journal,
        &Cancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(end.reason, EndReason::ModelFinished);
    assert_eq!(end.budget.model_turns, 2);
    assert_eq!(end.budget.tool_calls, 1);
    assert_eq!(end.budget.reported_input_tokens, 11);
    let (_, records) =
        FileJournal::recover(&f.data.join("nano/sessions").join(RUN), Quotas::default()).unwrap();
    assert!(
        matches!(&records[0].event, SessionEvent::Started { inherited_budget: Some(budget), .. } if budget.model_turns == 1 && budget.tool_calls == 1)
    );
}

#[tokio::test]
async fn unknown_previous_command_fails_the_exact_owned_task() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    previous_run(&f, "old-command");
    previous_intent(&f, "old-command", "exec", None, true);
    let workspace =
        workspace::Workspace::new(session.workspace(), workspace::Limits::default()).unwrap();

    assert!(
        crate::nano::resume::recover_previous(&session, &workspace)
            .await
            .is_err()
    );
    assert_eq!(session.status().await.unwrap().status, "failed");
    let reason: String = f
        .connection()
        .query_row(
            "SELECT failure_reason FROM tasks WHERE id = ?1",
            [TASK],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(reason, "nano_effect_unknown");
}

#[tokio::test]
async fn empty_intermediate_run_cannot_hide_an_unknown_command() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    previous_run(&f, "old-command");
    previous_intent(&f, "old-command", "exec", None, true);
    f.connection()
        .execute("UPDATE runs SET rowid = -1 WHERE id = 'old-command'", [])
        .unwrap();
    f.connection()
        .execute(
            "INSERT INTO runs (rowid, id, task_id, role, agent, status, started_at, updated_at, workspace_path) \
             VALUES (0, 'empty-run', ?1, 'executor', ?2, 'failed', '2026-01-02', '2026-01-02', ?3)",
            rusqlite::params![TASK, AGENT, f.root.to_string_lossy()],
        )
        .unwrap();
    drop(FileJournal::create(&f.data, "empty-run", Quotas::default()).unwrap());
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
async fn historical_runs_after_the_latest_started_journal_do_not_exhaust_the_scan() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    previous_run(&f, "old-read");
    previous_intent(&f, "old-read", "read_file", None, true);
    let connection = f.connection();
    for ordinal in 1..=64 {
        connection.execute(
            "INSERT INTO runs (rowid, id, task_id, role, agent, status, started_at, updated_at, workspace_path) \
             VALUES (?1, ?2, ?3, 'executor', ?4, 'failed', '2026-01-01', '2026-01-01', ?5)",
            rusqlite::params![-ordinal, format!("older-{ordinal}"), TASK, AGENT, f.root.to_string_lossy()],
        ).unwrap();
    }
    let workspace =
        workspace::Workspace::new(session.workspace(), workspace::Limits::default()).unwrap();

    let recovered = crate::nano::resume::recover_previous(&session, &workspace)
        .await
        .unwrap()
        .unwrap();
    assert!(recovered.note.contains("old-read"));
}

#[tokio::test]
async fn completed_exec_intent_with_running_command_blocks_redispatch() {
    use std::io::Write;

    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    previous_run(&f, "old-running");
    previous_intent(&f, "old-running", "exec", None, true);
    let directory = f.data.join("nano/sessions/old-running");
    let (mut journal, _) = FileJournal::recover_open(&directory, Quotas::default()).unwrap();
    journal
        .append(
            SessionEvent::ToolResult {
                call_id: "call-1".into(),
                outcome: ToolOutcome::Success(json!({"process_id":"old-running-p1"})),
            },
            &journal.state().budget.clone(),
        )
        .unwrap();
    drop(journal);
    let commands_dir = directory.join("commands");
    crate::nano::private::directory(&commands_dir, true).unwrap();
    let snapshot = commands::Snapshot {
        process_id: "old-running-p1".into(),
        backend: "trusted_local".into(),
        completion: commands::Completion::Running,
        stdout: commands::OutputRef {
            handle: "old-running-p1-stdout".into(),
            bytes: 0,
        },
        stderr: commands::OutputRef {
            handle: "old-running-p1-stderr".into(),
            bytes: 0,
        },
        output_complete: false,
        mutation_scope: "unknown".into(),
    };
    let mut state =
        crate::nano::private::file(&commands_dir.join("old-running-p1.json"), true).unwrap();
    state
        .write_all(&serde_json::to_vec(&snapshot).unwrap())
        .unwrap();
    state.sync_all().unwrap();
    drop(state);
    for stream in ["stdout", "stderr"] {
        drop(
            crate::nano::private::file(
                &commands_dir.join(format!("old-running-p1-{stream}")),
                true,
            )
            .unwrap(),
        );
    }
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
async fn recovery_rejects_a_stolen_lease_before_touching_the_old_journal() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    previous_run(&f, "old-stale");
    previous_intent(&f, "old-stale", "exec", None, true);
    let old = f.data.join("nano/sessions/old-stale/events.jsonl");
    let bytes = std::fs::read(&old).unwrap();
    f.connection()
        .execute(
            "UPDATE tasks SET claimed_by = 'executor:other:1' WHERE id = ?1",
            [TASK],
        )
        .unwrap();
    let workspace =
        workspace::Workspace::new(session.workspace(), workspace::Limits::default()).unwrap();

    assert!(
        crate::nano::resume::recover_previous(&session, &workspace)
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(old).unwrap(), bytes);
    assert_eq!(session.status().await.unwrap().status, "executing");
}

#[tokio::test]
async fn recovery_does_not_skip_a_newer_run_from_another_workspace() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    previous_run(&f, "old-matching");
    previous_intent(&f, "old-matching", "exec", None, true);
    f.connection()
        .execute("UPDATE runs SET rowid = -1 WHERE id = 'old-matching'", [])
        .unwrap();
    f.connection().execute(
        "INSERT INTO runs (rowid, id, task_id, role, agent, status, started_at, updated_at, workspace_path) \
         VALUES (0, 'newer-elsewhere', ?1, 'executor', ?2, 'failed', '2026-01-02', '2026-01-02', '/elsewhere')",
        rusqlite::params![TASK, AGENT],
    ).unwrap();
    let journal = f.data.join("nano/sessions/old-matching/events.jsonl");
    let bytes = std::fs::read(&journal).unwrap();
    let workspace =
        workspace::Workspace::new(session.workspace(), workspace::Limits::default()).unwrap();

    assert!(
        crate::nano::resume::recover_previous(&session, &workspace)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(std::fs::read(journal).unwrap(), bytes);
}

#[tokio::test]
async fn recovery_does_not_skip_a_newer_run_from_another_executor() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    previous_run(&f, "old-matching");
    previous_intent(&f, "old-matching", "exec", None, true);
    f.connection()
        .execute("UPDATE runs SET rowid = -1 WHERE id = 'old-matching'", [])
        .unwrap();
    f.connection()
        .execute(
            "INSERT INTO runs (rowid, id, task_id, role, agent, status, started_at, updated_at, workspace_path) \
             VALUES (0, 'newer-agent', ?1, 'executor', 'other-agent', 'failed', '2026-01-02', '2026-01-02', ?2)",
            rusqlite::params![TASK, f.root.to_string_lossy()],
        )
        .unwrap();
    let journal = f.data.join("nano/sessions/old-matching/events.jsonl");
    let bytes = std::fs::read(&journal).unwrap();
    let workspace =
        workspace::Workspace::new(session.workspace(), workspace::Limits::default()).unwrap();

    assert!(
        crate::nano::resume::recover_previous(&session, &workspace)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(std::fs::read(journal).unwrap(), bytes);
}

#[tokio::test]
async fn restored_answer_is_carried_into_the_new_run_without_reasking() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    previous_run(&f, "old-human");
    previous_intent(&f, "old-human", "ask_human", None, true);
    let run_dir = f.root.join(".ferrus/runs/t-001");
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("ANSWER.md"), "Use the bounded implementation.").unwrap();
    let workspace =
        workspace::Workspace::new(session.workspace(), workspace::Limits::default()).unwrap();

    let note = crate::nano::resume::recover_previous(&session, &workspace)
        .await
        .unwrap()
        .unwrap();
    assert!(note.note.contains("Use the bounded implementation."));
    assert_eq!(session.status().await.unwrap().status, "executing");
    assert!(
        !f.events()
            .iter()
            .any(|(kind, _)| kind == "task_human_question")
    );
}

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
async fn completed_submit_starts_a_new_phase_without_inheriting_usage() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    previous_run(&f, "old-submitted");
    previous_intent(&f, "old-submitted", "submit", None, true);
    finish_previous_submit(&f, "old-submitted");
    f.connection()
        .execute(
            "UPDATE tasks SET status = 'addressing' WHERE id = ?1",
            [TASK],
        )
        .unwrap();
    let journal = f.data.join("nano/sessions/old-submitted/events.jsonl");
    let before = std::fs::read(&journal).unwrap();
    let workspace =
        workspace::Workspace::new(session.workspace(), workspace::Limits::default()).unwrap();

    assert!(
        crate::nano::resume::recover_previous(&session, &workspace)
            .await
            .is_err()
    );
    f.connection()
        .execute(
            "INSERT INTO events (run_id, type, payload_json, created_at) \
             VALUES ('old-submitted', 'submitted', '{}', '2026-01-01')",
            [],
        )
        .unwrap();
    assert!(
        crate::nano::resume::recover_previous(&session, &workspace)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(std::fs::read(journal).unwrap(), before);
    assert_eq!(session.status().await.unwrap().status, "addressing");
}

#[tokio::test]
async fn committed_submit_with_lost_response_is_confirmed_after_rejection() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    previous_run(&f, "old-submit");
    previous_intent(&f, "old-submit", "submit", None, true);
    f.connection()
        .execute(
            "UPDATE tasks SET status = 'addressing' WHERE id = ?1",
            [TASK],
        )
        .unwrap();
    f.connection()
        .execute(
            "INSERT INTO events (run_id, type, payload_json, created_at) \
         VALUES ('old-submit', 'submitted', '{}', '2026-01-01')",
            [],
        )
        .unwrap();
    let run_dir = f.root.join(".ferrus/runs/t-001");
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("SUBMISSION.md"), "Ready for review.").unwrap();
    let workspace =
        workspace::Workspace::new(session.workspace(), workspace::Limits::default()).unwrap();

    assert!(
        crate::nano::resume::recover_previous(&session, &workspace)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(session.status().await.unwrap().status, "addressing");
    let (_, records) =
        FileJournal::recover(&f.data.join("nano/sessions/old-submit"), Quotas::default()).unwrap();
    assert!(matches!(
        &records.last().unwrap().event,
        SessionEvent::Ended {
            reason: EndReason::Submitted
        }
    ));
    assert_eq!(
        f.events()
            .iter()
            .filter(|(kind, _)| kind == "submitted")
            .count(),
        1
    );
}

#[tokio::test]
async fn committed_submit_can_be_reconciled_after_legacy_journal_sealing() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    previous_run(&f, "old-submit-sealed");
    previous_intent(&f, "old-submit-sealed", "submit", None, true);
    f.connection()
        .execute(
            "UPDATE tasks SET status = 'addressing' WHERE id = ?1",
            [TASK],
        )
        .unwrap();
    f.connection()
        .execute(
            "INSERT INTO events (run_id, type, payload_json, created_at) \
         VALUES ('old-submit-sealed', 'submitted', '{}', '2026-01-01')",
            [],
        )
        .unwrap();
    let run_dir = f.root.join(".ferrus/runs/t-001");
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("SUBMISSION.md"), "Ready for review.").unwrap();
    let directory = f.data.join("nano/sessions/old-submit-sealed");
    FileJournal::recover(&directory, Quotas::default()).unwrap();
    let workspace =
        workspace::Workspace::new(session.workspace(), workspace::Limits::default()).unwrap();

    assert!(
        crate::nano::resume::recover_previous(&session, &workspace)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(session.status().await.unwrap().status, "addressing");
}

#[tokio::test]
async fn inconsistent_submitted_artifact_is_not_certified() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    previous_run(&f, "old-mismatch");
    previous_intent(&f, "old-mismatch", "submit", None, true);
    f.connection()
        .execute(
            "INSERT INTO events (run_id, type, payload_json, created_at) \
         VALUES ('old-mismatch', 'submitted', '{}', '2026-01-01')",
            [],
        )
        .unwrap();
    let run_dir = f.root.join(".ferrus/runs/t-001");
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("SUBMISSION.md"), "changed after commit").unwrap();
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
async fn uncommitted_submit_cannot_hide_interrupted_check_commands() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    previous_run(&f, "old-submit");
    previous_intent(&f, "old-submit", "submit", None, true);
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
async fn managed_consultation_relaunch_rejects_invalid_resume_and_foreign_live_lease() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    lifecycle::ask(&session, false, QUESTION.into())
        .await
        .unwrap();
    for (resume, owner, until) in [
        (None, AGENT, "2000-01-01T00:00:00Z"),
        (Some("reviewing"), AGENT, "2000-01-01T00:00:00Z"),
        (Some("consultation"), AGENT, "2000-01-01T00:00:00Z"),
        (
            Some("addressing"),
            "executor:nano:2",
            "2999-01-01T00:00:00Z",
        ),
    ] {
        f.connection()
            .execute(
                "UPDATE tasks SET paused_status=?1, claimed_by=?2, lease_until=?3 WHERE id=?4",
                rusqlite::params![resume, owner, until, TASK],
            )
            .unwrap();
        let tasks = project::list_tasks().await.unwrap();
        let events = f.events();
        assert!(matches!(
            session.claim().await.unwrap(),
            ReadyTaskClaim::NoAvailable
        ));
        f.assert_no_effect(tasks, events).await;
    }
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
async fn managed_checks_collect_superseded_graphs_from_the_bound_project() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = git_session(&f, true).await;
    let config = std::fs::read_to_string(f.root.join("ferrus.toml")).unwrap();
    std::fs::write(
        f.root.join("ferrus.toml"),
        format!("{config}\n[repository_graph.retention]\nmax_snapshots = 0\n"),
    )
    .unwrap();
    let baseline = session
        .status()
        .await
        .unwrap()
        .repository_view
        .baseline_snapshot_id
        .unwrap();
    let unrelated = tempfile::tempdir().unwrap();
    std::env::set_current_dir(unrelated.path()).unwrap();
    std::fs::write(f.root.join("src/lib.rs"), "pub struct FirstEdit;\n").unwrap();
    assert_eq!(
        lifecycle::check(&session, &Cancellation::default())
            .await
            .unwrap()["status"],
        "passed"
    );
    let previous = session
        .status()
        .await
        .unwrap()
        .repository_view
        .view_snapshot_id
        .unwrap();
    assert_ne!(previous, baseline);
    let sidecar = Connection::open(f.data.join("repo-graph.db")).unwrap();
    // Retain the baseline through the live task reference alone, not a publication.
    sidecar
        .execute(
            "DELETE FROM published_views WHERE view_name = 'canonical'",
            [],
        )
        .unwrap();
    let previous_content: String = sidecar
        .query_row(
            "SELECT content_digest FROM files WHERE snapshot_id = ?1 AND path = 'src/lib.rs'",
            [previous.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    std::fs::write(f.root.join("src/lib.rs"), "pub struct SecondEdit;\n").unwrap();
    assert_eq!(
        lifecycle::check(&session, &Cancellation::default())
            .await
            .unwrap()["status"],
        "passed"
    );
    let current = session
        .status()
        .await
        .unwrap()
        .repository_view
        .view_snapshot_id
        .unwrap();
    for (id, retained) in [(&baseline, true), (&previous, false), (&current, true)] {
        let exists: bool = sidecar
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM snapshots WHERE id = ?1)",
                [id.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exists, retained, "snapshot {id:?}");
    }
    let fragments: i64 = sidecar
        .query_row(
            "SELECT count(*) FROM fragment_cache WHERE path = 'src/lib.rs' AND content_digest = ?1",
            [previous_content],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(fragments, 0);
    assert_eq!(session.status().await.unwrap().status, "executing");
    assert!(!unrelated.path().join(".ferrus").exists());
}

#[tokio::test]
async fn managed_graph_maintenance_failure_preserves_a_successful_refresh() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = git_session(&f, true).await;
    let previous = session.status().await.unwrap().repository_view;
    // Invalid retention metadata must fail maintenance closed without undoing publication.
    f.connection()
        .execute(
            "UPDATE project_runtime_state SET canonical_graph_snapshot_id = '' WHERE row_id = 1",
            [],
        )
        .unwrap();
    assert!(
        project::repository_graph_retention_references_at(&f.data.join("ferrus.db"))
            .await
            .is_err()
    );
    std::fs::write(f.root.join("src/lib.rs"), "pub struct CheckedEdit;\n").unwrap();
    assert_eq!(
        lifecycle::check(&session, &Cancellation::default())
            .await
            .unwrap()["status"],
        "passed"
    );
    let context = session.status().await.unwrap();
    assert_eq!(context.status, "executing");
    assert_eq!(
        context.repository_view.status,
        project::RepositoryViewStatus::Available
    );
    assert_ne!(
        context.repository_view.view_snapshot_id,
        previous.view_snapshot_id
    );
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

#[tokio::test]
async fn working_set_refresh_publishes_edits_without_lifecycle_checks() {
    use crate::nano::{
        context::{Context, Request, Response},
        refresh::Refresh,
    };
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = git_session(&f, true).await;
    let before = session.status().await.unwrap();
    std::fs::write(f.root.join("src/lib.rs"), "pub struct WorkingSetEdit;\n").unwrap();
    let mut refresh = Refresh::default();
    refresh.invalidate();
    assert_eq!(
        refresh.prepare(&session, false).await[0]["status"],
        "scheduled"
    );
    assert_eq!(refresh.settle().await.unwrap()["status"], "published");
    let after = session.status().await.unwrap();
    assert_eq!(after.status, before.status);
    assert_eq!(after.check_retries, before.check_retries);
    assert_eq!(
        after.repository_view.baseline_snapshot_id,
        before.repository_view.baseline_snapshot_id
    );
    assert_ne!(
        after.repository_view.view_snapshot_id,
        before.repository_view.view_snapshot_id
    );
    let context = Context::new(session);
    let Response::RepositorySearch(Ok(result)) = context
        .retrieve(
            "repository_search",
            Request::parse("repository_search", json!({"query":"WorkingSetEdit"})).unwrap(),
        )
        .await
        .unwrap()
    else {
        panic!("search")
    };
    assert!(!result.data.hits.is_empty());
    assert_eq!(
        Some(result.snapshot_id),
        after.repository_view.view_snapshot_id
    );
}

#[tokio::test]
async fn quick_patch_schedules_refresh_before_the_next_graph_query() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = git_session(&f, true).await;
    let previous = session
        .status()
        .await
        .unwrap()
        .repository_view
        .view_snapshot_id;
    let (mut tools, _journal) = native(&f, session.clone(), "quick-patch-refresh");
    let patch = baseline_patch(&tools, "QuickPatch");
    assert!(
        matches!(tools.execute(&patch, &Cancellation::default()).await,
        ToolOutcome::Success(value) if value["complete"] == true)
    );
    // A graph lookup in the same tool group waits for the already scheduled
    // refresh. No further model turn or second prepare_context is needed.
    let query = call("repository_search", json!({"query":"QuickPatch"}));
    let ToolOutcome::Success(value) = tools.execute(&query, &Cancellation::default()).await else {
        panic!("query failed before the scheduled refresh completed");
    };
    assert!(
        serde_json::to_string(&value)
            .unwrap()
            .contains("QuickPatch")
    );
    assert_ne!(
        session
            .status()
            .await
            .unwrap()
            .repository_view
            .view_snapshot_id,
        previous
    );
    assert!(tools.shutdown().await);
}

#[tokio::test]
async fn working_set_settles_refresh_before_reusing_graph_evidence_without_prefetch() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = git_session(&f, true).await;
    let previous = session
        .status()
        .await
        .unwrap()
        .repository_view
        .view_snapshot_id;
    let (mut tools, _journal) = native(&f, session.clone(), "working-set-after-patch");
    let query = call("repository_search", json!({"query":"Baseline"}));
    let result = tools.execute(&query, &Cancellation::default()).await;
    assert!(
        matches!(&result, ToolOutcome::Success(value) if serde_json::to_string(value).unwrap().contains("Baseline"))
    );
    let messages = vec![
        Message::Assistant {
            response: ModelResponse {
                finish: FinishReason::ToolCalls,
                text: String::new(),
                calls: vec![ToolCall {
                    provider_call_id: query.provider_call_id.clone(),
                    name: query.name.clone(),
                    arguments: query.arguments.to_string(),
                }],
                continuation: None,
            },
        },
        Message::Tool {
            provider_call_id: query.provider_call_id,
            outcome: result,
        },
    ];
    let patch = baseline_patch(&tools, "CurrentSymbol");
    assert!(
        matches!(tools.execute(&patch, &Cancellation::default()).await, ToolOutcome::Success(value) if value["complete"] == true)
    );
    let prepared = tools
        .prepare_context(&messages, &Cancellation::default())
        .await
        .unwrap()
        .unwrap();
    assert!(prepared.replacements.iter().any(|replacement| {
        replacement.message == 1 && replacement.value["kind"] == "evidence_unavailable"
    }));
    assert!(
        !serde_json::to_string(&prepared.apply(&messages).unwrap())
            .unwrap()
            .contains("pub struct Baseline;")
    );
    assert_ne!(
        session
            .status()
            .await
            .unwrap()
            .repository_view
            .view_snapshot_id,
        previous
    );
    assert!(tools.shutdown().await);
}

fn baseline_patch(tools: &NativeTools<TrustedLocal>, symbol: &str) -> ValidatedCall {
    let digest = tools
        .coding
        .workspace
        .read_file(
            serde_json::from_value(json!({
                "path":"src/lib.rs"
            }))
            .unwrap(),
        )
        .unwrap()
        .source
        .digest;
    call(
        "apply_patch",
        json!({"edits":[{
            "operation":"update", "path":"src/lib.rs", "expected_digest":digest,
            "hunks":[{"start_line":1, "old_text":"pub struct Baseline;\n",
                "new_text":format!("pub struct {symbol};\n")}]
        }]}),
    )
}

#[tokio::test]
async fn disabled_working_set_keeps_workspace_tools_but_never_schedules_refresh() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = git_session(&f, true).await;
    let previous = session.status().await.unwrap().repository_view;
    let (mut tools, _journal) = native(&f, session.clone(), "disabled-working-set");
    tools.working_set_enabled = false;
    tools.context.cache_enabled = false;
    let patch = baseline_patch(&tools, "DisabledModeEdit");
    assert!(
        matches!(tools.execute(&patch, &Cancellation::default()).await,
        ToolOutcome::Success(value) if value["complete"] == true)
    );
    let messages = vec![Message::User {
        text: "task".into(),
    }];
    assert!(
        tools
            .prepare_context(&messages, &Cancellation::default())
            .await
            .unwrap()
            .is_none()
    );
    tools.prefetch = vec![json!({"type":"path", "value":"src/lib.rs"})];
    let prepared = tools
        .prepare_context(&messages, &Cancellation::default())
        .await
        .unwrap()
        .unwrap();
    assert!(
        prepared
            .observations
            .iter()
            .any(|item| item["kind"] == "prefetch_unavailable")
    );
    assert!(
        !prepared
            .observations
            .iter()
            .any(|item| item["kind"] == "overlay_refresh")
    );
    assert_eq!(session.status().await.unwrap().repository_view, previous);
    assert!(tools.shutdown().await);
    assert_eq!(session.status().await.unwrap().repository_view, previous);
}

#[tokio::test]
async fn prefetch_waits_for_the_new_overlay_after_a_quick_patch() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = git_session(&f, true).await;
    let previous = session
        .status()
        .await
        .unwrap()
        .repository_view
        .view_snapshot_id;
    let (mut tools, _journal) = native(&f, session.clone(), "prefetch-after-patch");
    tools.prefetch = vec![json!({"type":"path", "value":"src/lib.rs"})];
    let patch = baseline_patch(&tools, "PrefetchedEdit");
    assert!(
        matches!(tools.execute(&patch, &Cancellation::default()).await,
        ToolOutcome::Success(value) if value["complete"] == true)
    );
    let prepared = tools
        .prepare_context(
            &[Message::User {
                text: "task".into(),
            }],
            &Cancellation::default(),
        )
        .await
        .unwrap()
        .unwrap();
    let prefetched = prepared
        .observations
        .iter()
        .find(|item| item["kind"] == "prefetch")
        .expect("prefetch should use the new task view");
    let view = session.status().await.unwrap().repository_view;
    assert_ne!(view.view_snapshot_id, previous);
    assert_eq!(
        prefetched["evidence"]["provenance"]["snapshot_id"],
        json!(view.view_snapshot_id)
    );
    assert!(
        !serde_json::to_string(prefetched)
            .unwrap()
            .contains("pub struct Baseline;")
    );
    assert!(tools.shutdown().await);
}

#[tokio::test]
async fn shutdown_refreshes_an_overlay_deferred_by_an_active_writer() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = git_session(&f, true).await;
    let previous = session
        .status()
        .await
        .unwrap()
        .repository_view
        .view_snapshot_id;
    let (mut tools, _journal) = native(&f, session.clone(), "writer-at-shutdown");
    let command = if cfg!(windows) {
        r"echo pub struct ShutdownEdit;> src\lib.rs & ping -n 30 127.0.0.1 > nul"
    } else {
        "printf 'pub struct ShutdownEdit;\\n' > src/lib.rs; sleep 30"
    };
    let started = tools
        .execute(
            &call(
                "exec",
                json!({"command":command, "cwd":".", "timeout_ms":30_000}),
            ),
            &Cancellation::default(),
        )
        .await;
    assert!(matches!(started, ToolOutcome::Success(_)));
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if std::fs::read_to_string(f.root.join("src/lib.rs"))
                .is_ok_and(|source| source.contains("ShutdownEdit"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("command must write before shutdown");
    assert!(tools.coding.commands.potentially_active_writers() > 0);
    assert_eq!(
        session
            .status()
            .await
            .unwrap()
            .repository_view
            .view_snapshot_id,
        previous
    );
    assert!(tools.shutdown().await);
    assert_ne!(
        session
            .status()
            .await
            .unwrap()
            .repository_view
            .view_snapshot_id,
        previous
    );
}

#[tokio::test]
async fn explicit_prefetch_is_read_only_host_evidence_and_rechecks_source_bytes() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = git_session(&f, true).await;
    let (mut tools, _journal) = native(&f, session, "prefetch-test");
    tools.working_set_enabled = false;
    tools.context.cache_enabled = false;
    let messages = vec![Message::User {
        text: "bound task and constraints".into(),
    }];
    assert!(
        tools
            .prepare_context(&messages, &Cancellation::default())
            .await
            .unwrap()
            .is_none()
    );
    tools.prefetch = vec![json!({"type":"path", "value":"src/lib.rs"})];
    let events = f.events();
    let tasks = project::list_tasks().await.unwrap();
    let prepared = tools
        .prepare_context(&messages, &Cancellation::default())
        .await
        .unwrap()
        .unwrap();
    assert!(prepared.replacements.is_empty());
    assert_eq!(prepared.observations.len(), 1);
    assert_eq!(prepared.observations[0]["kind"], "prefetch");
    assert_eq!(
        prepared.observations[0]["evidence"]["binding"]["task"],
        TASK
    );
    assert!(
        !prepared.observations[0]["evidence"]["sources"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let projected = prepared.apply(&messages).unwrap();
    assert_eq!(projected.len(), 2);
    assert_eq!(projected[0], messages[0]);
    assert!(
        matches!(&projected[1], Message::User {text} if text.contains("Ferrus host observation") && text.contains("pub struct Baseline;"))
    );
    std::fs::write(f.root.join("src/lib.rs"), "pub struct ExternalChange;\n").unwrap();
    let next = tools
        .prepare_context(&messages, &Cancellation::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(next.observations[0]["kind"], "prefetch_unavailable");
    assert!(
        !serde_json::to_string(&next.apply(&messages).unwrap())
            .unwrap()
            .contains("pub struct Baseline;")
    );
    assert_eq!(messages.len(), 1);
    f.assert_no_effect(tasks, events).await;
    assert!(tools.shutdown().await);
}
