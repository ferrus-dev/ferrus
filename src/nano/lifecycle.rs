//! Native task operations. SQLite transitions share the MCP implementation.

use super::{ferrus::FerrusSession, tools::Cancellation};
use crate::{
    config::Config,
    project::{self, RuntimeTaskContext, TaskStatus},
    repository_graph::{
        domain::Digest,
        source::{capture_worktree_tree, parse_git_tree_digest, release_submitted_tree_pin},
    },
    repository_graph_runtime::{self as graph, LocalGraphContext},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

pub(super) const NAMES: &[&str] = &["check", "submit", "consult", "ask_human"];
static LOG_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn working(context: &RuntimeTaskContext) -> Result<()> {
    ensure!(
        context.status.parse::<TaskStatus>()?.is_executor_working(),
        "Task is not executing or addressing"
    );
    Ok(())
}

async fn graph_context(
    session: &FerrusSession,
    context: &RuntimeTaskContext,
) -> Result<LocalGraphContext> {
    LocalGraphContext::load_for_runtime(
        session.project_root(),
        session.project_id(),
        session.data_dir(),
        context,
    )
    .await
}

async fn refresh(session: &FerrusSession, context: &RuntimeTaskContext) {
    let Some(baseline) = session.baseline_tree() else {
        return;
    };

    let result = async {
        graph::refresh_task_overlay_explicit(
            graph_context(session, context).await?,
            session.data_dir(),
            context,
            baseline,
        )
        .await
    }
    .await;

    if let Err(error) = result {
        tracing::warn!(
            ?error,
            "Nano task overlay refresh failed; lifecycle is unchanged"
        );
    }
}

pub(super) async fn check(session: &FerrusSession, stop: &Cancellation) -> Result<Value> {
    run_check(session, stop, false).await
}

async fn run_check(
    session: &FerrusSession,
    stop: &Cancellation,
    final_gate: bool,
) -> Result<Value> {
    let context = session.authorize().await?;
    working(&context)?;

    let config = Config::load_from(session.project_root()).await?;
    let sequence = LOG_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();

    let logs = session.project_root().join(".ferrus/logs");
    tokio::fs::create_dir_all(&logs).await?;

    let scope: String = session
        .scope
        .run_id
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect();

    let log = logs.join(format!(
        "check_{}_{}-{}-{timestamp}-{sequence}.txt",
        context.check_retries + 1,
        scope,
        std::process::id()
    ));

    let (passed, report, failure) =
        super::checks::run(&config, session.workspace(), &log, stop).await?;

    ensure!(!stop.is_cancelled(), "Check interrupted");
    session.authorize().await?;

    refresh(session, &context).await;

    let stop = stop.clone();
    let result = session.mutate(move |tx, scope, context| {
        ensure!(!stop.is_cancelled(), "Check interrupted");
        working(&context)?;

        if passed {
            if !final_gate {
                project::task_check_passed_in_transaction(tx, scope.task_id.clone())?;
                project::executor_event(tx, scope, "check_passed", json!({"commands":config.checks.commands.len()}))?;
            }
            Ok(json!({"status":"passed", "task_state":context.status}))
        } else {
            let outcome = project::task_check_failed_in_transaction(tx, scope.task_id.clone(), failure, config.limits.max_check_retries)?;
            let (retries, failed) = match outcome {
                project::TaskCheckFailure::Failed { retries } => (retries, false),
                project::TaskCheckFailure::LimitExceeded { retries } => (retries, true),
            };
            let kind = match (final_gate, failed) {
                (false, false) => "check_failed", (false, true) => "check_limit_exceeded",
                (true, false) => "submit_check_failed", (true, true) => "submit_check_limit_exceeded",
            };
            project::executor_event(tx, scope, kind, json!({"task_id":scope.task_id, "retries":retries, "max_retries":config.limits.max_check_retries, "state":context.status}))?;
            Ok(json!({"status":"failed", "task_state":if failed { "failed" } else { &context.status }, "retries":retries}))
        }
    }).await?;

    if passed {
        let _ = tokio::fs::remove_file(&log).await;
    }

    let mut result = result;
    result["report"] = json!(report);

    if !passed {
        result["log"] = json!(log);
    }

    Ok(result)
}

/// On drop, an uncommitted prepared tree must not leave a submitted-tree pin.
struct Pin {
    workspace: PathBuf,
    task: String,
    database: PathBuf,
    run: String,
    armed: bool,
    commit_attempted: bool,
    commit_started: bool,
}

impl Drop for Pin {
    fn drop(&mut self) {
        if self.should_release() {
            let _ = release_submitted_tree_pin(&self.workspace, &self.task);
        }
    }
}

impl Pin {
    fn should_release(&self) -> bool {
        self.armed
            && !self.commit_started
            && (!self.commit_attempted || self.submission_committed() == Some(false))
    }

    fn submission_committed(&self) -> Option<bool> {
        use rusqlite::{OpenFlags, TransactionBehavior};

        // The async caller can be dropped while the SQLite transaction is still
        // committing on a blocking worker. Serialize with that writer before
        // deleting the ref; a busy/unreadable database leaves a safe orphan.
        let mut connection = rusqlite::Connection::open_with_flags(
            &self.database,
            OpenFlags::SQLITE_OPEN_READ_WRITE,
        )
        .ok()?;
        connection.busy_timeout(Duration::from_millis(100)).ok()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .ok()?;
        transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM events WHERE run_id = ?1 AND type = 'submitted')",
                [&self.run],
                |row| row.get(0),
            )
            .ok()
    }
}

#[cfg(test)]
mod pin_tests {
    use super::*;

    #[test]
    fn pin_survives_an_unsettled_commit_and_an_authoritative_submission() {
        let root = tempfile::TempDir::new().unwrap();
        let database = root.path().join("ferrus.db");
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch("CREATE TABLE events (run_id TEXT, type TEXT)")
            .unwrap();
        let mut pin = Pin {
            workspace: root.path().into(),
            task: "task".into(),
            database,
            run: "run".into(),
            armed: true,
            commit_attempted: true,
            commit_started: true,
        };
        assert!(!pin.should_release());
        pin.commit_started = false;
        assert!(pin.should_release());
        pin.commit_attempted = false;
        assert!(pin.should_release());
        pin.commit_attempted = true;
        connection
            .execute(
                "INSERT INTO events (run_id, type) VALUES ('run', 'submitted')",
                [],
            )
            .unwrap();
        assert!(!pin.should_release());
    }
}

async fn tree(session: &FerrusSession) -> Result<Option<Digest>> {
    if session.baseline_tree().is_none() {
        return Ok(None);
    }

    let root = session.workspace().to_path_buf();
    Ok(Some(
        tokio::task::spawn_blocking(move || capture_worktree_tree(root)).await??,
    ))
}

async fn stamp(session: &FerrusSession) -> Result<String> {
    let root = session.workspace().to_path_buf();
    let git = session.baseline_tree().is_some();
    tokio::task::spawn_blocking(move || source_stamp(&root, git)).await?
}

fn source_stamp(root: &Path, git: bool) -> Result<String> {
    if git {
        Ok(capture_worktree_tree(root)?.value().into())
    } else {
        super::workspace::source_stamp(root)
    }
}

pub(super) async fn submit(
    session: &FerrusSession,
    content: String,
    stop: &Cancellation,
) -> Result<Value> {
    let initial = session.authorize().await?;
    working(&initial)?;

    let source = tree(session).await?;
    let expected = match &source {
        Some(source) => source.value().to_owned(),
        None => stamp(session).await?,
    };

    // Preserve both required gates: /check immediately before the final submit gate.
    for final_gate in [false, true] {
        let result = run_check(session, stop, final_gate).await?;
        if result["status"] != "passed" {
            return Ok(result);
        }

        ensure!(
            stamp(session).await? == expected,
            "Workspace changed during review checks; submit again after checking the new source"
        );
    }

    let context = session.authorize().await?;
    let mut pin = Pin {
        workspace: session.workspace().into(),
        task: context.task_id.clone(),
        database: session.scope.database_path.clone(),
        run: session.scope.run_id.clone(),
        armed: false,
        commit_attempted: false,
        commit_started: false,
    };

    let mut freeze_failed = false;
    let frozen = if let Some(source) = &source {
        let result = async {
            let graph = graph_context(session, &context).await?;
            if !graph.config.enabled {
                return Ok(None);
            }

            let view = context.repository_view.clone();
            let snapshot = view
                .view_snapshot_id
                .clone()
                .context("Task graph was not materialized")?;

            let path = session.data_dir().join("repo-graph.db");
            let root = session.workspace().to_path_buf();
            let task = context.task_id.clone();

            let captured = tokio::task::spawn_blocking(move || {
                graph::capture_matching_submitted_tree(
                    &path,
                    &root,
                    &task,
                    graph.repository,
                    &graph.config,
                    &snapshot,
                )
            })
            .await??;

            pin.armed = true;
            ensure!(&captured == source, "Workspace changed during graph freeze");

            Ok::<_, anyhow::Error>(Some(view.frozen(captured)?))
        }
        .await;

        match result {
            Ok(view) => view,
            Err(error) => {
                tracing::warn!(?error, "Nano graph freeze failed; submit will continue");
                freeze_failed = true;
                None
            }
        }
    } else {
        None
    };

    let skipped = Config::load_from(session.project_root())
        .await?
        .checks
        .commands
        .is_empty();

    let keep_pin = frozen.is_some();
    let patch = match (&source, session.baseline_tree()) {
        (Some(source), Some(baseline)) if session.workspace() != session.project_root() => Some(
            crate::server::tools::submit::tree_patch_between(
                session.workspace(),
                &parse_git_tree_digest(baseline)?,
                source,
            )
            .await?,
        ),
        _ => None,
    };

    ensure!(!stop.is_cancelled(), "Submit interrupted");
    ensure!(
        stamp(session).await? == expected,
        "Workspace changed before handoff"
    );

    let root = session.project_root().to_path_buf();
    let workspace = session.workspace().to_path_buf();
    let git = session.baseline_tree().is_some();
    let task_id = context.task_id.clone();
    let stop = stop.clone();

    // If this future is dropped, the blocking transaction may still commit.
    // Preserve the pin until a later authoritative cleanup can decide.
    pin.commit_attempted = true;
    pin.commit_started = true;
    let publication = session
        .mutate(move |tx, scope, context| {
            working(&context)?;

            ensure!(!stop.is_cancelled(), "Submit interrupted");
            ensure!(
                source_stamp(&workspace, git)? == expected,
                "Workspace changed at handoff"
            );

            project::require_executor_owner(tx, scope)?;
            ensure!(!stop.is_cancelled(), "Submit interrupted");

            let directory = root.join(&context.run_dir);
            std::fs::create_dir_all(&directory)?;

            write(&directory, "SUBMISSION.md", &content)?;
            clear(&directory, "INTEGRATION_ERROR.md")?;

            match patch {
                Some(patch) => write(&directory, "PATCH.diff", &patch)?,
                None => clear(&directory, "PATCH.diff")?,
            }

            project::task_check_passed_in_transaction(tx, scope.task_id.clone())?;
            project::task_submitted_in_transaction(
                tx,
                scope.task_id.clone(),
                context.task_path,
                Some(scope.run_id.clone()),
                frozen,
                freeze_failed,
            )?;

            project::executor_event(tx, scope, "submitted", json!({"content_bytes":content.len(), "check_gate":if skipped { "skipped" } else { "passed" }}))
        })
        .await;
    pin.commit_started = false;
    publication?;

    if keep_pin {
        pin.armed = false;
    }

    Ok(json!({"status":"submitted", "task_state":"reviewing", "task_id":task_id}))
}

// Artifact paths are host-owned and operations run inside the owner-fenced transaction.
fn write(directory: &Path, name: &str, content: &str) -> Result<()> {
    super::workspace::runtime_artifact(directory, name, Some(content))
}

fn clear(directory: &Path, name: &str) -> Result<()> {
    super::workspace::runtime_artifact(directory, name, None)
}

pub(super) async fn ask(session: &FerrusSession, human: bool, question: String) -> Result<()> {
    if !human {
        crate::server::tools::consult::validate_consult_request(&question)?;
    }

    let root = session.project_root().to_path_buf();
    session
        .mutate(move |tx, scope, context| {
            working(&context)?;
            let status = context.status.parse::<TaskStatus>()?;
            let directory = root.join(&context.run_dir);
            std::fs::create_dir_all(&directory)?;

            if human {
                clear(&directory, "ANSWER.md")?;
                write(&directory, "QUESTION.md", &question)?;

                project::task_human_question_in_transaction(
                    tx,
                    scope.task_id.clone(),
                    status,
                    Some(status),
                    scope.agent_id.clone(),
                )
            } else {
                clear(&directory, "CONSULT_RESPONSE.md")?;
                write(&directory, "CONSULT_REQUEST.md", &question)?;

                project::task_consultation_in_transaction(tx, scope.task_id.clone(), status)
            }
        })
        .await
}

pub(super) async fn poll_answer(session: &FerrusSession, human: bool) -> Result<Option<Value>> {
    poll_answer_checked(session, human, |_| Ok(())).await
}

/// Validate delivery before committing restoration. Keep the response until a
/// durable Nano record can prove delivery across a process crash.
pub(super) async fn poll_answer_checked(
    session: &FerrusSession,
    human: bool,
    validate: impl FnOnce(&Value) -> Result<()> + Send + 'static,
) -> Result<Option<Value>> {
    let root = session.project_root().to_path_buf();
    session
        .mutate(move |tx, scope, context| {
            // A Consultant may ask the human while the Executor is still waiting.
            // Only the Consultant consumes that answer; keep waiting for its response.
            if !human && context.status == "awaiting_human" {
                let resume: Option<String> = tx.query_row(
                    "SELECT awaiting_human_status FROM tasks WHERE id = ?1",
                    [&scope.task_id],
                    |row| row.get(0),
                )?;

                ensure!(
                    resume.as_deref() == Some("consultation"),
                    "Task left consultation"
                );

                return Ok(None);
            }
            ensure!(
                context.status
                    == if human {
                        "awaiting_human"
                    } else {
                        "consultation"
                    },
                "Task no longer waiting"
            );

            if human {
                let owner: Option<String> = tx.query_row(
                    "SELECT awaiting_human_by FROM tasks WHERE id = ?1",
                    [&scope.task_id],
                    |row| row.get(0),
                )?;

                ensure!(
                    owner.as_deref() == Some(&scope.agent_id),
                    "Question belongs to another agent"
                );
            }
            let directory = root.join(&context.run_dir);
            let name = if human {
                "ANSWER.md"
            } else {
                "CONSULT_RESPONSE.md"
            };

            let Some((text, _)) = super::workspace::instruction_file(
                &root,
                &format!("{}/{name}", context.run_dir),
                16 * 1024,
            )?
            else {
                return Ok(None);
            };

            if text.trim().is_empty() {
                return Ok(None);
            }

            super::journal::encode(
                &json!({"status":"answered", "answer":text.trim(), "resumed_state":"consultation"}),
                24 * 1024,
            )?;

            let status = if human {
                match project::task_restore_human_in_transaction(tx, scope.task_id.clone())? {
                    project::TaskHumanAnswerRestore::Restored { status } => status,
                    _ => anyhow::bail!("Answer already consumed"),
                }
            } else {
                match project::task_restore_consultation_in_transaction(tx, scope.task_id.clone())?
                {
                    project::TaskConsultRestore::Restored { status } => status,
                    _ => anyhow::bail!("Consultation already consumed"),
                }
            };

            let result = json!({"status":"answered", "answer":text.trim(), "resumed_state":status});
            validate(&result)?;
            clear(
                &directory,
                if human {
                    "QUESTION.md"
                } else {
                    "CONSULT_REQUEST.md"
                },
            )?;

            Ok(Some(result))
        })
        .await
}
