//! Tests for compaction.

use super::*;

#[tokio::test]
async fn uncommitted_projection_never_reaches_the_provider() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut engine = Engine::new(
        identity(),
        limits(),
        scripted(vec![response("done", vec![])]),
        EvidenceTools {
            workspace: super::super::workspace::Workspace::new(
                &root,
                super::super::workspace::Limits::default(),
            )
            .unwrap(),
            enabled: true,
            preparation_delay: Duration::ZERO,
        },
        FakeHost::default(),
        RejectPreparation(FileJournal::create(&root, "session-1", Quotas::default()).unwrap()),
    )
    .unwrap();
    assert_eq!(
        run(&mut engine, &Cancellation::default()).await.reason,
        EndReason::JournalFailed
    );
    assert!(engine.provider.requests.is_empty());
}

#[tokio::test]
async fn prepared_context_precedes_compaction_and_survives_final_projection_failure() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut bounds = limits();
    bounds.context_bytes = 4 * 1024;
    bounds.response_bytes = 1024;
    let mut engine = Engine::new(
        identity(),
        bounds,
        CompactionProvider {
            read_calls: true,
            ..Default::default()
        },
        PreparedReads,
        FakeHost::default(),
        RejectPostCompactionPreparation {
            inner: FileJournal::create(&root, "session-1", Quotas::default()).unwrap(),
            compacted: false,
        },
    )
    .unwrap();
    let end = run(&mut engine, &Cancellation::default()).await;
    assert_eq!(end.reason, EndReason::JournalFailed);
    assert!(!end.durable);
    let records = engine.host.records.clone();
    let started = records
        .iter()
        .position(|record| matches!(record.event, SessionEvent::CompactionStarted { .. }))
        .expect("the prepared session must compact");
    assert!(records[..started].iter().rev().any(|record| {
        matches!(&record.event, SessionEvent::ContextPrepared { preparation }
            if preparation.observations[0]["marker"] == "prepared-input")
    }));
    assert!(
        records
            .iter()
            .any(|record| { matches!(record.event, SessionEvent::CompactionCompleted { .. }) })
    );
    assert!(engine.provider.requests.iter().any(|request| {
        request.tools.is_empty()
            && matches!(request.messages.first(), Some(Message::User { text }) if text.contains("prepared-input"))
    }));
    drop(engine);
    let (_journal, recovered) =
        FileJournal::recover(&root.join("nano/sessions/session-1"), Quotas::default()).unwrap();
    let replay = Replay::from_records(&recovered).unwrap();
    assert!(replay.summary.is_some());
    assert_eq!(replay.end, Some(EndReason::Limit(LimitKind::Elapsed)));
}

#[tokio::test]
async fn unsupported_provider_projection_fails_without_attempting_compaction() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut engine = Engine::new(
        identity(),
        limits(),
        UnsupportedEstimate,
        FakeTools::default(),
        FakeHost::default(),
        FileJournal::create(&root, "session-1", Quotas::default()).unwrap(),
    )
    .unwrap();
    let end = run(&mut engine, &Cancellation::default()).await;
    assert_eq!(end.reason, EndReason::ProviderProtocol);
    assert_eq!(end.budget.model_turns, 0);
    assert!(
        !engine
            .host
            .records
            .iter()
            .any(|record| { matches!(record.event, SessionEvent::CompactionStarted { .. }) })
    );
}

#[tokio::test]
async fn remaining_token_budget_can_reduce_output_without_compaction() {
    let provider = scripted(vec![response("Done", vec![])]);
    let tools = FakeTools::default().descriptors();
    let input = provider
        .estimate_input_tokens(&ModelRequest {
            messages: vec![Message::User {
                text: "Implement a fixture task".into(),
            }],
            tools,
            max_output_tokens: 12,
        })
        .unwrap();
    let mut bounds = limits();
    bounds.tokens = input + 12;
    let (_dir, mut engine) = setup(provider, bounds);
    let end = run(&mut engine, &Cancellation::default()).await;
    assert_eq!(end.reason, EndReason::ModelFinished);
    assert_eq!(engine.provider.requests.len(), 1);
    assert_eq!(engine.provider.requests[0].max_output_tokens, 12);
    assert!(
        !engine
            .host
            .records
            .iter()
            .any(|record| { matches!(record.event, SessionEvent::CompactionStarted { .. }) })
    );
}

#[tokio::test]
async fn compaction_stream_preserves_provider_error_kinds() {
    for (kind, reason) in [
        (
            ProviderErrorKind::ResponseLimit,
            EndReason::Limit(LimitKind::ResponseBytes),
        ),
        (ProviderErrorKind::Protocol, EndReason::ProviderProtocol),
    ] {
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut bounds = limits();
        bounds.context_bytes = 4 * 1024;
        bounds.response_bytes = 1024;
        let mut engine = Engine::new(
            identity(),
            bounds,
            CompactionProvider {
                summary_stream_error: Some(kind.clone()),
                ..Default::default()
            },
            FakeTools::default(),
            FakeHost::default(),
            FileJournal::create(&root, "session-1", Quotas::default()).unwrap(),
        )
        .unwrap();
        let end = run(&mut engine, &Cancellation::default()).await;
        assert_eq!(end.reason, reason);
        assert!(engine.host.records.iter().any(|record| {
            matches!(&record.event, SessionEvent::ModelFailed { error: Some(actual), .. } if actual == &kind)
        }));
        let replay = Replay::from_records(&engine.host.records).unwrap();
        assert_eq!(replay.end, Some(reason));
        assert_eq!(replay.budget, end.budget);
    }
}

#[tokio::test]
async fn compaction_does_not_spend_the_last_normal_turn_reservation() {
    let baseline_dir = TempDir::new().unwrap();
    let baseline_root = baseline_dir.path().canonicalize().unwrap();
    let mut bounds = limits();
    bounds.context_bytes = 4 * 1024;
    bounds.response_bytes = 1024;
    let mut baseline = Engine::new(
        identity(),
        bounds.clone(),
        CompactionProvider {
            normal_text_bytes: 400,
            ..Default::default()
        },
        FakeTools::default(),
        FakeHost::default(),
        FileJournal::create(&baseline_root, "session-1", Quotas::default()).unwrap(),
    )
    .unwrap();
    assert_eq!(
        run(&mut baseline, &Cancellation::default()).await.reason,
        EndReason::ModelFinished
    );
    let records = &baseline.host.records;
    let started = records
        .iter()
        .position(|record| matches!(record.event, SessionEvent::CompactionStarted { .. }))
        .unwrap();
    let completed = records
        .iter()
        .position(|record| matches!(record.event, SessionEvent::CompactionCompleted { .. }))
        .unwrap();
    let cost = |record: &Record| match &record.event {
        SessionEvent::ContextComposed { composition } => {
            composition.input_tokens_estimated + composition.output_tokens_reserved
        }
        _ => unreachable!(),
    };
    let summary_cost = cost(
        records[..started]
            .iter()
            .rev()
            .find(|record| matches!(record.event, SessionEvent::ContextComposed { .. }))
            .unwrap(),
    );
    let normal_cost = cost(
        records[completed + 1..]
            .iter()
            .find(|record| matches!(record.event, SessionEvent::ContextComposed { .. }))
            .unwrap(),
    );
    let consumed = records[started].budget.tokens()
        - records[started].budget.reserved_input_tokens
        - records[started].budget.reserved_output_tokens;
    bounds.tokens = consumed + summary_cost.max(normal_cost) + 1;
    assert!(bounds.tokens < consumed + summary_cost + normal_cost);
    let normal_turns_before_summary = baseline
        .provider
        .requests
        .iter()
        .take_while(|request| !request.tools.is_empty())
        .count();

    let limited_dir = TempDir::new().unwrap();
    let limited_root = limited_dir.path().canonicalize().unwrap();
    let mut limited = Engine::new(
        identity(),
        bounds,
        CompactionProvider {
            normal_text_bytes: 400,
            ..Default::default()
        },
        FakeTools::default(),
        FakeHost::default(),
        FileJournal::create(&limited_root, "session-1", Quotas::default()).unwrap(),
    )
    .unwrap();
    let end = run(&mut limited, &Cancellation::default()).await;
    assert_eq!(end.reason, EndReason::Limit(LimitKind::Tokens));
    assert_eq!(limited.provider.normal_turns, normal_turns_before_summary);
    assert_eq!(limited.provider.summaries, 0);
    assert!(
        !limited
            .host
            .records
            .iter()
            .any(|record| { matches!(record.event, SessionEvent::CompactionStarted { .. }) })
    );
}

#[tokio::test]
async fn interactive_capacity_keeps_compaction_available_for_later_user_turns() {
    let mut limits = limits();
    limits.context_bytes = 4 * 1024;
    limits.response_bytes = 1024;
    let (_dir, base) = setup(scripted(vec![]), limits.clone());
    let (sender, commands) = tokio::sync::mpsc::channel(8);
    let journal = InteractiveJournal {
        inner: base.journal,
        commands: Some(commands),
        sender,
        interactive: true,
        waits: 0,
        user_requests: 40,
        fail_input: false,
        inject_steering: false,
    };
    let mut identity = identity();
    identity.task_id = None;
    let mut engine = Engine::new(
        identity,
        limits,
        CompactionProvider {
            normal_turns: 10,
            ..Default::default()
        },
        base.tools,
        base.host,
        journal,
    )
    .unwrap();
    let end = engine
        .run(
            SessionCommand::Start {
                input: "Workspace constraints".into(),
            },
            &Cancellation::default(),
        )
        .await
        .unwrap();
    assert_eq!(end.reason, EndReason::Cancelled);
    assert!(end.durable);
    assert!(engine.provider.summaries > 0);
    let records = &engine.host.records;
    assert_eq!(
        records
            .iter()
            .filter(|r| matches!(r.event, SessionEvent::UserInput { .. }))
            .count(),
        40
    );
    assert_eq!(
        engine
            .provider
            .requests
            .iter()
            .filter(|r| !r.tools.is_empty())
            .count(),
        40
    );
    Replay::from_records(records).unwrap();
}
