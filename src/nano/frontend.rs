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
    event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers},
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
    streaming: (u64, String),
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
        if self.streaming.0 != turn {
            self.streaming = (turn, String::new());
        }
        self.streaming.1.push_str(text);
        let mut cut = self.streaming.1.len().saturating_sub(512);
        while !self.streaming.1.is_char_boundary(cut) {
            cut += 1;
        }
        self.streaming.1.drain(..cut);
        self.events.send_modify(|state| {
            state.observe(wire::Event::Text {
                turn,
                text: self.streaming.1.clone(),
            })
        });
    }
}

pub(crate) fn session(
    journal: FileJournal,
    session_id: &str,
    interactive: bool,
    initial: Option<String>,
    history: Option<Snapshot>,
) -> (DisplayJournal, Frontend) {
    let snapshot = Snapshot {
        entries: history.map(|snapshot| snapshot.entries).unwrap_or_default(),
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
            streaming: (0, String::new()),
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
            approval_scroll: 0,
            exit_on_end: false,
        },
    )
}

/// A one-shot host request bound to immutable tool arguments. Only the terminal
/// frontend owns its reply; model text and steering are never approval commands.
pub(crate) struct Approval {
    pub id: String,
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
    approval_scroll: usize,
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
            self.approval_scroll = 0;
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
                (KeyCode::PageUp, _) => {
                    self.approval_scroll = self.approval_scroll.saturating_sub(10)
                }
                (KeyCode::PageDown, _) => {
                    self.approval_scroll = self.approval_scroll.saturating_add(10)
                }
                (KeyCode::Backspace, _) => {
                    self.approval_text.pop();
                }
                (KeyCode::Esc, _) => {
                    if let Some(request) = self.approval.take() {
                        let _ = request.reply.send(false);
                    }
                }
                (KeyCode::Enter, _) => {
                    let approved = self.approval.as_ref().is_some_and(|request| {
                        self.approval_text.trim() == format!("approve {}", request.id)
                    });
                    if approved || self.approval_text.trim() == "reject" {
                        if let Some(request) = self.approval.take() {
                            let _ = request.reply.send(approved);
                        }
                        self.approval_text.clear();
                        self.error = None;
                    } else {
                        self.error = Some("Type the displayed approval code or reject.".into());
                    }
                }
                (KeyCode::Char(ch), KeyModifiers::NONE | KeyModifiers::SHIFT)
                    if (ch.is_ascii_graphic() || ch == ' ') && self.approval_text.len() < 64 =>
                {
                    self.approval_text.push(ch);
                }
                _ => (),
            },
            _ => (),
        }
    }
    fn approval_rows(&self, width: usize, height: usize) -> (Vec<(String, Color)>, (u16, u16)) {
        let request = self.approval.as_ref().unwrap();
        let mut rows = vec![(format!("Human approval: {}", request.tool), Color::Yellow)];
        let content = conversation::wrap(&request.content, width);
        let available = height.saturating_sub(3);
        let start = self
            .approval_scroll
            .min(content.len().saturating_sub(available));
        rows.extend(
            content
                .iter()
                .skip(start)
                .take(available)
                .cloned()
                .map(|line| (line, Color::Grey)),
        );
        rows.resize(height.saturating_sub(2), (String::new(), Color::Grey));
        rows.push((
            self.error.clone().unwrap_or_else(|| {
                format!(
                    "Type approve {} or reject. PgUp/PgDn reviews; Esc declines.",
                    request.id
                )
            }),
            Color::Yellow,
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
    fn event(&mut self, event: Event, stop: &Cancellation, width: usize) {
        // A receipt can arrive while EventStream is asleep. Settle it before
        // editing, so the next draft cannot be appended to already accepted text.
        self.poll();
        self.receive_approval();
        if self.approval.is_some() {
            self.approval_event(event, stop);
            return;
        }
        match event {
            Event::Paste(text) => self.insert(&text),
            Event::Key(key) if key.kind != KeyEventKind::Release => match (key.code, key.modifiers)
            {
                (KeyCode::Char('c'), KeyModifiers::CONTROL) => self.quit(stop),
                (KeyCode::PageUp, _) => self.view.scroll_up(width),
                (KeyCode::PageDown, _) => self.view.scroll = self.view.scroll.saturating_sub(10),
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
            _ => (),
        }
    }
    fn quit(&mut self, stop: &Cancellation) {
        self.quitting = true;
        stop.cancel();
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
            if (self.quitting || self.exit_on_end) && *done.borrow() {
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
        let top = height.saturating_sub(prompt_height + 1);
        let mut rows = conversation::lines(
            &self.view,
            width,
            top,
            "PgUp/PgDn scroll; Ctrl-C or /quit exits",
        );
        rows.resize(top, (String::new(), Color::Grey));
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
        let notice = self
            .error
            .as_deref()
            .or_else(|| self.view.notices.last().map(String::as_str))
            .unwrap_or(if self.pending.is_some() {
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
            (top + prompt.cursor_row as usize - prompt_start).min(height - 1) as u16,
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
                    ui.view.scroll = scroll;
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
}

#[cfg(test)]
mod approval_tests {
    use super::*;
    #[tokio::test]
    async fn approval_requires_the_current_code_and_never_uses_queued_steering() {
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
            id: "0123abcd".into(),
            tool: "create_spec".into(),
            content: "Full final specification\nNo omitted content".into(),
            reply,
        })
        .await
        .unwrap();
        let stop = Cancellation::default();
        ui.sender
            .try_send(SessionCommand::Steer {
                text: "approve 0123abcd".into(),
                input_id: None,
            })
            .unwrap();
        assert!(ui.receive_approval());
        assert!(result.try_recv().is_err());
        for text in ["y", "approve deadbeef"] {
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
        ui.event(Event::Paste("approve 0123abcd\n".into()), &stop, 80);
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
            id: "abcd0123".into(),
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
