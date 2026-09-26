//! Storage recovery, integrity, quotas, private permissions, and checkpoint boundaries.

use super::{
    journal::*,
    provider::{FinishReason, ModelResponse, Usage},
    session::*,
    tools::*,
};
use serde_json::json;
use std::{
    fs::{self, OpenOptions},
    io::Write,
};
use tempfile::TempDir;

fn create(quotas: Quotas) -> (TempDir, FileJournal) {
    let dir = TempDir::new().unwrap();
    let journal = FileJournal::create(&dir.path().canonicalize().unwrap(), "s-1", quotas).unwrap();
    (dir, journal)
}

fn started(journal: &mut FileJournal) -> Record {
    journal
        .append(
            SessionEvent::Started {
                provider: None,
                identity: SessionIdentity {
                    session_id: "s-1".into(),
                    project_id: "p-1".into(),
                    task_id: None,
                    run_id: None,
                },
                limits: Limits::default(),
                input: "task".into(),
                inherited_budget: None,
            },
            &Budget::default(),
        )
        .unwrap()
}

fn model_group(journal: &mut FileJournal) -> (Budget, Vec<ToolCall>) {
    let mut budget = Budget {
        model_turns: 1,
        reserved_input_tokens: 20,
        reserved_output_tokens: 30,
        ..Default::default()
    };

    journal
        .append(SessionEvent::ModelStarted { turn: 1 }, &budget)
        .unwrap();

    let calls = vec![
        ToolCall {
            provider_call_id: "a".into(),
            name: "edit".into(),
            arguments: "{}".into(),
        },
        ToolCall {
            provider_call_id: "b".into(),
            name: "edit".into(),
            arguments: "{}".into(),
        },
    ];

    budget.reserved_input_tokens = 0;
    budget.reserved_output_tokens = 0;
    budget.reported_input_tokens = 12;
    budget.reported_output_tokens = 8;

    journal
        .append(
            SessionEvent::ModelCompleted {
                response: ModelResponse {
                    finish: FinishReason::ToolCalls,
                    text: String::new(),
                    calls: calls.clone(),
                    continuation: None,
                },
                usage: Usage {
                    input_tokens: 12,
                    output_tokens: 8,
                    reported: true,
                },
            },
            &budget,
        )
        .unwrap();
    (budget, calls)
}

#[test]
fn interrupted_intent_can_be_reconciled_once_before_sealing() {
    let (_dir, mut journal) = create(Quotas::default());
    started(&mut journal);
    let (mut budget, calls) = model_group(&mut journal);
    budget.tool_calls = 1;
    journal
        .append(
            SessionEvent::ToolIntent {
                call_id: "call-1".into(),
                call: calls[0].clone(),
                effect_plan: None,
                start_recorded: false,
            },
            &budget,
        )
        .unwrap();
    let directory = journal.directory().to_path_buf();
    drop(journal);

    let (mut recovered, mut records) =
        FileJournal::recover_open(&directory, Quotas::default()).unwrap();
    assert_eq!(recovered.state().pending_effect.as_deref(), Some("call-1"));
    records.push(
        recovered
            .append(
                SessionEvent::ToolResult {
                    call_id: "call-1".into(),
                    outcome: ToolOutcome::Unknown(ToolError::Interrupted),
                },
                &budget,
            )
            .unwrap(),
    );
    recovered.seal_interrupted(&mut records).unwrap();
    assert_eq!(recovered.state().unknown_effects, vec!["call-1"]);
    drop(recovered);

    let (again, records_again) = FileJournal::recover(&directory, Quotas::default()).unwrap();
    assert_eq!(records_again.len(), records.len());
    assert_eq!(again.state().unknown_effects, vec!["call-1"]);
}

#[test]
fn interrupted_tail_is_removed_but_complete_corruption_is_not_rewritten() {
    let (_dir, mut journal) = create(Quotas::default());
    let record = started(&mut journal);
    let ended = journal
        .append(
            SessionEvent::Ended {
                reason: EndReason::Cancelled,
            },
            &Budget::default(),
        )
        .unwrap();
    let directory = journal.directory().to_path_buf();
    let path = directory.join("events.jsonl");
    let size = fs::metadata(&path).unwrap().len();
    drop(journal);
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{\"version\":1")
        .unwrap();
    let (journal, records) = FileJournal::recover(&directory, Quotas::default()).unwrap();
    assert_eq!(records, [record, ended]);
    assert_eq!(fs::metadata(&path).unwrap().len(), size);
    drop(journal);
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"corrupt\n")
        .unwrap();
    let size = fs::metadata(&path).unwrap().len();
    assert!(FileJournal::recover(&directory, Quotas::default()).is_err());
    assert_eq!(fs::metadata(&path).unwrap().len(), size);
}

#[test]
fn overlong_complete_records_are_rejected_without_rewriting_the_journal() {
    let quotas = Quotas {
        record_bytes: 2048,
        ..Default::default()
    };
    for extra_bytes in [0, 1, 2, 8192] {
        let (_dir, mut journal) = create(quotas.clone());
        let started = started(&mut journal);
        let directory = journal.directory().to_path_buf();
        let path = directory.join("events.jsonl");
        let end = Record {
            sequence: 2,
            event: SessionEvent::Ended {
                reason: EndReason::Cancelled,
            },
            ..started
        };
        drop(journal);

        let mut line = serde_json::to_vec(&end).unwrap();
        line.resize(quotas.record_bytes + extra_bytes, b' ');
        line.push(b'\n');
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&line)
            .unwrap();
        let original = fs::read(&path).unwrap();
        let result = FileJournal::recover(&directory, quotas.clone());
        if extra_bytes == 0 {
            assert_eq!(result.unwrap().0.state().end, Some(EndReason::Cancelled));
        } else {
            assert_eq!(
                result.err().expect("Overlong record accepted").to_string(),
                "Journal record exceeds quota"
            );
        }
        assert_eq!(fs::read(&path).unwrap(), original);
    }
}

#[test]
fn duplicate_writer_and_duplicate_session_creation_are_rejected() {
    let (dir, journal) = create(Quotas::default());
    assert!(FileJournal::recover(journal.directory(), Quotas::default()).is_err());
    assert!(
        FileJournal::create(
            &dir.path().canonicalize().unwrap(),
            "s-1",
            Quotas::default()
        )
        .is_err()
    );
    assert!(
        FileJournal::create(
            &dir.path().canonicalize().unwrap(),
            "../escape",
            Quotas::default()
        )
        .is_err()
    );
    let directory = journal.directory().to_path_buf();
    drop(journal);
    assert!(FileJournal::recover(&directory, Quotas::default()).is_ok());
}

#[test]
fn checkpoints_require_whole_model_tool_groups_and_verified_prefixes() {
    let (_dir, mut journal) = create(Quotas::default());
    started(&mut journal);
    let (mut budget, calls) = model_group(&mut journal);
    assert!(journal.checkpoint().is_err());
    for (index, call) in calls.into_iter().enumerate() {
        budget.tool_calls += 1;
        let call_id = format!("call-{}", budget.tool_calls);
        journal
            .append(
                SessionEvent::ToolIntent {
                    call_id: call_id.clone(),
                    call,
                    effect_plan: None,
                    start_recorded: false,
                },
                &budget,
            )
            .unwrap();
        assert!(journal.checkpoint().is_err());
        journal
            .append(
                SessionEvent::ToolResult {
                    call_id,
                    outcome: ToolOutcome::Success(json!("ok")),
                },
                &budget,
            )
            .unwrap();
        if index == 0 {
            assert!(journal.checkpoint().is_err());
        }
    }
    journal.checkpoint().unwrap();
    let directory = journal.directory().to_path_buf();
    let checkpoint_path = directory
        .join("checkpoints")
        .join(format!("{}.json", journal.state().sequence));
    let mut checkpoint: Checkpoint =
        serde_json::from_slice(&fs::read(&checkpoint_path).unwrap()).unwrap();
    assert!(
        fs::read_dir(directory.join("checkpoints"))
            .unwrap()
            .all(|entry| entry.unwrap().path().extension().unwrap() == "json")
    );
    drop(journal);
    let (_, records) = FileJournal::recover(&directory, Quotas::default()).unwrap();
    assert_eq!(
        verify_checkpoint(&checkpoint, &records).unwrap().budget,
        budget
    );
    checkpoint.budget.tool_calls += 1;
    assert!(verify_checkpoint(&checkpoint, &records).is_err());
}

#[test]
fn crash_during_model_request_preserves_reserved_usage_and_pending_tool_is_not_replayed() {
    let (_dir, mut journal) = create(Quotas::default());
    started(&mut journal);
    let budget = Budget {
        model_turns: 1,
        reserved_input_tokens: 12,
        reserved_output_tokens: 48,
        ..Default::default()
    };
    journal
        .append(SessionEvent::ModelStarted { turn: 1 }, &budget)
        .unwrap();
    let directory = journal.directory().to_path_buf();
    drop(journal);
    let (journal, _) = FileJournal::recover(&directory, Quotas::default()).unwrap();
    assert_eq!(journal.state().budget.tokens(), 60);
    assert!(!journal.state().checkpoint_ready());
}

#[test]
fn recovery_charges_unfinished_elapsed_budget_once_and_preserves_pending_work() {
    for phase in [
        "started",
        "provider",
        "overrun",
        "tool",
        "checkpoint",
        "ended",
    ] {
        let (_dir, mut journal) = create(Quotas::default());
        started(&mut journal);
        let mut budget = Budget::default();
        match phase {
            "provider" | "overrun" => {
                budget.model_turns = 1;
                budget.reserved_input_tokens = 12;
                budget.reserved_output_tokens = 48;
                budget.elapsed_ms = if phase == "overrun" {
                    Limits::default().elapsed_ms + 1
                } else {
                    10
                };
                journal
                    .append(SessionEvent::ModelStarted { turn: 1 }, &budget)
                    .unwrap();
            }
            "tool" => {
                let (model_budget, calls) = model_group(&mut journal);
                budget = model_budget;
                budget.tool_calls = 1;
                budget.elapsed_ms = 20;
                journal
                    .append(
                        SessionEvent::ToolIntent {
                            call_id: "call-1".into(),
                            call: calls[0].clone(),
                            effect_plan: None,
                            start_recorded: false,
                        },
                        &budget,
                    )
                    .unwrap();
            }
            "checkpoint" => journal.checkpoint().unwrap(),
            "ended" => {
                budget.elapsed_ms = 30;
                journal
                    .append(
                        SessionEvent::Ended {
                            reason: EndReason::Cancelled,
                        },
                        &budget,
                    )
                    .unwrap();
            }
            _ => (),
        }
        let directory = journal.directory().to_path_buf();
        let prefix = fs::read(directory.join("events.jsonl")).unwrap();
        drop(journal);

        let (mut journal, records) = FileJournal::recover(&directory, Quotas::default()).unwrap();
        let reason = if phase == "ended" {
            EndReason::Cancelled
        } else {
            budget.elapsed_ms = budget.elapsed_ms.max(Limits::default().elapsed_ms);
            EndReason::Limit(LimitKind::Elapsed)
        };
        assert_eq!(journal.state().budget, budget, "{phase}");
        assert_eq!(journal.state().end, Some(reason.clone()), "{phase}");
        assert_eq!(
            journal.state().pending_effect.as_deref(),
            (phase == "tool").then_some("call-1")
        );
        assert_eq!(
            super::replay::Replay::from_records(&records)
                .unwrap()
                .budget,
            budget
        );
        assert!(
            journal
                .append(SessionEvent::ModelStarted { turn: 2 }, &budget)
                .is_err()
        );
        let recovered = fs::read(directory.join("events.jsonl")).unwrap();
        assert!(recovered.starts_with(&prefix));
        drop(journal);
        let (journal, repeated) = FileJournal::recover(&directory, Quotas::default()).unwrap();
        assert_eq!(repeated, records);
        assert_eq!(journal.state().end, Some(reason));
        assert_eq!(fs::read(directory.join("events.jsonl")).unwrap(), recovered);
    }
}

#[test]
fn recovery_fails_if_the_elapsed_charge_cannot_be_persisted() {
    let (_dir, mut journal) = create(Quotas::default());
    started(&mut journal);
    let directory = journal.directory().to_path_buf();
    let path = directory.join("events.jsonl");
    let prefix = fs::read(&path).unwrap();
    drop(journal);

    let quotas = Quotas {
        journal_bytes: prefix.len() as u64,
        ..Default::default()
    };
    assert!(FileJournal::recover(&directory, quotas).is_err());
    assert_eq!(fs::read(&path).unwrap(), prefix);

    // A torn recovery ending is retried from the same committed budget.
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{\"version\":1")
        .unwrap();
    let (journal, records) = FileJournal::recover(&directory, Quotas::default()).unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(
        journal.state().budget.elapsed_ms,
        Limits::default().elapsed_ms
    );
    assert_eq!(
        journal.state().end,
        Some(EndReason::Limit(LimitKind::Elapsed))
    );
}

#[test]
fn bounded_artifacts_are_immutable_and_session_quotas_include_checkpoints() {
    let quotas = Quotas {
        artifact_bytes: 4,
        files: 3,
        ..Default::default()
    };
    let (_dir, mut journal) = create(quotas);
    started(&mut journal);
    assert!(journal.artifact("too-big", b"12345").is_err());
    assert!(journal.artifact("../escape", b"1").is_err());
    let path = journal.artifact("output-1", b"1234").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"1234");
    assert!(journal.artifact("output-1", b"0000").is_err());
    assert_eq!(fs::read(&path).unwrap(), b"1234");
    assert!(journal.checkpoint().is_err());
    assert!(journal.artifact("output-2", b"1").is_err());
}

#[test]
fn recovery_enforces_per_file_quotas_for_outputs_and_checkpoints() {
    for checkpoint in [false, true] {
        for lower_quota in [false, true] {
            let mut quotas = Quotas {
                artifact_bytes: 1024,
                record_bytes: 2048,
                ..Default::default()
            };
            let (_dir, mut journal) = create(quotas.clone());
            started(&mut journal);
            let directory = journal.directory().to_path_buf();
            let (path, limit, error) = if checkpoint {
                journal.checkpoint().unwrap();
                (
                    directory.join("checkpoints/1.json"),
                    quotas.record_bytes,
                    "Checkpoint exceeds quota",
                )
            } else {
                (
                    journal.artifact("output-1", b"private").unwrap(),
                    quotas.artifact_bytes,
                    "Artifact exceeds quota",
                )
            };
            drop(journal);

            // Whitespace padding keeps checkpoint JSON valid and makes its size
            // larger than the journal record, isolating the checkpoint quota.
            let mut bytes = fs::read(&path).unwrap();
            bytes.resize(limit, b' ');
            fs::write(&path, &bytes).unwrap();
            drop(FileJournal::recover(&directory, quotas.clone()).unwrap());

            if lower_quota {
                if checkpoint {
                    quotas.record_bytes -= 1;
                } else {
                    quotas.artifact_bytes -= 1;
                }
            } else {
                bytes.push(b' ');
                fs::write(&path, &bytes).unwrap();
            }

            let result = FileJournal::recover(&directory, quotas);
            assert_eq!(
                result.err().expect("Oversized file accepted").to_string(),
                error
            );
            assert_eq!(fs::read(&path).unwrap(), bytes);
            // A quota rejection must release the writer lock for a later reopen.
            assert!(FileJournal::recover(&directory, Quotas::default()).is_ok());
        }
    }
}

#[test]
fn recovery_accepts_private_command_spools_under_their_separate_quota() {
    let quotas = Quotas {
        artifact_bytes: 4,
        ..Default::default()
    };
    let (_dir, mut journal) = create(quotas.clone());
    started(&mut journal);
    let directory = journal.directory().to_path_buf();
    let commands = directory.join("commands");
    super::private::directory(&commands, true).unwrap();
    let mut output = super::private::file(&commands.join("s-1-p1-stdout"), true).unwrap();
    output.write_all(b"command output").unwrap();
    output.sync_all().unwrap();
    drop(output);
    drop(journal);

    let (recovered, records) = FileJournal::recover(&directory, quotas).unwrap();
    assert_eq!(records.len(), 2);
    drop(recovered);
}

#[cfg(unix)]
#[test]
fn recovery_rejects_a_redirected_command_spool() {
    use std::os::unix::fs::symlink;

    let (root, mut journal) = create(Quotas::default());
    started(&mut journal);
    let directory = journal.directory().to_path_buf();
    drop(journal);
    symlink(root.path(), directory.join("commands")).unwrap();
    assert!(FileJournal::recover(&directory, Quotas::default()).is_err());
}

#[test]
fn record_and_total_byte_quotas_fail_before_writing() {
    for quotas in [
        Quotas {
            record_bytes: 10,
            ..Default::default()
        },
        Quotas {
            total_bytes: 10,
            ..Default::default()
        },
        Quotas {
            journal_bytes: 10,
            ..Default::default()
        },
    ] {
        let (_dir, mut journal) = create(quotas);
        assert!(
            journal
                .append(
                    SessionEvent::Started {
                        provider: None,
                        identity: SessionIdentity {
                            session_id: "s-1".into(),
                            project_id: "p".into(),
                            task_id: None,
                            run_id: None
                        },
                        limits: Limits::default(),
                        input: "task".into(),
                        inherited_budget: None,
                    },
                    &Budget::default()
                )
                .is_err()
        );
        assert_eq!(
            fs::metadata(journal.directory().join("events.jsonl"))
                .unwrap()
                .len(),
            0
        );
    }
    assert!(encode(&json!({"data":"x".repeat(10000)}), 10).is_err());
}

#[test]
fn versions_sequences_and_mixed_session_ids_are_rejected() {
    for corruption in ["version", "sequence", "session_id"] {
        let (_dir, mut journal) = create(Quotas::default());
        let mut record = started(&mut journal);
        let directory = journal.directory().to_path_buf();
        drop(journal);
        match corruption {
            "version" => record.version += 1,
            "sequence" => record.sequence += 1,
            _ => record.session_id = "other".into(),
        }
        let mut bytes = serde_json::to_vec(&record).unwrap();
        bytes.push(b'\n');
        fs::write(directory.join("events.jsonl"), &bytes).unwrap();
        assert!(
            FileJournal::recover(&directory, Quotas::default()).is_err(),
            "{corruption}"
        );
    }
}

#[test]
fn private_permissions_are_enforced_for_new_storage_and_recovery() {
    let (_dir, mut journal) = create(Quotas::default());
    started(&mut journal);
    let output = journal.artifact("output-1", b"private").unwrap();
    for path in [
        journal.directory().join("events.jsonl"),
        journal.directory().join("writer.lock"),
        output,
    ] {
        super::private::check(&path, false).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
    super::private::check(journal.directory(), true).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let directory = journal.directory().to_path_buf();
        drop(journal);
        fs::set_permissions(
            directory.join("events.jsonl"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(FileJournal::recover(&directory, Quotas::default()).is_err());
    }
}

#[cfg(unix)]
#[test]
fn symlinks_cannot_redirect_artifacts_or_recovery() {
    use std::os::unix::fs::symlink;
    let (dir, mut journal) = create(Quotas::default());
    let outside = dir.path().join("outside");
    fs::write(&outside, b"keep").unwrap();
    symlink(&outside, journal.directory().join("outputs/redirect")).unwrap();
    assert!(journal.artifact("redirect", b"replace").is_err());
    let directory = journal.directory().to_path_buf();
    drop(journal);
    assert!(FileJournal::recover(&directory, Quotas::default()).is_err());
    assert_eq!(fs::read(outside).unwrap(), b"keep");
}
