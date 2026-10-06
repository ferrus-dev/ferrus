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
                && self.snapshot.sequence == 0
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
    fn oversized_steering_frames_preserve_the_editor_and_targets() {
        use crate::nano::wire::{self, CommandKind};

        // Include the envelope and newline when finding the exact text allowance.
        let mut empty = Vec::new();
        wire::write_command(
            &mut empty,
            CommandKind::Steer {
                text: String::new(),
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
        wire::write_command(&mut frame, CommandKind::Steer { text: input.text }).unwrap();
        assert_eq!(frame.len(), wire::FRAME_BYTES);
        assert!(app.input.is_empty());
        assert!(app.last_error.is_none());
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
