//! Conversation state and rows shared by HQ and the standalone frontend.

use super::{scroll::Scroll, style};
use crate::nano::{
    conversation::{Snapshot, display_text},
    wire::{Event as NativeEvent, ObservedEvents},
};
use crossterm::style::Color;
use tokio::sync::watch;

pub(crate) struct Conversation {
    name: String,
    pub(crate) run_id: String,
    snapshots: watch::Receiver<Option<Snapshot>>,
    events: Option<watch::Receiver<ObservedEvents>>,
    pub(crate) snapshot: Snapshot,
    pub(crate) preview: Option<(u64, String)>,
    pub(crate) scroll: Scroll,
    pub(crate) notices: Vec<String>,
    seen_notice: u64,
    pub(crate) rejected_input: Option<String>,
}
impl Conversation {
    pub(crate) fn new(
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
            scroll: Scroll::default(),
            notices: Vec::new(),
            seen_notice: 0,
            rejected_input: None,
        }
    }
    pub(crate) fn poll(&mut self) -> bool {
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
                let preview = Some((*turn, display_text(text, crate::nano::wire::PREVIEW_BYTES)));
                changed |= preview != self.preview;
                self.preview = preview;
            }
            if let Some(NativeEvent::Ended { reason, durable }) = &event
                && !self.snapshot.ended
            {
                self.snapshot.status = format!("Process ended: {reason:?}, durable={durable}");
                self.snapshot.ended = true;
                self.snapshot.end_reason = Some(reason.clone());
                changed = true;
            }
            if state.notice_sequence > self.seen_notice {
                self.seen_notice = state.notice_sequence;
                if let Some(code) = state.notice {
                    self.notices
                        .push(format!("Nano diagnostic: {}", display_text(&code, 128)));
                    if self.notices.len() > 64 {
                        self.notices.remove(0);
                    }
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

pub(crate) fn lines(
    view: &Conversation,
    width: usize,
    height: usize,
    controls: &str,
) -> Vec<(String, Color)> {
    let plain = |text: String, color| (text.chars().take(width).collect(), color);
    let mut lines = vec![
        plain(format!("Ferrus / Nano - {}", view.name), style::ACCENT),
        plain(
            format!(
                "{}  |  Tokens: {}  Turns: {}  Tools: {}",
                display_text(&view.snapshot.status, 256),
                view.snapshot.budget.tokens(),
                view.snapshot.budget.model_turns,
                view.snapshot.budget.tool_calls,
            ),
            Color::DarkGrey,
        ),
        plain(style::separator(width), Color::DarkGrey),
    ];
    let mut body = Vec::new();
    if view.snapshot.omitted_entries > 0 {
        body.extend(
            wrap(
                &format!(
                    "[{} earlier entries are available in the session journal]",
                    view.snapshot.omitted_entries
                ),
                width,
            )
            .into_iter()
            .map(|text| (text, Color::DarkGrey)),
        );
    }
    for entry in &view.snapshot.entries {
        let color = if entry.starts_with("You:") {
            Color::White
        } else if entry.starts_with("Tool:") || entry.starts_with("Result:") {
            Color::DarkGrey
        } else {
            Color::Grey
        };
        body.extend(
            wrap(entry, width)
                .into_iter()
                .enumerate()
                .map(|(index, text)| {
                    let color = if index == 0
                        && (entry.starts_with("You:") || entry.starts_with("Nano:"))
                    {
                        style::ACCENT
                    } else {
                        color
                    };
                    (text, color)
                }),
        );
        body.push((String::new(), color));
    }
    if let Some((_, preview)) = &view.preview {
        body.extend(
            wrap(&format!("Nano: {preview}"), width)
                .into_iter()
                .enumerate()
                .map(|(index, text)| {
                    (
                        text,
                        if index == 0 {
                            style::ACCENT
                        } else {
                            Color::Grey
                        },
                    )
                }),
        );
    }
    let available = height.saturating_sub(lines.len() + 1);
    let visible = view.scroll.range(body.len(), available);
    lines.extend(
        body[visible.clone()]
            .iter()
            .cloned()
            .map(|(text, color)| plain(text, color)),
    );
    lines.resize(height.saturating_sub(1), (String::new(), Color::Grey));
    lines.push(plain(
        format!(
            "{}-{} / {}  {}  {controls}",
            visible.start.saturating_add(1).min(body.len()),
            visible.end,
            body.len(),
            if view.scroll.following() {
                "Live"
            } else {
                "History; Ctrl-End: latest"
            }
        ),
        Color::DarkGrey,
    ));
    lines.truncate(height);
    lines
}

pub(crate) fn wrap(text: &str, width: usize) -> Vec<String> {
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
