use super::super::journal::Quotas;
use super::*;

fn frontend() -> (tempfile::TempDir, DisplayJournal, Frontend) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let journal = FileJournal::create(&root, "ui-test", Quotas::default()).unwrap();
    let (journal, frontend) = session(journal, "ui-test", true, None, None);
    (temp, journal, frontend)
}

#[test]
fn input_is_retained_until_durable_acceptance_and_rejected_queues_keep_it() {
    let (_temp, mut journal, mut ui) = frontend();
    let mut commands = journal.take_commands().unwrap();
    ui.insert("Inspect the result\nthen explain");
    let original = ui.text.clone();
    ui.submit();
    ui.submit();
    let SessionCommand::Steer { input_id, text } = commands.try_recv().unwrap() else {
        panic!("steering expected")
    };
    assert_eq!(text, original);
    assert!(commands.try_recv().is_err());
    assert_eq!(ui.text, original);
    journal.snapshots.send_replace(Some(Snapshot {
        accepted_inputs: [input_id.unwrap()].into(),
        ..Default::default()
    }));
    ui.poll();
    assert!(ui.text.is_empty());
    assert!(ui.pending.is_none());
    ui.history(true);
    assert_eq!(ui.text, original);
    drop(commands);
    ui.submit();
    assert!(ui.pending.is_none());
    assert_eq!(ui.text, original);
    assert!(ui.error.as_deref().unwrap().contains("not queued"));
}

#[test]
fn a_receipt_arriving_between_ticks_is_applied_before_the_next_edit() {
    let (_temp, journal, mut ui) = frontend();
    ui.insert("Previous request");
    ui.submit();
    let id = ui.pending.as_ref().unwrap().0.clone();
    journal.snapshots.send_replace(Some(Snapshot {
        accepted_inputs: [id].into(),
        ..Default::default()
    }));
    ui.event(
        Event::Paste("New request".into()),
        &Cancellation::default(),
        80,
    );
    assert_eq!(ui.text, "New request");
    assert!(ui.pending.is_none());
}

#[test]
fn full_command_queue_and_oversized_serialized_input_preserve_the_editor() {
    let (_temp, journal, mut ui) = frontend();
    for _ in 0..8 {
        ui.sender.try_send(SessionCommand::Interact).unwrap();
    }

    ui.insert("Still needed\nrequest");
    let draft = ui.text.clone();
    ui.submit();
    assert!(ui.pending.is_none());
    assert_eq!(ui.text, draft);
    assert!(ui.error.as_deref().unwrap().contains("not queued"));
    drop(journal);
    ui.text = "\"".repeat(wire::FRAME_BYTES / 2);
    ui.submit();
    assert!(ui.error.as_deref().unwrap().contains("frame limit"));
    assert_eq!(ui.text.len(), wire::FRAME_BYTES / 2);
}

#[test]
fn shutdown_keeps_unconfirmed_input_and_newer_drafts_and_rejects_further_turns() {
    let (_temp, journal, mut ui) = frontend();
    ui.insert("First request");
    ui.submit();
    ui.insert(" with newer detail");
    journal.snapshots.send_replace(Some(Snapshot {
        ended: true,
        ..Default::default()
    }));
    ui.poll();
    assert_eq!(ui.text, "First request");
    assert_eq!(
        ui.history.last().unwrap(),
        "First request with newer detail"
    );
    ui.submit();
    assert!(ui.pending.is_none());
    assert!(ui.error.as_deref().unwrap().contains("--resume"));
}

#[test]
fn resize_scroll_streaming_tools_and_multiline_editor_stay_within_geometry() {
    let (_temp, journal, mut ui) = frontend();
    journal.snapshots.send_replace(Some(Snapshot {
        turn: 2,
        status: "Generating response (turn 2)".into(),
        entries: (0..128)
            .map(|i| format!("Tool result {i}: {}", "long output ".repeat(100)))
            .collect(),
        ..Default::default()
    }));
    for _ in 0..10_000 {
        journal.events.send_modify(|state| {
            state.observe(wire::Event::Text {
                turn: 2,
                text: "preview".repeat(300),
            })
        });
    }

    ui.poll();
    ui.insert(&"multiline input\n".repeat(80));
    for width in [1, 2, 8, 40, 80] {
        for height in [1, 3, 8, 24] {
            for scroll in [0, 10, usize::MAX] {
                ui.view.scroll.end();
                if scroll > 0 {
                    ui.view.scroll.up(scroll);
                }

                let (rows, (row, col)) = ui.rows(width, height);
                assert_eq!(rows.len(), height);
                assert!(rows.iter().all(|(text, _)| text.chars().count() <= width));
                assert!(usize::from(row) < height);
                assert!(usize::from(col) < width);
            }
        }
    }

    let stop = Cancellation::default();
    ui.event(
        Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )),
        &stop,
        80,
    );
    assert!(stop.is_cancelled());
    assert!(ui.quitting);
}

#[test]
fn failed_append_never_publishes_a_display_receipt() {
    let (_temp, mut journal, _ui) = frontend();
    assert!(
        journal
            .append(
                SessionEvent::UserInput {
                    text: "Not durable".into(),
                    input_id: Some("input-1".into())
                },
                &Budget::default()
            )
            .is_err()
    );
    assert!(journal.snapshots.borrow().is_none());
}

#[test]
fn long_streaming_response_keeps_its_beginning_and_the_readers_position() {
    let (_temp, mut journal, mut ui) = frontend();
    let text = (0..100)
        .map(|i| format!("Paragraph {i}\n"))
        .collect::<String>();
    journal.snapshots.send_replace(Some(Snapshot {
        turn: 1,
        status: "Generating response (turn 1)".into(),
        ..Default::default()
    }));
    journal.model_delta(1, &text);
    ui.poll();
    assert_eq!(ui.view.preview.as_ref().unwrap().1, text);
    ui.view.scroll.home();
    let before = ui.rows(80, 24).0;
    assert!(
        before
            .iter()
            .any(|(row, _)| row.contains("Nano: Paragraph 0"))
    );
    journal.events.send_modify(|state| {
        state.observe(wire::Event::Text {
            turn: 1,
            text: format!("{text}New paragraph\n"),
        })
    });
    ui.poll();
    let during = ui.rows(80, 24).0;
    assert_eq!(&before[3..10], &during[3..10]);
    journal.snapshots.send_replace(Some(Snapshot {
        turn: 1,
        status: "Waiting for your input".into(),
        entries: [format!("Nano: {text}New paragraph\n")].into(),
        ..Default::default()
    }));
    ui.poll();
    assert!(ui.view.preview.is_none());
    assert_eq!(&before[3..10], &ui.rows(80, 24).0[3..10]);
    ui.event(
        Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::End,
            KeyModifiers::CONTROL,
        )),
        &Cancellation::default(),
        80,
    );
    assert!(ui.view.scroll.following());
    assert!(
        ui.rows(80, 24)
            .0
            .iter()
            .any(|(row, _)| row.contains("New paragraph"))
    );
}
