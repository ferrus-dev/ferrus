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
                system_prompt: None,
                launch_evidence: None,
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

struct RejectedProvider(ProviderErrorKind, bool);

impl Provider for RejectedProvider {
    async fn start(&mut self, _: ModelRequest) -> std::result::Result<(), ProviderError> {
        Err(ProviderError::new(self.0.clone(), self.1))
    }

    async fn next_event(&mut self) -> std::result::Result<Option<ProviderEvent>, ProviderError> {
        panic!("Rejected request cannot have events")
    }
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

mod reviewer_profiles {
    use super::*;

    include!("review/lifecycle_tests.rs");
}
#[path = "lifecycle_tests/behavior.rs"]
mod behavior_tests;
#[path = "lifecycle_tests/cancellation.rs"]
mod cancellation_tests;
#[path = "lifecycle_tests/checks.rs"]
mod checks_tests;
#[path = "lifecycle_tests/compaction.rs"]
mod compaction_tests;
#[path = "lifecycle_tests/human_waits.rs"]
mod human_waits_tests;
#[path = "lifecycle_tests/ownership.rs"]
mod ownership_tests;
#[path = "lifecycle_tests/recovery.rs"]
mod recovery_tests;
#[path = "lifecycle_tests/review.rs"]
mod review_tests;
#[path = "lifecycle_tests/submission.rs"]
mod submission_tests;

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
