//! Native conversation rendering reuses HQ input geometry and terminal primitives.

use super::*;
use crate::nano::{
    conversation::{Snapshot, display_text},
    wire::{Event as NativeEvent, ObservedEvents},
};

pub(super) struct NanoConversation {
    name: String,
    pub(super) run_id: String,
    snapshots: watch::Receiver<Option<Snapshot>>,
    events: Option<watch::Receiver<ObservedEvents>>,
    pub(super) snapshot: Snapshot,
    preview: Option<(u64, String)>,
    pub(super) scroll: usize,
    pub(super) notices: Vec<TranscriptLine>,
    seen_notice: u64,
    pub(super) rejected_input: Option<String>,
}
impl NanoConversation {
    pub(super) fn new(
        name: String,
        run_id: String,
        snapshots: watch::Receiver<Option<Snapshot>>,
        events: Option<watch::Receiver<ObservedEvents>>,
    ) -> Self {
        let snapshot = snapshots.borrow().clone().unwrap_or_else(|| Snapshot {
            status: "Waiting for durable session records".into(),
            ..Default::default()
        });
        Self {
            name,
            run_id,
            snapshots,
            events,
            snapshot,
            preview: None,
            scroll: 0,
            notices: Vec::new(),
            seen_notice: 0,
            rejected_input: None,
        }
    }
    pub(super) fn scroll_up(&mut self, width: usize) {
        let maximum = self
            .snapshot
            .entries
            .iter()
            .map(|entry| wrap(entry, width).len() + 1)
            .sum::<usize>()
            + self
                .preview
                .as_ref()
                .map_or(0, |(_, text)| wrap(text, width).len());
        self.scroll = self
            .scroll
            .saturating_add(10)
            .min(maximum.saturating_sub(1));
    }
    pub(super) fn poll(&mut self) -> bool {
        let mut changed = false;
        if !matches!(self.snapshots.has_changed(), Ok(false))
            && let Some(snapshot) = self.snapshots.borrow_and_update().clone()
            && snapshot != self.snapshot
        {
            self.snapshot = snapshot;
            if !self.snapshot.status.starts_with("Generating") {
                self.preview = None;
            }
            changed = true;
        }
        if let Some(events) = &mut self.events
            && !matches!(events.has_changed(), Ok(false))
        {
            let state = events.borrow_and_update().clone();
            self.rejected_input = state.rejected_input.clone();
            let event = state.latest;
            if let Some(NativeEvent::Text { turn, text }) = &event
                && *turn == self.snapshot.turn
                && self.snapshot.status.starts_with("Generating")
            {
                let preview = Some((*turn, display_text(text, 512)));
                changed |= preview != self.preview;
                self.preview = preview;
            }
            if let Some(NativeEvent::Ended { reason, durable }) = &event
                && !self.snapshot.ended
            {
                self.snapshot.status = format!("Process ended: {reason:?}, durable={durable}");
                self.snapshot.ended = true;
                changed = true;
            }
            if state.notice_sequence > self.seen_notice {
                self.seen_notice = state.notice_sequence;
                if let Some(code) = state.notice {
                    self.notices.push(TranscriptLine {
                        text: format!("Nano diagnostic: {}", display_text(&code, 128)),
                        kind: TranscriptKind::Error,
                        continuation: false,
                    });
                    trim_notices(&mut self.notices);
                    changed = true;
                }
            }
            if events.has_changed().is_err() {
                self.events = None;
            }
        }
        changed
    }
}

pub(super) fn lines(view: &NanoConversation, width: usize, height: usize) -> Vec<DashboardLine> {
    let plain = |text: String, color| {
        DashboardLine::new(StyledLine::plain(truncate_to_width(&text, width), color))
    };
    let mut lines = vec![
        plain(format!("Nano conversation: {}", view.name), orange()),
        plain(display_text(&view.snapshot.status, 256), Color::Grey),
        plain(
            format!(
                "Tokens: {}  Turns: {}  Tools: {}  PgUp/PgDn scroll; /cancel; /detach",
                view.snapshot.budget.tokens(),
                view.snapshot.budget.model_turns,
                view.snapshot.budget.tool_calls
            ),
            Color::DarkGrey,
        ),
    ];
    let mut body = Vec::new();
    for entry in &view.snapshot.entries {
        body.extend(wrap(entry, width));
        body.push(String::new());
    }
    if let Some((_, preview)) = &view.preview {
        body.extend(wrap(&format!("Nano (streaming preview): {preview}"), width));
    }
    let available = height.saturating_sub(lines.len());
    let end = body
        .len()
        .saturating_sub(view.scroll.min(body.len().saturating_sub(available)));
    let start = end.saturating_sub(available);
    lines.extend(
        body[start..end]
            .iter()
            .cloned()
            .map(|text| plain(text, Color::Grey)),
    );
    lines.truncate(height);
    lines
}

pub(super) fn trim_notices(notices: &mut Vec<TranscriptLine>) {
    while notices.len() > 64 {
        let end = notices
            .iter()
            .skip(1)
            .position(|line| !line.continuation)
            .map_or(notices.len(), |index| index + 1);
        notices.drain(..end);
    }
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for line in text.lines() {
        let mut current = String::new();
        let mut used = 0;
        for ch in line.chars() {
            let next = 1; // Same character-cell convention as the existing HQ renderer.
            if used + next > width && !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                used = 0;
            }
            current.push(ch);
            used += next;
        }
        lines.push(current);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(run: &str) -> NanoConversation {
        let (_, snapshots) = watch::channel(Some(Snapshot {
            run_id: run.into(),
            task_id: "t-001".into(),
            status: "Generating response (turn 1)".into(),
            turn: 1,
            entries: (0..50)
                .map(|i| format!("Result {i}: {}", "long output ".repeat(20)))
                .collect(),
            ..Default::default()
        }));
        NanoConversation::new("executor:nano:t-001".into(), run.into(), snapshots, None)
    }

    #[test]
    fn conversation_resize_scroll_and_prompt_use_existing_terminal_geometry() {
        let mut app = App::new();
        app.nano = Some(view("run-1"));
        app.insert_text("Inspect the result\nthen explain it");
        for width in [1, 2, 8, 40, 80] {
            for height in [1, 3, 8, 24] {
                for scroll in [0, 10, usize::MAX] {
                    app.nano.as_mut().unwrap().scroll = scroll;
                    let rows = dashboard_lines(&app, width, height);
                    assert!(rows.len() <= height);
                    for row in rows {
                        let text: String = row
                            .line
                            .segments
                            .iter()
                            .map(|segment| segment.text.as_str())
                            .collect();
                        assert!(display_width(&text) <= width);
                    }
                }
            }
            let prompt = render_prompt(&app, width);
            assert!(usize::from(prompt.cursor_row) < prompt.lines.len());
            assert!(usize::from(prompt.cursor_col) <= width.max(3));
        }
        let native_rows = dashboard_lines(&app, 80, 20);
        app.nano = None;
        assert_ne!(native_rows.len(), 0);
        assert!(!dashboard_lines(&app, 80, 20).is_empty());
    }

    #[test]
    fn conversation_input_keeps_run_and_question_targets_and_bounds_multiline_paste() {
        let mut app = App::new();
        app.nano = Some(view("run-1"));
        app.question_task_id = Some("t-001".into());
        app.insert_text("Answer\nwith detail");
        app.nano = Some(view("run-2"));
        app.question_task_id = Some("t-002".into());
        let (sender, mut receiver) = mpsc::unbounded_channel();
        app.submit_input(&sender);
        let input = receiver.try_recv().unwrap();
        assert_eq!(input.human_question_task_id.as_deref(), Some("t-001"));
        assert_eq!(input.nano_run_id.as_deref(), Some("run-1"));
        app.insert_text(&"line\n".repeat(10_000));
        assert!(app.input.len() <= crate::nano::wire::FRAME_BYTES);
        assert_eq!(app.cursor_pos, app.input.chars().count());
    }

    #[test]
    fn recalled_answers_keep_the_question_target_from_drafting_start() {
        for (draft, initial_question, expected_question) in [
            ("", Some("t-001"), Some("t-001")),
            ("", None, None),
            ("Existing draft", Some("t-001"), Some("t-001")),
            ("Existing draft", None, None),
        ] {
            let mut app = App::new();
            app.nano = Some(view("run-1"));
            app.question_task_id = initial_question.map(str::to_string);
            app.history = vec!["Older answer".into(), "Recalled answer".into()];
            app.insert_text(draft);
            if !draft.is_empty() {
                app.question_task_id = Some("t-002".into());
                app.nano = Some(view("run-2"));
            }
            app.history_up();
            app.question_task_id = Some("t-003".into());
            app.nano = Some(view("run-3"));
            app.history_up();
            app.history_down();
            let (sender, mut receiver) = mpsc::unbounded_channel();
            app.submit_input(&sender);
            let input = receiver.try_recv().unwrap();
            assert_eq!(input.text, "Recalled answer");
            assert_eq!(input.human_question_task_id.as_deref(), expected_question);
            assert_eq!(input.nano_run_id.as_deref(), Some("run-1"));
            assert_eq!(input.nano_input_id.is_some(), expected_question.is_none());
            assert_eq!(app.nano_pending.is_some(), expected_question.is_none());
            assert_eq!(app.input.is_empty(), expected_question.is_some());
        }
    }

    #[test]
    fn returning_from_history_to_an_empty_editor_releases_answer_targets() {
        let mut app = App::new();
        app.nano = Some(view("run-1"));
        app.question_task_id = Some("t-001".into());
        app.history = vec!["Recalled answer".into()];
        app.history_up();
        assert_eq!(app.answering_question_task_id.as_deref(), Some("t-001"));
        app.history_down();
        assert!(app.input.is_empty());
        assert!(app.answering_question_task_id.is_none());
        assert!(app.answering_nano_run_id.is_none());
        app.question_task_id = Some("t-002".into());
        app.nano = Some(view("run-2"));
        app.insert_text("New answer");
        let (sender, mut receiver) = mpsc::unbounded_channel();
        app.submit_input(&sender);
        let input = receiver.try_recv().unwrap();
        assert_eq!(input.human_question_task_id.as_deref(), Some("t-002"));
        assert_eq!(input.nano_run_id.as_deref(), Some("run-2"));
        assert!(input.nano_input_id.is_none());
    }

    #[test]
    fn oversized_steering_frames_preserve_the_editor_and_targets() {
        use crate::nano::wire::{self, CommandKind};

        // Include the envelope and newline when finding the exact text allowance.
        let mut empty = Vec::new();
        wire::write_command(
            &mut empty,
            CommandKind::Steer {
                text: String::new(),
                input_id: Some(crate::project::allocate_run_id("input", "nano")),
            },
        )
        .unwrap();
        let allowance = wire::FRAME_BYTES - empty.len();
        for text in [
            "x".repeat(allowance + 1),
            "\"".repeat(allowance / 2 + 1),
            "\\".repeat(allowance / 2 + 1),
            format!("x{}x", "\n".repeat(allowance / 2)),
        ] {
            let mut app = App::new();
            app.nano = Some(view("run-1"));
            app.insert_text(&text);
            assert_eq!(app.input, text);
            let cursor = app.cursor_pos;
            let history = app.history.clone();
            let (sender, mut receiver) = mpsc::unbounded_channel();
            app.submit_input(&sender);
            assert!(receiver.try_recv().is_err());
            assert_eq!(app.input, text);
            assert_eq!(app.cursor_pos, cursor);
            assert_eq!(app.answering_nano_run_id.as_deref(), Some("run-1"));
            assert_eq!(app.history, history);
            assert!(app.last_error.as_deref().unwrap().contains("frame limit"));
        }

        let mut app = App::new();
        app.nano = Some(view("run-1"));
        app.insert_text(&"x".repeat(allowance));
        let (sender, mut receiver) = mpsc::unbounded_channel();
        app.submit_input(&sender);
        let input = receiver.try_recv().unwrap();
        let mut frame = Vec::new();
        wire::write_command(
            &mut frame,
            CommandKind::Steer {
                text: input.text,
                input_id: input.nano_input_id,
            },
        )
        .unwrap();
        assert_eq!(frame.len(), wire::FRAME_BYTES);
        assert!(!app.input.is_empty());
        assert!(app.nano_pending.is_some());
    }

    #[test]
    fn steering_retains_multiline_drafts_until_durable_acceptance_or_queue_rejection() {
        let (snapshots_tx, snapshots) = watch::channel(Some(Snapshot::default()));
        let (events_tx, events) = watch::channel(ObservedEvents::default());
        let mut app = App::new();
        app.nano = Some(NanoConversation::new(
            "nano".into(),
            "run-1".into(),
            snapshots,
            Some(events),
        ));
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let text = "Implement the fix\nand run the checks";
        app.insert_text(text);
        app.submit_input(&sender);
        let first = receiver.try_recv().unwrap();
        let first_id = first.nano_input_id.unwrap();
        assert_eq!(app.input, text);
        // Rapid Enter cannot enqueue the same request twice.
        app.submit_input(&sender);
        assert!(receiver.try_recv().is_err());
        assert!(app.nano_pending.is_some());
        // The host's queue failure is tied to the original run and input.
        assert!(!app.reject_nano_input("other-run", &first_id));
        assert!(app.reject_nano_input("run-1", &first_id));
        assert_eq!(app.input, text);
        assert!(app.nano_pending.is_none());

        app.submit_input(&sender);
        let second_id = receiver.try_recv().unwrap().nano_input_id.unwrap();
        assert_ne!(first_id, second_id);
        events_tx.send_modify(|state| {
            state.observe(NativeEvent::Error {
                code: format!("input_queue_full:{second_id}"),
            });
            // An unrelated diagnostic must not hide the child's rejection.
            state.observe(NativeEvent::Error {
                code: "another_notice".into(),
            });
            state.observe(NativeEvent::Text {
                turn: 1,
                text: "preview".into(),
            });
        });
        assert!(app.poll_nano_input());
        assert_eq!(app.input, text);
        assert!(app.nano_pending.is_none());

        app.submit_input(&sender);
        let third_id = receiver.try_recv().unwrap().nano_input_id.unwrap();
        snapshots_tx.send_replace(Some(Snapshot {
            accepted_inputs: [first_id, second_id].into(),
            ..Default::default()
        }));
        app.poll_nano_input();
        assert_eq!(app.input, text);
        assert!(app.nano_pending.is_some());
        snapshots_tx.send_replace(Some(Snapshot {
            accepted_inputs: [third_id].into(),
            ..Default::default()
        }));
        assert!(app.poll_nano_input());
        assert!(app.input.is_empty());
        assert!(app.nano_pending.is_none());
        assert!(app.last_error.is_none());
        // History recall does not go through insert_char, but still needs a receipt.
        app.history.push("A recalled request".into());
        app.history_up();
        app.submit_input(&sender);
        assert_eq!(app.input, "A recalled request");
        assert_eq!(
            receiver.try_recv().unwrap().nano_run_id.as_deref(),
            Some("run-1")
        );
        assert!(app.nano_pending.is_some());
    }

    #[test]
    fn steering_failure_preserves_newer_edits_and_closed_hq_channel_input() {
        let mut app = App::new();
        app.nano = Some(view("run-1"));
        let (sender, mut receiver) = mpsc::unbounded_channel();
        app.insert_text("Original\nrequest");
        app.submit_input(&sender);
        let id = receiver.try_recv().unwrap().nano_input_id.unwrap();
        app.insert_text("\nnew draft detail");
        let newer = app.input.clone();
        assert!(app.reject_nano_input("run-1", &id));
        assert_eq!(app.input, "Original\nrequest");
        assert_eq!(app.history.last(), Some(&newer));
        drop(receiver);
        app.submit_input(&sender);
        assert_eq!(app.input, "Original\nrequest");
        assert!(app.nano_pending.is_none());
        assert!(app.last_error.as_deref().unwrap().contains("closed"));
    }

    #[test]
    fn human_question_and_errors_remain_visible_over_a_full_conversation() {
        let mut app = App::new();
        app.nano = Some(view("run-1"));
        app.question = Some("Which API should I preserve?".into());
        app.last_error = Some("Input queue is full".into());
        let rows = dashboard_lines(&app, 80, 20);
        let text: String = rows
            .iter()
            .flat_map(|row| {
                row.line
                    .segments
                    .iter()
                    .map(|segment| segment.text.as_str())
            })
            .collect();
        assert!(text.contains("Which API"));
        assert!(text.contains("Input queue is full"));
    }

    #[test]
    fn hq_tables_remain_visible_and_notice_history_is_bounded() {
        let mut app = App::new();
        app.nano = Some(view("run-1"));
        app.append_transcript(split_transcript("A command reply", TranscriptKind::Info));
        app.append_transcript(vec![
            TranscriptLine {
                text: "+---------+".into(),
                kind: TranscriptKind::TableTop,
                continuation: false,
            },
            TranscriptLine {
                text: "| t-001   |".into(),
                kind: TranscriptKind::TableRow,
                continuation: true,
            },
            TranscriptLine {
                text: "+---------+".into(),
                kind: TranscriptKind::TableBottom,
                continuation: true,
            },
        ]);
        let rows = dashboard_lines(&app, 80, 30);
        let text: String = rows
            .iter()
            .flat_map(|row| {
                row.line
                    .segments
                    .iter()
                    .map(|segment| segment.text.as_str())
            })
            .collect();
        assert!(text.contains("| t-001"));
        assert!(text.contains("A command reply"));
        for _ in 0..1000 {
            app.append_transcript(split_transcript(&"x".repeat(4096), TranscriptKind::Info));
        }
        let notices = &app.nano.as_ref().unwrap().notices;
        assert!(notices.len() <= 64);
        assert!(notices.iter().all(|line| line.text.len() <= 2048));
    }

    #[test]
    fn preview_is_coalesced_and_never_replaces_durable_completed_text() {
        let (snapshots_tx, snapshots) = watch::channel(Some(Snapshot {
            turn: 1,
            status: "Generating response (turn 1)".into(),
            ..Default::default()
        }));
        let (events_tx, events) = watch::channel(ObservedEvents::default());
        let mut view = NanoConversation::new("nano".into(), "run".into(), snapshots, Some(events));
        events_tx.send_modify(|state| {
            state.observe(NativeEvent::Error {
                code: "input_queue_full".into(),
            })
        });
        for index in 0..10_000 {
            events_tx.send_modify(|state| {
                state.observe(NativeEvent::Text {
                    turn: 1,
                    text: format!("preview {index}"),
                })
            });
        }
        assert!(view.poll());
        assert_eq!(view.preview.as_ref().unwrap().1, "preview 9999");
        assert!(
            view.notices
                .iter()
                .any(|line| line.text.contains("input_queue_full"))
        );
        snapshots_tx.send_replace(Some(Snapshot {
            turn: 1,
            status: "Waiting for your input".into(),
            entries: ["Nano: committed answer".into()].into(),
            ..Default::default()
        }));
        assert!(view.poll());
        assert!(view.preview.is_none());
        assert_eq!(
            view.snapshot.entries.back().unwrap(),
            "Nano: committed answer"
        );
    }
}
