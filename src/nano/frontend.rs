//! Native terminal composition. Commands and durable records use the core protocol.

use super::{
    conversation::{Snapshot, display_text},
    journal::{FileJournal, Journal},
    session::*,
    tools::Cancellation,
    wire,
};
use crate::shared::{
    conversation::{self, Conversation},
    input,
};
use anyhow::{Result, ensure};
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind},
    queue,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal::{Clear, ClearType, size},
};
use futures_util::StreamExt;
use std::{
    io::{self, IsTerminal, Write},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tokio::sync::{mpsc, watch};

pub(crate) fn require_terminal() -> Result<()> {
    ensure!(
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        "--interactive requires terminal input and output; omit it for a headless request"
    );
    Ok(())
}

/// Display updates follow successful durable appends; a slow renderer cannot block persistence.
pub(crate) struct DisplayJournal {
    pub inner: FileJournal,
    snapshots: watch::Sender<Option<Snapshot>>,
    events: watch::Sender<wire::ObservedEvents>,
    snapshot: Snapshot,
    commands: Option<mpsc::Receiver<SessionCommand>>,
    streaming: wire::Preview,
    interactive: bool,
}
impl Journal for DisplayJournal {
    fn append(&mut self, event: SessionEvent, budget: &Budget) -> Result<Record> {
        let record = self.inner.append(event, budget)?;
        if self.interactive {
            self.snapshot.apply(&record);
            self.snapshots.send_replace(Some(self.snapshot.clone()));
        }
        Ok(record)
    }
    fn checkpoint(&mut self) -> Result<()> {
        self.inner.checkpoint()
    }
    fn take_commands(&mut self) -> Option<mpsc::Receiver<SessionCommand>> {
        self.commands.take()
    }
    fn interactive(&self) -> bool {
        self.interactive
    }
    fn model_delta(&mut self, turn: u64, text: &str) {
        if !self.interactive {
            return;
        }
        if let Some(event) = self.streaming.update(turn, text) {
            self.events.send_modify(|state| state.observe(event));
        }
    }
}

pub(crate) fn session(
    journal: FileJournal,
    session_id: &str,
    interactive: bool,
    initial: Option<String>,
    history: Option<Snapshot>,
) -> (DisplayJournal, Frontend) {
    let history = history.unwrap_or_default();
    let snapshot = Snapshot {
        entries: history.entries,
        omitted_entries: history.omitted_entries,
        ..Default::default()
    };
    let (snapshots, receiver) = watch::channel(None);
    let (events, previews) = watch::channel(wire::ObservedEvents::default());
    let (sender, commands) = mpsc::channel(8);
    let view = Conversation::new(
        session_id.into(),
        session_id.into(),
        receiver,
        Some(previews),
    );
    (
        DisplayJournal {
            inner: journal,
            snapshots,
            events,
            snapshot,
            commands: interactive.then_some(commands),
            streaming: wire::Preview::default(),
            interactive,
        },
        Frontend {
            view,
            sender,
            text: initial.unwrap_or_default(),
            cursor: 0,
            pending: None,
            history: Vec::new(),
            history_index: None,
            saved: String::new(),
            error: None,
            quitting: false,
            approvals: None,
            approval: None,
            approval_text: String::new(),
            approval_scroll: crate::shared::scroll::Scroll::default(),
            exit_on_end: false,
        },
    )
}

/// A one-shot host request bound to immutable tool arguments. Only the terminal
/// frontend owns its reply; model text and steering are never approval commands.
pub(crate) struct Approval {
    pub tool: String,
    pub content: String,
    pub reply: tokio::sync::oneshot::Sender<bool>,
}

pub(crate) struct Frontend {
    view: Conversation,
    sender: mpsc::Sender<SessionCommand>,
    text: String,
    cursor: usize,
    pending: Option<(String, String)>,
    history: Vec<String>,
    history_index: Option<usize>,
    saved: String,
    error: Option<String>,
    quitting: bool,
    approvals: Option<mpsc::Receiver<Approval>>,
    approval: Option<Approval>,
    approval_text: String,
    approval_scroll: crate::shared::scroll::Scroll,
    exit_on_end: bool,
}
impl Frontend {
    pub(crate) fn authoring(mut self, approvals: mpsc::Receiver<Approval>) -> Self {
        self.approvals = Some(approvals);
        self.exit_on_end = true;
        self
    }
    fn receive_approval(&mut self) -> bool {
        if self.approval.is_some() {
            return false;
        }
        if let Some(approval) = self.approvals.as_mut().and_then(|rx| rx.try_recv().ok()) {
            self.approval = Some(approval);
            self.approval_text.clear();
            self.approval_scroll.home();
            self.error = None;
            return true;
        }
        false
    }
    fn approval_event(&mut self, event: Event, stop: &Cancellation) {
        match event {
            Event::Paste(text) => {
                let remaining = 64usize.saturating_sub(self.approval_text.len());
                self.approval_text.extend(
                    text.chars()
                        .filter(|ch| ch.is_ascii_graphic() || *ch == ' ')
                        .take(remaining),
                );
            }
            Event::Key(key) if key.kind != KeyEventKind::Release => match (key.code, key.modifiers)
            {
                (KeyCode::Char('c'), KeyModifiers::CONTROL) => self.quit(stop),
                (KeyCode::PageUp, _) => self.approval_scroll.up(10),
                (KeyCode::PageDown, _) => self.approval_scroll.down(10),
                (KeyCode::Home, KeyModifiers::CONTROL) => self.approval_scroll.home(),
                (KeyCode::End, KeyModifiers::CONTROL) => self.approval_scroll.end(),
                (KeyCode::Backspace, _) => {
                    self.approval_text.pop();
                }
                (KeyCode::Esc, _) => {
                    if let Some(request) = self.approval.take() {
                        let _ = request.reply.send(false);
                    }
                }
                (KeyCode::Enter, _) => {
                    let answer = self.approval_text.trim().to_ascii_lowercase();
                    let approved = matches!(answer.as_str(), "" | "y" | "yes" | "approve");
                    if approved || matches!(answer.as_str(), "n" | "no" | "reject") {
                        if let Some(request) = self.approval.take() {
                            let _ = request.reply.send(approved);
                        }
                        self.approval_text.clear();
                        self.error = None;
                    } else {
                        self.error = Some("Type approve or reject, then press Enter.".into());
                    }
                }
                (KeyCode::Char(ch), KeyModifiers::NONE | KeyModifiers::SHIFT)
                    if (ch.is_ascii_graphic() || ch == ' ') && self.approval_text.len() < 64 =>
                {
                    self.approval_text.push(ch);
                }
                _ => (),
            },
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => self.approval_scroll.up(3),
                MouseEventKind::ScrollDown => self.approval_scroll.down(3),
                _ => (),
            },
            _ => (),
        }
    }
    fn approval_rows(&self, width: usize, height: usize) -> (Vec<(String, Color)>, (u16, u16)) {
        let request = self.approval.as_ref().unwrap();
        let title = match request.tool.as_str() {
            "archive_spec" => "Archive specification",
            "create_spec" => "Save specification",
            "enqueue_task" => "Queue task",
            _ => "Confirm operation",
        };
        let mut rows = vec![
            (
                format!("Ferrus / Nano - {title}"),
                crate::shared::style::ACCENT,
            ),
            (crate::shared::style::separator(width), Color::DarkGrey),
        ];
        let content = conversation::wrap(&display_text(&request.content, 32 * 1024), width);
        let available = height.saturating_sub(6);
        let visible = self.approval_scroll.range(content.len(), available);
        rows.extend(
            content
                .iter()
                .skip(visible.start)
                .take(visible.len())
                .cloned()
                .map(|line| (line, Color::Grey)),
        );
        rows.resize(height.saturating_sub(4), (String::new(), Color::Grey));
        rows.push((crate::shared::style::separator(width), Color::DarkGrey));
        rows.push((
            "Enter: approve  Esc: decline  PgUp/PgDn: read".into(),
            crate::shared::style::ACCENT,
        ));
        rows.push((
            self.error
                .clone()
                .unwrap_or_else(|| "Approve this content? (approve/reject)".into()),
            Color::Grey,
        ));
        rows.push((format!("> {}", self.approval_text), Color::White));
        rows.truncate(height);
        for (text, _) in &mut rows {
            *text = text.chars().take(width).collect();
        }
        (
            rows,
            (
                (height - 1) as u16,
                (2 + self.approval_text.len()).min(width - 1) as u16,
            ),
        )
    }

    fn submit(&mut self) {
        if self.pending.is_some() || self.text.trim().is_empty() {
            return;
        }
        if self.view.snapshot.ended {
            self.error = Some(format!(
                "Session {} ended. The request remains in the editor; use --resume to continue.",
                self.view.run_id
            ));
            return;
        }
        static INPUT: AtomicU64 = AtomicU64::new(1);
        let id = format!("input-{}", INPUT.fetch_add(1, Ordering::Relaxed));
        if let Err(error) = wire::validate_identified_steer(&self.text, Some(&id)) {
            self.error = Some(format!("Input exceeds the command frame limit: {error}"));
            return;
        }
        let command = SessionCommand::Steer {
            text: self.text.clone(),
            input_id: Some(id.clone()),
        };
        if let Err(error) = self.sender.try_send(command) {
            self.error = Some(format!(
                "Input was not queued: {error}. The request remains in the editor."
            ));
            return;
        }
        self.pending = Some((id, self.text.clone()));
        self.view.scroll.end();
        self.error = None;
    }
    fn poll(&mut self) -> bool {
        let changed = self.view.poll();
        if let Some((id, text)) = &self.pending {
            let accepted = self.view.snapshot.accepted_inputs.contains(id);
            if accepted || self.view.snapshot.ended {
                if accepted {
                    if self.history.last() != Some(text) {
                        self.history.push(text.clone());
                    }
                    if self.history.len() > 100 {
                        self.history.remove(0);
                    }
                    if self.text == *text {
                        self.text.clear();
                        self.cursor = 0;
                    }
                } else {
                    if self.text != *text {
                        if !self.text.is_empty() {
                            self.history.push(self.text.clone());
                        }
                        self.text = text.clone();
                        self.cursor = self.text.chars().count();
                    }
                    self.error = Some("Session ended before input confirmation. The request remains in the editor; inspect the journal before retrying.".into());
                }
                self.pending = None;
                return true;
            }
        }
        changed
    }
    fn insert(&mut self, text: &str) {
        for ch in text.replace("\r\n", "\n").replace('\r', "\n").chars() {
            if (ch.is_control() && ch != '\n')
                || self.text.len() + ch.len_utf8() > wire::FRAME_BYTES
            {
                continue;
            }
            input::insert(&mut self.text, &mut self.cursor, ch);
        }
        self.history_index = None;
    }
    fn history(&mut self, up: bool) {
        if self.history.is_empty() {
            return;
        }
        if up {
            self.history_index = Some(match self.history_index {
                None => {
                    self.saved = self.text.clone();
                    self.history.len() - 1
                }
                Some(index) => index.saturating_sub(1),
            });
        } else {
            self.history_index = self
                .history_index
                .and_then(|index| (index + 1 < self.history.len()).then_some(index + 1));
        }
        self.text = self
            .history_index
            .map_or_else(|| self.saved.clone(), |index| self.history[index].clone());
        self.cursor = self.text.chars().count();
    }
    fn event(&mut self, event: Event, stop: &Cancellation, _width: usize) {
        // A receipt can arrive while EventStream is asleep. Settle it before
        // editing, so the next draft cannot be appended to already accepted text.
        self.poll();
        let new_approval = self.receive_approval();
        if self.approval.is_some() {
            // An Enter already waiting in the input stream must not approve a
            // proposal that the frontend has not had a chance to display yet.
            if new_approval && matches!(&event, Event::Key(key) if key.code == KeyCode::Enter) {
                return;
            }
            self.approval_event(event, stop);
            return;
        }
        match event {
            Event::Paste(text) => self.insert(&text),
            Event::Key(key) if key.kind != KeyEventKind::Release => match (key.code, key.modifiers)
            {
                (KeyCode::Char('c'), KeyModifiers::CONTROL) => self.quit(stop),
                (KeyCode::Esc, _) if self.exit_on_end && self.view.snapshot.ended => {
                    self.quit(stop)
                }
                (KeyCode::PageUp, _) => self.view.scroll.up(10),
                (KeyCode::PageDown, _) => self.view.scroll.down(10),
                (KeyCode::Home, KeyModifiers::CONTROL) => self.view.scroll.home(),
                (KeyCode::End, KeyModifiers::CONTROL) => self.view.scroll.end(),
                (KeyCode::Enter, modifiers) if input::multiline_enter(modifiers) => {
                    self.insert("\n")
                }
                (KeyCode::Enter, KeyModifiers::NONE) => match self.text.trim() {
                    "/quit" | "/cancel" => self.quit(stop),
                    _ => self.submit(),
                },
                (KeyCode::Backspace, _) => input::delete(&mut self.text, &mut self.cursor, true),
                (KeyCode::Delete, _) => input::delete(&mut self.text, &mut self.cursor, false),
                (KeyCode::Left, _) => self.cursor = self.cursor.saturating_sub(1),
                (KeyCode::Right, _) => {
                    self.cursor = (self.cursor + 1).min(self.text.chars().count())
                }
                (KeyCode::Home, _) => self.cursor = 0,
                (KeyCode::End, _) => self.cursor = self.text.chars().count(),
                (KeyCode::Up, _) => self.history(true),
                (KeyCode::Down, _) => self.history(false),
                (KeyCode::Char(ch), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                    self.insert(&ch.to_string())
                }
                _ => (),
            },
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => self.view.scroll.up(3),
                MouseEventKind::ScrollDown => self.view.scroll.down(3),
                _ => (),
            },
            _ => (),
        }
    }
    fn quit(&mut self, stop: &Cancellation) {
        self.quitting = true;
        stop.cancel();
    }

    fn should_exit(&self, done: bool) -> bool {
        done && (self.quitting
            || self.exit_on_end
                && matches!(
                    self.view.snapshot.end_reason,
                    Some(EndReason::DefinitionCompleted | EndReason::Cancelled)
                ))
    }

    pub(crate) async fn run(
        mut self,
        stop: &Cancellation,
        mut done: watch::Receiver<bool>,
    ) -> Result<()> {
        // Cancellation is requested even if terminal setup, rendering or input fails.
        struct CancelOnDrop(Cancellation);
        impl Drop for CancelOnDrop {
            fn drop(&mut self) {
                self.0.cancel();
            }
        }
        let _cancel = CancelOnDrop(stop.clone());
        let _terminal = crate::shared::terminal::Guard::enter()?;
        self.cursor = self.text.chars().count();
        let mut initial = !self.text.is_empty();
        let mut events = EventStream::new();
        let mut input_open = true;
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        let mut dirty = true;
        loop {
            dirty |= self.poll();
            dirty |= self.receive_approval();
            if initial && self.view.snapshot.status == "Waiting for your input" {
                initial = false;
                self.submit();
                dirty = true;
            }
            if *done.borrow() && !self.view.snapshot.ended {
                self.view.snapshot.ended = true;
                self.view.snapshot.status =
                    "Session stopped; inspect the journal and diagnostics".into();
                dirty = true;
            }
            if self.should_exit(*done.borrow()) {
                _terminal.finish()?;
                return Ok(());
            }
            if dirty {
                let (width, height) = size()?;
                self.draw(&mut io::stdout(), width as usize, height as usize)?;
                dirty = false;
            }
            tokio::select! {
                event = events.next(), if input_open => {
                    match event {
                        Some(Ok(Event::Mouse(mouse))) if !matches!(mouse.kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown) => continue,
                        Some(Ok(event)) => self.event(event, stop, size()?.0 as usize),
                        Some(Err(error)) => return Err(error.into()),
                        None => { input_open = false; self.quit(stop); },
                    }
                    dirty = true;
                }
                _ = tick.tick() => (),
                _ = done.changed(), if !*done.borrow() => { dirty = true; },
            }
        }
    }
    fn draw(&self, out: &mut impl Write, width: usize, height: usize) -> Result<()> {
        let (rows, cursor) = self.rows(width.max(1), height.max(1));
        queue!(out, Hide)?;
        for (row, (text, color)) in rows.into_iter().enumerate() {
            queue!(
                out,
                MoveTo(0, row as u16),
                Clear(ClearType::CurrentLine),
                SetForegroundColor(color),
                Print(text)
            )?;
        }
        queue!(out, ResetColor, MoveTo(cursor.1, cursor.0), Show)?;
        out.flush()?;
        Ok(())
    }
    fn rows(&self, width: usize, height: usize) -> (Vec<(String, Color)>, (u16, u16)) {
        if self.approval.is_some() {
            return self.approval_rows(width, height);
        }
        let prompt = input::prompt(
            &display_text(&self.text, wire::FRAME_BYTES),
            self.cursor,
            width,
            "> ",
        );
        let prompt_height = prompt.lines.len().min((height / 3).max(1));
        let prompt_start = (prompt.cursor_row as usize).saturating_sub(prompt_height - 1);
        let top = height.saturating_sub(prompt_height + 3);
        let mut rows = conversation::lines(
            &self.view,
            width,
            top,
            if self.exit_on_end && self.view.snapshot.ended {
                "PgUp/PgDn or wheel: scroll; Esc: HQ"
            } else {
                "PgUp/PgDn or wheel: scroll; Ctrl-Home/End: first/latest"
            },
        );
        rows.resize(top, (String::new(), Color::Grey));
        rows.push((crate::shared::style::separator(width), Color::DarkGrey));
        for (index, text) in prompt
            .lines
            .iter()
            .skip(prompt_start)
            .take(prompt_height)
            .enumerate()
        {
            rows.push((
                format!("{}{text}", if index == 0 { "> " } else { "  " })
                    .chars()
                    .take(width)
                    .collect(),
                Color::White,
            ));
        }
        rows.push((crate::shared::style::separator(width), Color::DarkGrey));
        let notice = self
            .error
            .as_deref()
            .or_else(|| self.view.notices.last().map(String::as_str))
            .unwrap_or(if self.exit_on_end && self.view.snapshot.ended {
                "Session ended. Esc returns to HQ; diagnostics are saved on exit."
            } else if self.pending.is_some() {
                "Input queued; waiting for durable acceptance"
            } else {
                "Enter sends; Alt/Shift-Enter adds a line; Up/Down recalls input"
            });
        rows.push((
            display_text(notice, 1024)
                .chars()
                .filter(|ch| *ch != '\n')
                .take(width)
                .collect(),
            if self.error.is_some() {
                Color::Red
            } else {
                Color::DarkGrey
            },
        ));
        rows.resize(height, (String::new(), Color::Grey));
        rows.truncate(height);
        let cursor = (
            (top + 1 + prompt.cursor_row as usize - prompt_start).min(height - 1) as u16,
            (prompt.cursor_col as usize).min(width - 1) as u16,
        );
        (rows, cursor)
    }
}

#[cfg(test)]
mod tests {
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
}

#[cfg(test)]
mod approval_tests {
    use super::*;
    #[test]
    fn authoring_failures_remain_visible_until_the_user_closes_them() {
        let temp = tempfile::tempdir().unwrap();
        let journal = FileJournal::create(
            &temp.path().canonicalize().unwrap(),
            "failed-authoring",
            super::super::journal::Quotas::default(),
        )
        .unwrap();
        let (_, ui) = session(journal, "failed-authoring", true, None, None);
        let (_sender, requests) = mpsc::channel(1);
        let mut ui = ui.authoring(requests);
        for reason in [
            EndReason::Limit(LimitKind::NoProgress),
            EndReason::ProviderProtocol,
            EndReason::EffectUnknown,
        ] {
            ui.view.snapshot.end_reason = Some(reason);
            assert!(!ui.should_exit(true));
        }
        ui.view.snapshot.end_reason = Some(EndReason::DefinitionCompleted);
        assert!(!ui.should_exit(false));
        assert!(ui.should_exit(true));
        ui.view.snapshot.end_reason = None;
        assert!(!ui.should_exit(true));
        ui.view.snapshot.ended = true;
        ui.event(
            Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            )),
            &Cancellation::default(),
            80,
        );
        assert!(ui.should_exit(true));
    }
    #[tokio::test]
    async fn approval_accepts_plain_user_confirmation_and_never_uses_queued_steering() {
        let temp = tempfile::tempdir().unwrap();
        let journal = FileJournal::create(
            &temp.path().canonicalize().unwrap(),
            "approval-ui",
            super::super::journal::Quotas::default(),
        )
        .unwrap();
        let (_journal, ui) = session(journal, "approval-ui", true, None, None);
        let (tx, rx) = mpsc::channel(1);
        let mut ui = ui.authoring(rx);
        let (reply, mut result) = tokio::sync::oneshot::channel();
        tx.send(Approval {
            tool: "create_spec".into(),
            content: "Full final specification\nNo omitted content".into(),
            reply,
        })
        .await
        .unwrap();
        let stop = Cancellation::default();
        ui.sender
            .try_send(SessionCommand::Steer {
                text: "approve".into(),
                input_id: None,
            })
            .unwrap();
        ui.event(
            Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )),
            &stop,
            80,
        );
        assert!(ui.approval.is_some());
        assert!(result.try_recv().is_err());
        for text in ["not an answer", "approve deadbeef"] {
            ui.approval_text = text.into();
            ui.event(
                Event::Key(crossterm::event::KeyEvent::new(
                    KeyCode::Enter,
                    KeyModifiers::NONE,
                )),
                &stop,
                80,
            );
            assert!(result.try_recv().is_err());
            assert!(ui.approval.is_some());
            assert!(
                ui.rows(80, 24)
                    .0
                    .iter()
                    .any(|(row, _)| row.contains("Enter: approve"))
            );
        }
        for width in [1, 2, 8, 80] {
            for height in [1, 3, 24] {
                let (rows, (row, col)) = ui.rows(width, height);
                assert!(rows.len() <= height);
                assert!(rows.iter().all(|(text, _)| text.chars().count() <= width));
                assert!((row as usize) < height && (col as usize) < width);
            }
        }
        ui.approval_text.clear();
        ui.event(Event::Paste("approve\n".into()), &stop, 80);
        assert!(result.try_recv().is_err());
        ui.event(
            Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )),
            &stop,
            80,
        );
        assert!(result.await.unwrap());
        assert!(ui.approval.is_none());
    }
    #[tokio::test]
    async fn enter_approves_the_displayed_content_without_a_challenge_code() {
        let temp = tempfile::tempdir().unwrap();
        let journal = FileJournal::create(
            &temp.path().canonicalize().unwrap(),
            "enter-approval",
            super::super::journal::Quotas::default(),
        )
        .unwrap();
        let (_, mut ui) = session(journal, "enter-approval", true, None, None);
        let (reply, result) = tokio::sync::oneshot::channel();
        ui.approval = Some(Approval {
            tool: "archive_spec".into(),
            content: "## Outcome\n\nComplete".into(),
            reply,
        });
        let rows = ui.rows(80, 24).0;
        assert!(rows.iter().any(|(row, _)| row == "## Outcome"));
        assert!(!rows.iter().any(|(row, _)| row.contains("code")));
        ui.event(
            Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )),
            &Cancellation::default(),
            80,
        );
        assert!(result.await.unwrap());
    }
    #[tokio::test]
    async fn escape_declines_without_altering_the_normal_draft() {
        let temp = tempfile::tempdir().unwrap();
        let journal = FileJournal::create(
            &temp.path().canonicalize().unwrap(),
            "decline-ui",
            super::super::journal::Quotas::default(),
        )
        .unwrap();
        let (_, mut ui) = session(journal, "decline-ui", true, None, None);
        ui.insert("Unsent user draft");
        let (tx, rx) = mpsc::channel(1);
        ui = ui.authoring(rx);
        let (reply, result) = tokio::sync::oneshot::channel();
        tx.send(Approval {
            tool: "archive_spec".into(),
            content: "Approved Outcome draft".into(),
            reply,
        })
        .await
        .unwrap();
        ui.event(
            Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            )),
            &Cancellation::default(),
            80,
        );
        assert!(!result.await.unwrap());
        assert_eq!(ui.text, "Unsent user draft");
    }
}
