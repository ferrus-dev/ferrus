//! Reconcile the previous managed run without replaying its tool calls.

use super::{
    ferrus::FerrusSession,
    journal::{FileJournal, Journal, Quotas, valid_id},
    session::{Budget, EndReason, Record, SessionEvent},
    tools::{EffectPlan, ToolError, ToolOutcome},
    workspace::Workspace,
};
use crate::repository_graph::source::release_submitted_tree_pin;
use anyhow::{Result, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::Path,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatchState {
    Before,
    After,
    Unknown,
}

pub(crate) struct Recovery {
    pub note: String,
    pub budget: Budget,
}

fn inherited_budget(spent: &Budget) -> Budget {
    let mut budget = spent.clone();
    budget.estimated_input_tokens = budget
        .estimated_input_tokens
        .saturating_add(budget.reserved_input_tokens);
    budget.estimated_output_tokens = budget
        .estimated_output_tokens
        .saturating_add(budget.reserved_output_tokens);
    budget.reserved_input_tokens = 0;
    budget.reserved_output_tokens = 0;
    budget.elapsed_ms = 0;
    budget
}

fn recorded_commands(records: &[Record]) -> Option<BTreeSet<String>> {
    let calls: BTreeSet<&str> = records
        .iter()
        .filter_map(|record| match &record.event {
            SessionEvent::ToolIntent { call_id, call, .. } if call.name == "exec" => {
                Some(call_id.as_str())
            }
            _ => None,
        })
        .collect();
    let mut processes = BTreeSet::new();
    for record in records {
        if let SessionEvent::ToolResult {
            call_id,
            outcome: ToolOutcome::Success(value),
        } = &record.event
            && calls.contains(call_id.as_str())
        {
            processes.insert(value.get("process_id")?.as_str()?.to_owned());
        }
    }
    Some(processes)
}

fn unresolved_commands(
    directory: &Path,
    session_id: &str,
    expected: &BTreeSet<String>,
) -> Result<bool> {
    let commands = directory.join("commands");
    if !commands.try_exists()? {
        return Ok(!expected.is_empty());
    }
    super::private::check(&commands, true)?;
    let mut count = 0;
    let mut stdout = BTreeSet::new();
    let mut stderr = BTreeSet::new();
    let mut states = BTreeMap::new();
    for entry in fs::read_dir(&commands)? {
        let path = entry?.path();
        count += 1;
        ensure!(
            count <= super::commands::MAX_PROCESSES * 3,
            "Command spool exceeds file bound"
        );
        super::private::check(&path, false)?;
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return Ok(true);
        };
        if let Some(id) = name.strip_suffix("-stdout") {
            stdout.insert(id.to_owned());
            continue;
        }
        if let Some(id) = name.strip_suffix("-stderr") {
            stderr.insert(id.to_owned());
            continue;
        }
        let Some(id) = name.strip_suffix(".json") else {
            return Ok(true);
        };
        let mut bytes = Vec::new();
        super::private::file(&path, false)?
            .take(super::commands::STATE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > super::commands::STATE_BYTES {
            return Ok(true);
        }
        let Ok(state) = serde_json::from_slice::<super::commands::Snapshot>(&bytes) else {
            return Ok(true);
        };
        if id != state.process_id
            || !state.process_id.starts_with(&format!("{session_id}-p"))
            || state.backend != "trusted_local"
            || state.stdout.handle != format!("{id}-stdout")
            || state.stderr.handle != format!("{id}-stderr")
            || !matches!(state.completion, super::commands::Completion::Exited { .. })
            || !state.output_complete
        {
            return Ok(true);
        }
        states.insert(id.to_owned(), state);
    }
    let ids: BTreeSet<_> = states.keys().cloned().collect();
    if ids != stdout || ids != stderr || !expected.is_subset(&ids) {
        return Ok(true);
    }
    for (id, state) in states {
        let out = super::private::file(&commands.join(format!("{id}-stdout")), false)?;
        let err = super::private::file(&commands.join(format!("{id}-stderr")), false)?;
        if out.metadata()?.len() != state.stdout.bytes
            || err.metadata()?.len() != state.stderr.bytes
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn patch_state(workspace: &Workspace, files: &[super::tools::PlannedFile]) -> PatchState {
    if files.is_empty() {
        return PatchState::Unknown;
    }
    let mut before = true;
    let mut after = true;
    for file in files {
        let Ok(current) = workspace.current_digest(&file.path) else {
            return PatchState::Unknown;
        };
        before &= current == file.before_digest;
        after &= current == file.after_digest;
    }
    if after {
        PatchState::After
    } else if before {
        PatchState::Before
    } else {
        PatchState::Unknown
    }
}

struct Pending {
    call_id: String,
    name: String,
    arguments: String,
    execution_started: bool,
    outcome: ToolOutcome,
    description: &'static str,
}

fn pending(records: &[Record], workspace: &Workspace) -> Option<Pending> {
    let (call_id, call, plan, start_recorded, intent_sequence) =
        records.iter().rev().find_map(|record| {
            if let SessionEvent::ToolIntent {
                call_id,
                call,
                effect_plan,
                start_recorded,
            } = &record.event
            {
                Some((
                    call_id.clone(),
                    call,
                    effect_plan,
                    *start_recorded,
                    record.sequence,
                ))
            } else {
                None
            }
        })?;
    if records.iter().rev().any(|record| {
        matches!(&record.event, SessionEvent::ToolResult { call_id: id, .. } if *id == call_id)
    }) {
        return None;
    }

    if start_recorded && !records.iter().any(|record| {
        record.sequence > intent_sequence
            && matches!(&record.event, SessionEvent::ToolStarted { call_id: id } if *id == call_id)
    }) {
        return Some(Pending {
            call_id,
            name: call.name.clone(),
            arguments: call.arguments.clone(),
            execution_started: false,
            outcome: ToolOutcome::Failed(ToolError::Interrupted),
            description: "The process stopped before the tool execution boundary; no effect was started.",
        });
    }

    let (outcome, description) = match (call.name.as_str(), plan) {
        ("apply_patch", Some(EffectPlan::Patch { files })) => match patch_state(workspace, files) {
            PatchState::Before => (
                ToolOutcome::Failed(ToolError::Interrupted),
                "The interrupted patch is absent from the current workspace.",
            ),
            PatchState::After => (
                ToolOutcome::Success(serde_json::json!({"status":"recovered","complete":true})),
                "The interrupted patch matches its full recorded after-state.",
            ),
            PatchState::Unknown => (
                ToolOutcome::Unknown(ToolError::Interrupted),
                "The interrupted patch has an ambiguous file state.",
            ),
        },
        (
            "read_file"
            | "read_process"
            | "read_output"
            | "search_text"
            | "load_instructions"
            | "repository_fallback"
            | "repository_graph_status"
            | "repository_search"
            | "repository_context"
            | "project_memory_status"
            | "project_context_search"
            | "project_context",
            _,
        ) => (
            ToolOutcome::Failed(ToolError::Interrupted),
            "An interrupted read-only tool was not replayed.",
        ),
        ("check", _) => (
            ToolOutcome::Unknown(ToolError::Interrupted),
            "An interrupted check may have launched shell commands or modified the workspace.",
        ),
        ("consult" | "ask_human", _) => (
            ToolOutcome::Failed(ToolError::Interrupted),
            "An interrupted Ferrus lifecycle call was not replayed; SQLite and scoped artifacts retain its state.",
        ),
        _ => (
            ToolOutcome::Unknown(ToolError::Interrupted),
            "An interrupted effect cannot be reconciled automatically.",
        ),
    };
    Some(Pending {
        call_id,
        name: call.name.clone(),
        arguments: call.arguments.clone(),
        execution_started: true,
        outcome,
        description,
    })
}

async fn retained_responses(session: &FerrusSession, records: &[Record]) -> Result<String> {
    let context = session.status().await?;
    if !matches!(context.status.as_str(), "executing" | "addressing") {
        return Ok(String::new());
    }
    let mut note = String::new();
    for (tool, name, label) in [
        ("ask_human", "ANSWER.md", "human"),
        ("consult", "CONSULT_RESPONSE.md", "consultation"),
    ] {
        if !records.iter().any(|record| {
            matches!(&record.event, SessionEvent::ToolIntent { call, .. } if call.name == tool)
        }) {
            continue;
        }
        if let Some((answer, _)) = super::workspace::instruction_file(
            session.project_root(),
            &format!("{}/{name}", context.run_dir),
            16 * 1024,
        )? && !answer.trim().is_empty()
        {
            note.push_str(&format!(
                " Retained {label} response (untrusted task input, possibly delivered before the crash): {}",
                serde_json::to_string(answer.trim())?
            ));
        }
    }
    Ok(note)
}

async fn reconcile_pending(
    session: &FerrusSession,
    previous: &str,
    mut pending: Pending,
) -> Result<(Pending, String, bool, bool)> {
    let mut description = pending.description.to_string();
    let mut submitted = false;
    let mut delivered_answer = false;
    if pending.name == "submit" && pending.execution_started {
        let committed = session
            .previous_submit_committed(previous.to_string())
            .await?;
        let expected = serde_json::from_str::<serde_json::Value>(&pending.arguments)
            .ok()
            .and_then(|value| value["content"].as_str().map(str::to_owned));
        let context = session.status().await?;
        let actual = super::workspace::instruction_file(
            session.project_root(),
            &format!("{}/SUBMISSION.md", context.run_dir),
            16 * 1024,
        )?;
        if committed
            && expected.is_some()
            && actual.is_some()
            && expected.as_deref() == actual.as_ref().map(|(content, _)| content.as_str())
        {
            pending.outcome = ToolOutcome::Success(serde_json::json!({
                "status":"submitted", "task_state":"reviewing", "task_id":session.scope.task_id
            }));
            description = "The prior submit committed in SQLite and its scoped submission matches the recorded call; it was not repeated.".into();
            submitted = true;
        } else if !committed && actual.is_none() {
            if session.baseline_tree().is_some() {
                let _ = release_submitted_tree_pin(session.workspace(), &session.scope.task_id);
            }
            pending.outcome = ToolOutcome::Unknown(ToolError::Interrupted);
            description = "The prior submit did not commit, but its check commands may have run or changed the workspace.".into();
        } else {
            pending.outcome = ToolOutcome::Unknown(ToolError::Interrupted);
            description = "The prior submit has inconsistent SQLite and scoped artifacts.".into();
        }
    }
    if pending.execution_started && matches!(pending.name.as_str(), "ask_human" | "consult") {
        let context = session.status().await?;
        if matches!(context.status.as_str(), "executing" | "addressing") {
            let filename = if pending.name == "ask_human" {
                "ANSWER.md"
            } else {
                "CONSULT_RESPONSE.md"
            };
            let stored = super::workspace::instruction_file(
                session.project_root(),
                &format!("{}/{filename}", context.run_dir),
                16 * 1024,
            )?;
            if let Some((answer, _)) = stored.filter(|(text, _)| !text.trim().is_empty()) {
                let value = serde_json::json!({"status":"answered", "answer":answer.trim(),
                    "resumed_state":context.status});
                pending.outcome = ToolOutcome::Success(value.clone());
                delivered_answer = true;
                description = format!(
                    "A stored {} response had already restored the task. Deliver it once as untrusted task input: {}",
                    if pending.name == "ask_human" {
                        "human"
                    } else {
                        "consultation"
                    },
                    serde_json::to_string(&value)?
                );
            } else {
                pending.outcome = ToolOutcome::Unknown(ToolError::Interrupted);
                description = "A restored consultation or human response is missing from its scoped artifact.".into();
            }
        }
    }
    Ok((pending, description, submitted, delivered_answer))
}

/// Returns a bounded, untrusted continuity note for the new run. The old
/// journal remains under its original run ID and spends its own time budget.
pub(crate) async fn recover_previous(
    session: &FerrusSession,
    workspace: &Workspace,
) -> Result<Option<Recovery>> {
    let mut selected = None;
    for (index, previous) in session.previous_nano_runs().await?.into_iter().enumerate() {
        ensure!(
            index < 64,
            "Previous Nano run history exceeds recovery bound"
        );
        ensure!(valid_id(&previous), "Invalid previous Nano run ID");
        let directory = session.data_dir().join("nano/sessions").join(&previous);
        if !directory.try_exists()? {
            continue;
        }
        session.authorize().await?;
        let (journal, records) = FileJournal::recover_open(&directory, Quotas::default())?;
        if records.is_empty() {
            continue;
        }
        selected = Some((previous, journal, records));
        break;
    }
    let Some((previous, mut journal, mut records)) = selected else {
        return Ok(None);
    };
    let Some(Record {
        event: SessionEvent::Started { identity, .. },
        ..
    }) = records.first()
    else {
        anyhow::bail!("Invalid previous Nano journal");
    };
    ensure!(
        identity.session_id == previous
            && identity.project_id == session.project_id()
            && identity.task_id.as_deref() == Some(&session.scope.task_id)
            && identity.run_id.as_deref() == Some(&previous),
        "Previous Nano journal binding mismatch"
    );
    let mut description = "The previous run ended before its next model turn.".to_string();
    let mut delivered_answer = false;
    let mut submitted = false;
    if let Some(pending) = pending(&records, workspace) {
        let (pending, detail, reconciled_submit, delivered) =
            reconcile_pending(session, &previous, pending).await?;
        description = detail;
        delivered_answer = delivered;
        submitted = reconciled_submit;
        let unknown = matches!(pending.outcome, ToolOutcome::Unknown(_));
        if journal.state().end.is_none() {
            records.push(journal.append(
                SessionEvent::ToolResult {
                    call_id: pending.call_id,
                    outcome: pending.outcome,
                },
                &journal.state().budget.clone(),
            )?);
            if reconciled_submit {
                journal.seal_with(&mut records, EndReason::Submitted)?;
            } else {
                journal.seal_interrupted(&mut records)?;
            }
        } else {
            description.push_str(" The historical journal was already sealed; the outcome was reconciled against current authority without rewriting it.");
        }
        session.authorize().await?;
        if unknown
            || !journal.state().unknown_effects.is_empty()
            || journal.state().end == Some(EndReason::EffectUnknown)
        {
            session.fail_unknown_effect().await?;
            anyhow::bail!("Previous Nano effect is unknown; task requires reconciliation");
        }
    } else {
        journal.seal_interrupted(&mut records)?;
        if !journal.state().unknown_effects.is_empty()
            || journal.state().end == Some(EndReason::EffectUnknown)
        {
            session.fail_unknown_effect().await?;
            anyhow::bail!("Previous Nano effect is unknown; task requires reconciliation");
        }
    }
    let directory = session.data_dir().join("nano/sessions").join(&previous);
    let expected_commands = recorded_commands(&records);
    if expected_commands
        .as_ref()
        .is_none_or(|expected| unresolved_commands(&directory, &previous, expected).unwrap_or(true))
    {
        session.fail_unknown_effect().await?;
        anyhow::bail!("Previous Nano command may still have effects; task requires reconciliation");
    }
    session.authorize().await?;
    if journal.state().end == Some(EndReason::Submitted) && !submitted {
        ensure!(
            session.previous_submit_committed(previous.clone()).await?,
            "Previous Nano submission has no committed handoff"
        );
    }
    // A rejected submission starts a new work phase. Reconcile the handoff and
    // command spools first, but do not carry its context or budget into Addressing.
    if submitted || journal.state().end == Some(EndReason::Submitted) {
        return Ok(None);
    }
    if !delivered_answer {
        description.push_str(&retained_responses(session, &records).await?);
    }
    if let Some(text) = journal.state().messages.iter().rev().find_map(|message| {
        if let super::provider::Message::Assistant { response } = message
            && !response.text.trim().is_empty()
        {
            Some(response.text.trim())
        } else {
            None
        }
    }) {
        let excerpt: String = text
            .chars()
            .rev()
            .take(4096)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        description.push_str(&format!(
            " Last assistant text (unverified, at most 4096 characters): {}",
            serde_json::to_string(&excerpt)?
        ));
    }
    let spent = &journal.state().budget;
    let note = format!(
        "\n\nPrevious Nano run {previous} was recovered from its durable journal ({} records; {} model turns, {} tool calls, {} tokens charged). {description} Its tool calls are never replayed. Prior source and graph evidence is unavailable; inspect current files before relying on it.",
        records.len(),
        spent.model_turns,
        spent.tool_calls,
        spent.tokens()
    );
    Ok(Some(Recovery {
        note,
        budget: inherited_budget(spent),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nano::{
        session::Budget,
        tools::{PlannedFile, ToolCall},
        workspace::{
            Limits,
            patch::{Edit, PatchRequest},
        },
    };
    use std::io::Write;
    use tempfile::TempDir;

    fn fixture() -> (TempDir, Workspace) {
        let dir = TempDir::new().unwrap();
        let workspace =
            Workspace::new(&dir.path().canonicalize().unwrap(), Limits::default()).unwrap();
        (dir, workspace)
    }

    #[test]
    fn durable_terminal_command_state_is_safe_but_missing_spools_are_not() {
        let root = TempDir::new().unwrap();
        let session = root.path().join("old-run");
        super::super::private::directory(&session, true).unwrap();
        let expected = BTreeSet::from(["old-run-p1".to_owned()]);
        assert!(unresolved_commands(&session, "old-run", &expected).unwrap());
        let commands = session.join("commands");
        super::super::private::directory(&commands, true).unwrap();
        let mut snapshot = super::super::commands::Snapshot {
            process_id: "old-run-p1".into(),
            backend: "trusted_local".into(),
            completion: super::super::commands::Completion::Exited {
                code: Some(0),
                success: true,
            },
            stdout: super::super::commands::OutputRef {
                handle: "old-run-p1-stdout".into(),
                bytes: 0,
            },
            stderr: super::super::commands::OutputRef {
                handle: "old-run-p1-stderr".into(),
                bytes: 0,
            },
            output_complete: true,
            mutation_scope: "unknown".into(),
        };
        let mut state =
            super::super::private::file(&commands.join("old-run-p1.json"), true).unwrap();
        state
            .write_all(&serde_json::to_vec(&snapshot).unwrap())
            .unwrap();
        drop(state);
        assert!(unresolved_commands(&session, "old-run", &expected).unwrap());
        for stream in ["stdout", "stderr"] {
            drop(
                super::super::private::file(&commands.join(format!("old-run-p1-{stream}")), true)
                    .unwrap(),
            );
        }
        assert!(!unresolved_commands(&session, "old-run", &expected).unwrap());
        snapshot.completion = super::super::commands::Completion::Cancelled;
        fs::write(
            commands.join("old-run-p1.json"),
            serde_json::to_vec(&snapshot).unwrap(),
        )
        .unwrap();
        assert!(unresolved_commands(&session, "old-run", &expected).unwrap());
        assert!(
            unresolved_commands(&session, "old-run", &BTreeSet::from(["missing".into()])).unwrap()
        );
    }

    fn intent(files: Vec<PlannedFile>) -> Vec<Record> {
        vec![Record {
            version: crate::nano::replay::JOURNAL_VERSION,
            session_id: "old-run".into(),
            sequence: 4,
            budget: Budget::default(),
            event: SessionEvent::ToolIntent {
                call_id: "call-1".into(),
                call: ToolCall {
                    provider_call_id: "provider-1".into(),
                    name: "apply_patch".into(),
                    arguments: "{}".into(),
                },
                effect_plan: Some(EffectPlan::Patch { files }),
                start_recorded: false,
            },
        }]
    }

    #[test]
    fn interrupted_create_is_classified_from_current_bytes_without_reexecution() {
        let (dir, workspace) = fixture();
        let plan = workspace
            .patch_effect_plan(PatchRequest {
                edits: vec![Edit::Create {
                    path: "new.txt".into(),
                    content: "new\n".into(),
                }],
            })
            .unwrap();
        let EffectPlan::Patch { files } = plan;
        let records = intent(files);
        assert!(matches!(
            pending(&records, &workspace).unwrap().outcome,
            ToolOutcome::Failed(ToolError::Interrupted)
        ));
        std::fs::write(dir.path().join("new.txt"), "new\n").unwrap();
        assert!(matches!(
            pending(&records, &workspace).unwrap().outcome,
            ToolOutcome::Success(_)
        ));
        std::fs::write(dir.path().join("new.txt"), "changed\n").unwrap();
        assert!(matches!(
            pending(&records, &workspace).unwrap().outcome,
            ToolOutcome::Unknown(ToolError::Interrupted)
        ));
    }

    #[test]
    fn partial_batch_and_unsafe_target_never_certify_a_patch() {
        let (dir, workspace) = fixture();
        let plan = workspace
            .patch_effect_plan(PatchRequest {
                edits: vec![
                    Edit::Create {
                        path: "a.txt".into(),
                        content: "a\n".into(),
                    },
                    Edit::Create {
                        path: "b.txt".into(),
                        content: "b\n".into(),
                    },
                ],
            })
            .unwrap();
        let EffectPlan::Patch { files } = plan;
        let records = intent(files);
        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        assert!(matches!(
            pending(&records, &workspace).unwrap().outcome,
            ToolOutcome::Unknown(_)
        ));
        let unsafe_records = intent(vec![PlannedFile {
            path: ".ferrus/secret".into(),
            before_digest: None,
            after_digest: None,
        }]);
        assert!(matches!(
            pending(&unsafe_records, &workspace).unwrap().outcome,
            ToolOutcome::Unknown(_)
        ));
    }

    #[test]
    fn external_effects_are_unknown_and_read_only_calls_are_not_replayed() {
        let (_dir, workspace) = fixture();
        let mut records = intent(vec![]);
        let SessionEvent::ToolIntent {
            call, effect_plan, ..
        } = &mut records[0].event
        else {
            unreachable!()
        };
        call.name = "exec".into();
        *effect_plan = None;
        assert!(matches!(
            pending(&records, &workspace).unwrap().outcome,
            ToolOutcome::Unknown(_)
        ));
        for name in ["repository_search", "read_process", "read_output"] {
            let SessionEvent::ToolIntent { call, .. } = &mut records[0].event else {
                unreachable!()
            };
            call.name = name.into();
            assert!(matches!(
                pending(&records, &workspace).unwrap().outcome,
                ToolOutcome::Failed(ToolError::Interrupted)
            ));
        }
        let SessionEvent::ToolIntent { call, .. } = &mut records[0].event else {
            unreachable!()
        };
        call.name = "stop_process".into();
        assert!(matches!(
            pending(&records, &workspace).unwrap().outcome,
            ToolOutcome::Unknown(ToolError::Interrupted)
        ));
        let SessionEvent::ToolIntent { call, .. } = &mut records[0].event else {
            unreachable!()
        };
        call.name = "check".into();
        assert!(matches!(
            pending(&records, &workspace).unwrap().outcome,
            ToolOutcome::Unknown(ToolError::Interrupted)
        ));
        let SessionEvent::ToolIntent { call, .. } = &mut records[0].event else {
            unreachable!()
        };
        call.name = "external__write".into();
        assert!(matches!(
            pending(&records, &workspace).unwrap().outcome,
            ToolOutcome::Unknown(ToolError::Interrupted)
        ));
    }

    #[test]
    fn inherited_usage_charges_unfinished_provider_reservations() {
        let spent = Budget {
            model_turns: 3,
            tool_calls: 2,
            retries: 1,
            elapsed_ms: 30_000,
            reported_input_tokens: 11,
            estimated_output_tokens: 7,
            reserved_input_tokens: 17,
            reserved_output_tokens: 23,
            ..Default::default()
        };
        let inherited = inherited_budget(&spent);
        assert_eq!(inherited.model_turns, 3);
        assert_eq!(inherited.tool_calls, 2);
        assert_eq!(inherited.retries, 1);
        assert_eq!(inherited.elapsed_ms, 0);
        assert_eq!(inherited.estimated_input_tokens, 17);
        assert_eq!(inherited.estimated_output_tokens, 30);
        assert_eq!(inherited.reserved_input_tokens, 0);
        assert_eq!(inherited.reserved_output_tokens, 0);
        assert_eq!(inherited.tokens(), spent.tokens());
    }
}
