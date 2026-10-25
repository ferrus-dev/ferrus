//! Tests for cancellation.

use super::*;

#[tokio::test]
async fn cancellation_before_authorization_never_starts_an_effect() {
    let cancellation = Cancellation::default();
    let (_dir, mut engine) = setup(scripted(vec![response("", vec![call("a", 1)])]), limits());
    engine.host.cancel_on_intent = Some(cancellation.clone());
    let end = run(&mut engine, &cancellation).await;
    assert_eq!(end.reason, EndReason::Cancelled);
    assert!(engine.tools.effects.lock().unwrap().is_empty());
    assert!(engine.journal.state().unknown_effects.is_empty());
}

#[tokio::test(start_paused = true)]
async fn cancellation_or_deadline_during_an_effect_records_unknown_outcome() {
    for cancel in [false, true] {
        let mut budget = limits();
        budget.elapsed_ms = if cancel { 30_000 } else { 1_000 };
        let (_dir, mut engine) = setup(scripted(vec![response("", vec![call("a", 1)])]), budget);
        engine.tools.stall = true;
        let cancellation = Cancellation::default();
        let control = cancellation.clone();
        let effects = engine.tools.effects.clone();
        let interrupter = async move {
            if cancel {
                loop {
                    if !effects.lock().unwrap().is_empty() {
                        control.cancel();
                        break;
                    }

                    tokio::task::yield_now().await;
                }
            }
        };
        let (end, ()) = tokio::join!(run(&mut engine, &cancellation), interrupter);
        assert_eq!(
            end.reason,
            if cancel {
                EndReason::Cancelled
            } else {
                EndReason::Limit(LimitKind::Elapsed)
            }
        );
        assert_eq!(engine.journal.state().unknown_effects, ["call-1"]);
        assert!(engine.journal.state().pending_effect.is_none());
    }
}

#[tokio::test]
async fn canceled_compaction_is_charged_and_never_replays_tools() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut bounds = limits();
    bounds.context_bytes = 4 * 1024;
    bounds.response_bytes = 1024;
    let provider = CompactionProvider {
        stall_summary: true,
        ..Default::default()
    };
    let started = provider.summary_started.clone();
    let cancellation = Cancellation::default();
    let control = cancellation.clone();
    let canceller = tokio::spawn(async move {
        while !started.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }

        control.cancel();
    });
    let mut engine = Engine::new(
        identity(),
        bounds,
        provider,
        FakeTools::default(),
        FakeHost::default(),
        FileJournal::create(&root, "session-1", Quotas::default()).unwrap(),
    )
    .unwrap();
    let end = tokio::time::timeout(Duration::from_secs(10), run(&mut engine, &cancellation))
        .await
        .expect("compaction must become cancellable");
    canceller.await.unwrap();
    assert_eq!(end.reason, EndReason::Cancelled);
    assert!(end.durable);
    assert!(
        engine
            .host
            .records
            .iter()
            .any(|record| matches!(record.event, SessionEvent::CompactionStarted { .. }))
    );
    assert!(
        engine
            .host
            .records
            .iter()
            .any(|record| matches!(record.event, SessionEvent::ModelFailed { .. }))
    );
    let effects = engine.tools.effects.lock().unwrap().len();
    drop(engine);
    let (_journal, recovered) =
        FileJournal::recover(&root.join("nano/sessions/session-1"), Quotas::default()).unwrap();
    let replay = Replay::from_records(&recovered).unwrap();
    assert_eq!(replay.end, Some(EndReason::Cancelled));
    assert_eq!(replay.budget, end.budget);
    assert!(effects > 0);
}

#[tokio::test]
async fn interactive_input_wait_is_cancellable_without_an_extra_inference() {
    let (_dir, base) = setup(
        scripted(vec![response("Ready for direction.", vec![])]),
        limits(),
    );
    let (sender, commands) = tokio::sync::mpsc::channel(8);
    let journal = InteractiveJournal {
        inner: base.journal,
        commands: Some(commands),
        sender,
        interactive: true,
        waits: 0,
        user_requests: 1,
        fail_input: false,
        inject_steering: false,
    };
    let cancel = Cancellation::default();
    let host = FakeHost {
        cancel_on_input: Some(cancel.clone()),
        ..Default::default()
    };
    let mut engine = Engine::new(
        identity(),
        limits(),
        base.provider,
        base.tools,
        host,
        journal,
    )
    .unwrap();
    let end = engine
        .run(
            SessionCommand::Start {
                input: "Implement".into(),
            },
            &cancel,
        )
        .await
        .unwrap();
    assert_eq!(end.reason, EndReason::Cancelled);
    assert!(end.durable);
    assert_eq!(engine.provider.requests.len(), 1);
    let replay = Replay::from_records(&engine.host.records).unwrap();
    assert_eq!(replay.end, Some(EndReason::Cancelled));
}
