//! Editable prompt state, command completion, and interactive selection behavior.

use super::*;

pub(super) struct PendingNanoInput {
    pub(super) run_id: String,
    pub(super) text: String,
    pub(super) input_id: String,
}

impl App {
    pub(super) fn reject_nano_input(&mut self, run_id: &str, input_id: &str) -> bool {
        if !self
            .nano_pending
            .as_ref()
            .is_some_and(|pending| pending.run_id == run_id && pending.input_id == input_id)
        {
            return false;
        }
        self.release_nano_input(false);
        self.last_error =
            Some("Nano input was rejected; the request is still in the editor.".into());
        true
    }

    pub(super) fn poll_nano_input(&mut self) -> bool {
        let changed = self.nano.as_mut().is_some_and(NanoConversation::poll);
        let (Some(view), Some(pending)) = (&self.nano, &self.nano_pending) else {
            return changed;
        };
        if view.run_id != pending.run_id {
            return changed;
        }
        let accepted = view
            .snapshot
            .accepted_inputs
            .iter()
            .any(|id| *id == pending.input_id);
        let rejected = view.rejected_input.as_deref() == Some(pending.input_id.as_str());
        if accepted || rejected || view.snapshot.ended {
            self.release_nano_input(accepted);
            if !accepted {
                self.last_error = Some(if rejected {
                    "Nano rejected the input; the request is still in the editor."
                } else {
                    "Nano ended before input confirmation; the request is kept. Inspect the journal before retrying."
                }.into());
            }
            return true;
        }
        changed
    }

    pub(super) fn release_nano_input(&mut self, accepted: bool) {
        let Some(pending) = self.nano_pending.take() else {
            return;
        };
        if accepted {
            self.last_error = None;
            let line = pending.text.trim();
            if !line.contains('\n') && self.history.last().map(String::as_str) != Some(line) {
                self.history.push(line.into());
                if self.history.len() > MAX_HISTORY {
                    self.history.remove(0);
                }
            }
            if self.input == pending.text {
                self.input.clear();
                self.cursor_pos = 0;
                self.answering_nano_run_id = None;
                self.history_idx = None;
                self.history_saved.clear();
                self.clear_completion();
            }
        } else {
            // Preserve a newer draft too, including multiline edits made while waiting.
            if !self.input.is_empty() && self.input != pending.text {
                self.history.push(self.input.clone());
                if self.history.len() > MAX_HISTORY {
                    self.history.remove(0);
                }
            }
            self.input = pending.text;
            self.cursor_pos = self.input.chars().count();
            self.answering_nano_run_id = Some(pending.run_id);
            self.answering_question_task_id = None;
            self.history_idx = None;
            self.update_command_context();
        }
    }

    pub(super) fn append_transcript(&mut self, mut lines: Vec<TranscriptLine>) {
        trim_transcript_history(&mut lines);
        if let Some(view) = &mut self.nano {
            view.notices
                .extend(
                    take_transcript_block(&lines, 16)
                        .into_iter()
                        .map(|line| TranscriptLine {
                            text: crate::nano::conversation::display_text(&line.text, 2048),
                            kind: line.kind,
                            continuation: line.continuation,
                        }),
                );
            nano::trim_notices(&mut view.notices);
        }
        self.messages.extend(lines);
        trim_transcript_history(&mut self.messages);
    }

    pub(super) fn new() -> Self {
        Self {
            nano: None,
            nano_pending: None,
            status: StatusSnapshot::default(),
            debug: false,
            messages: Vec::new(),
            startup: None,
            runtime_tasks: Vec::new(),
            runtime_runs: Vec::new(),
            runtime_snapshot_at: None,
            question: None,
            question_task_id: None,
            answering_question_task_id: None,
            answering_nano_run_id: None,
            last_error: None,
            input: String::new(),
            cursor_pos: 0,
            history: load_history(),
            history_idx: None,
            history_saved: String::new(),
            completion_candidates: Vec::new(),
            completion_selected: 0,
            completion_active: false,
            completion_hidden: false,
            confirmation: None,
            selection: None,
            suspended: false,
            should_quit: false,
            ctrl_c_pending: false,
            ctrl_c_at: None,
            input_suppressed_until: None,
            redraw_on_resume: false,
        }
    }

    pub(super) fn clear_completion(&mut self) {
        self.completion_candidates.clear();
        self.completion_selected = 0;
        self.completion_active = false;
        self.completion_hidden = false;
    }

    pub(super) fn hide_completion_popup(&mut self) {
        self.completion_active = false;
        self.completion_hidden = true;
    }

    pub(super) fn insert_char(&mut self, ch: char) {
        if self.nano.is_some() && self.input.len() + ch.len_utf8() > crate::nano::wire::FRAME_BYTES
        {
            return;
        }
        if self.input.is_empty() && ch != '/' {
            self.answering_question_task_id = self.question_task_id.clone();
            self.answering_nano_run_id = self.nano.as_ref().map(|view| view.run_id.clone());
        }
        let idx = byte_index_for_char(&self.input, self.cursor_pos);
        self.input.insert(idx, ch);
        self.cursor_pos += 1;
        self.history_idx = None;
        self.update_command_context();
    }

    pub(super) fn insert_newline(&mut self) {
        if self.nano.is_some() && self.input.len() >= crate::nano::wire::FRAME_BYTES {
            return;
        }
        if self.input.is_empty() {
            self.answering_question_task_id = self.question_task_id.clone();
            self.answering_nano_run_id = self.nano.as_ref().map(|view| view.run_id.clone());
        }
        let idx = byte_index_for_char(&self.input, self.cursor_pos);
        self.input.insert(idx, '\n');
        self.cursor_pos += 1;
        self.history_idx = None;
        self.update_command_context();
    }

    pub(super) fn insert_text(&mut self, text: &str) {
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '\r' => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    self.insert_newline();
                }
                '\n' => self.insert_newline(),
                ch => self.insert_char(ch),
            }
        }
    }

    pub(super) fn delete_before_cursor(&mut self) {
        if self.cursor_pos == 0 {
            return;
        }
        let end = byte_index_for_char(&self.input, self.cursor_pos);
        let start = byte_index_for_char(&self.input, self.cursor_pos - 1);
        self.input.replace_range(start..end, "");
        self.cursor_pos -= 1;
        if self.input.is_empty() {
            self.answering_question_task_id = None;
            self.answering_nano_run_id = None;
        }
        self.history_idx = None;
        self.update_command_context();
    }

    pub(super) fn delete_after_cursor(&mut self) {
        if self.cursor_pos >= self.input.chars().count() {
            return;
        }
        let start = byte_index_for_char(&self.input, self.cursor_pos);
        let end = byte_index_for_char(&self.input, self.cursor_pos + 1);
        self.input.replace_range(start..end, "");
        if self.input.is_empty() {
            self.answering_question_task_id = None;
            self.answering_nano_run_id = None;
        }
        self.history_idx = None;
        self.update_command_context();
    }

    pub(super) fn move_left(&mut self) {
        self.cursor_pos = self.cursor_pos.saturating_sub(1);
    }

    pub(super) fn move_right(&mut self) {
        let len = self.input.chars().count();
        self.cursor_pos = (self.cursor_pos + 1).min(len);
    }

    pub(super) fn move_home(&mut self) {
        self.cursor_pos = 0;
    }

    pub(super) fn move_end(&mut self) {
        self.cursor_pos = self.input.chars().count();
    }

    pub(super) fn move_up_or_history(&mut self) {
        if self.completion_popup_visible() {
            self.previous_completion();
            return;
        }
        if self.move_cursor_up() {
            return;
        }
        self.history_up();
    }

    pub(super) fn move_down_or_history(&mut self) {
        if self.completion_popup_visible() {
            self.next_completion();
            return;
        }
        if self.move_cursor_down() {
            return;
        }
        self.history_down();
    }

    pub(super) fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.history_idx {
            None => {
                self.history_saved = self.input.clone();
                self.history_idx = Some(self.history.len() - 1);
            }
            Some(0) => {}
            Some(idx) => self.history_idx = Some(idx - 1),
        }
        if let Some(idx) = self.history_idx {
            self.input = self.history[idx].clone();
            self.cursor_pos = self.input.chars().count();
        }
        self.update_command_context();
    }

    pub(super) fn history_down(&mut self) {
        match self.history_idx {
            None => {}
            Some(idx) if idx + 1 < self.history.len() => {
                self.history_idx = Some(idx + 1);
                self.input = self.history[idx + 1].clone();
                self.cursor_pos = self.input.chars().count();
            }
            Some(_) => {
                self.history_idx = None;
                self.input = self.history_saved.clone();
                self.cursor_pos = self.input.chars().count();
            }
        }
        self.update_command_context();
    }

    pub(super) fn move_cursor_up(&mut self) -> bool {
        let chars: Vec<char> = self.input.chars().collect();
        let current_start = line_start(&chars, self.cursor_pos);
        if current_start == 0 {
            return false;
        }

        let current_col = self.cursor_pos - current_start;
        let previous_end = current_start - 1;
        let previous_start = line_start(&chars, previous_end);
        let previous_len = previous_end - previous_start;
        self.cursor_pos = previous_start + current_col.min(previous_len);
        true
    }

    pub(super) fn move_cursor_down(&mut self) -> bool {
        let chars: Vec<char> = self.input.chars().collect();
        let current_end = line_end(&chars, self.cursor_pos);
        if current_end == chars.len() {
            return false;
        }

        let current_start = line_start(&chars, self.cursor_pos);
        let current_col = self.cursor_pos - current_start;
        let next_start = current_end + 1;
        let next_end = line_end(&chars, next_start);
        let next_len = next_end - next_start;
        self.cursor_pos = next_start + current_col.min(next_len);
        true
    }

    pub(super) fn completion_prefix(&self) -> &str {
        self.input.trim()
    }

    pub(super) fn has_command_context(&self) -> bool {
        self.completion_prefix().starts_with('/') && !self.completion_candidates.is_empty()
    }

    pub(super) fn completion_popup_visible(&self) -> bool {
        self.confirmation.is_none()
            && self.selection.is_none()
            && self.has_command_context()
            && !self.completion_hidden
    }

    pub(super) fn compute_completions(&mut self) {
        let prefix = self.completion_prefix();
        self.completion_candidates = COMMANDS
            .iter()
            .copied()
            .filter(|(cmd, _)| cmd.starts_with(prefix))
            .take(MAX_COMPLETIONS)
            .collect();
        self.completion_selected = 0;
    }

    pub(super) fn refresh_completions(&mut self) {
        let prefix = self.completion_prefix();
        let needs_refresh = self.completion_candidates.is_empty()
            || self
                .completion_candidates
                .iter()
                .any(|(cmd, _)| !cmd.starts_with(prefix));
        if needs_refresh {
            self.compute_completions();
        }
    }

    pub(super) fn update_command_context(&mut self) {
        if self.completion_prefix().starts_with('/') {
            self.compute_completions();
            if self.completion_candidates.is_empty() {
                self.completion_active = false;
                self.completion_hidden = false;
            } else if self.completion_selected >= self.completion_candidates.len() {
                self.completion_selected = 0;
                self.completion_hidden = false;
            } else {
                self.completion_hidden = false;
            }
        } else {
            self.clear_completion();
        }
    }

    pub(super) fn accept_completion(&mut self) {
        if let Some((cmd, _)) = self.completion_candidates.get(self.completion_selected) {
            self.input = (*cmd).to_string();
            self.cursor_pos = self.input.chars().count();
        }
        self.clear_completion();
    }

    pub(super) fn accept_completion_and_submit(&mut self, cmd_tx: &mpsc::UnboundedSender<HqInput>) {
        self.accept_completion();
        self.submit_input(cmd_tx);
    }

    pub(super) fn next_completion(&mut self) {
        self.refresh_completions();
        if self.completion_candidates.is_empty() {
            self.completion_active = false;
            return;
        }
        self.completion_hidden = false;

        let prefix = self.completion_prefix().to_string();
        let shared_prefix = longest_common_prefix(&self.completion_candidates);
        if shared_prefix.len() > prefix.len() {
            self.input = shared_prefix.to_string();
            self.cursor_pos = self.input.chars().count();
            self.compute_completions();
            if self.completion_candidates.len() == 1 {
                self.accept_completion();
            } else {
                self.completion_active = true;
            }
            return;
        }

        if self.completion_candidates.len() == 1 {
            self.accept_completion();
            return;
        }
        if !self.completion_active {
            self.completion_active = true;
            self.completion_selected = 0;
            return;
        }
        self.completion_selected =
            (self.completion_selected + 1) % self.completion_candidates.len();
    }

    pub(super) fn previous_completion(&mut self) {
        self.refresh_completions();
        if !self.completion_candidates.is_empty() {
            self.completion_hidden = false;
            self.completion_active = true;
            self.completion_selected = if self.completion_selected == 0 {
                self.completion_candidates.len() - 1
            } else {
                self.completion_selected - 1
            };
        }
    }

    pub(super) fn submit_input(&mut self, cmd_tx: &mpsc::UnboundedSender<HqInput>) {
        let line = self.input.trim().to_string();
        if line.is_empty() {
            return;
        }
        let steering = !line.starts_with('/')
            && self.answering_question_task_id.is_none()
            && (self.nano.is_some() || self.answering_nano_run_id.is_some());
        if steering && self.nano_pending.is_some() {
            self.last_error = Some(
                "Nano input is awaiting acceptance; keep the next draft until it is confirmed."
                    .into(),
            );
            return;
        }
        let input_id = steering.then(|| crate::project::allocate_run_id("input", "nano"));
        if steering
            && crate::nano::wire::validate_identified_steer(&line, input_id.as_deref()).is_err()
        {
            self.last_error = Some(
                "Nano input exceeds the serialized frame limit; shorten it before sending.".into(),
            );
            return;
        }
        self.last_error = None;
        if line == "/quit" {
            self.should_quit = true;
        }
        let human_question_task_id = if line.starts_with('/') {
            None
        } else {
            self.answering_question_task_id.clone()
        };
        let nano_run_id = if line.starts_with('/') {
            self.nano.as_ref().map(|view| view.run_id.clone())
        } else {
            self.answering_nano_run_id
                .clone()
                .or_else(|| self.nano.as_ref().map(|view| view.run_id.clone()))
        };
        if cmd_tx
            .send(HqInput {
                text: line.clone(),
                human_question_task_id,
                nano_run_id: nano_run_id.clone(),
                nano_input_id: input_id.clone(),
            })
            .is_err()
        {
            self.last_error =
                Some("HQ input channel is closed; the request is still in the editor.".into());
            return;
        }
        if steering && let Some(run_id) = nano_run_id {
            self.nano_pending = Some(PendingNanoInput {
                run_id,
                text: self.input.clone(),
                input_id: input_id.unwrap(),
            });
            self.last_error = Some("Waiting for Nano to accept the input.".into());
            return;
        }
        if !line.contains('\n') && self.history.last() != Some(&line) {
            self.history.push(line);
            if self.history.len() > MAX_HISTORY {
                let extra = self.history.len() - MAX_HISTORY;
                self.history.drain(0..extra);
            }
        }
        self.input.clear();
        self.answering_question_task_id = None;
        self.answering_nano_run_id = None;
        self.cursor_pos = 0;
        self.history_idx = None;
        self.history_saved.clear();
        self.clear_completion();
    }
}

/// Keep recent complete blocks. An oversized newest block retains its opening
/// lines and, for a table, its bottom border, just like the visible viewport.
fn trim_transcript_history(lines: &mut Vec<TranscriptLine>) {
    if lines.len() <= MAX_TRANSCRIPT_LINES {
        return;
    }
    let cutoff = lines.len() - MAX_TRANSCRIPT_LINES;
    if let Some(start) = (cutoff..lines.len()).find(|&index| !lines[index].continuation) {
        lines.drain(..start);
    } else {
        let start = lines
            .iter()
            .rposition(|line| !line.continuation)
            .unwrap_or(0);
        lines.drain(..start);
        let bottom = if matches!(
            lines.first().map(|line| line.kind),
            Some(TranscriptKind::TableTop)
        ) && matches!(
            lines.last().map(|line| line.kind),
            Some(TranscriptKind::TableBottom)
        ) {
            lines.pop()
        } else {
            None
        };
        lines.truncate(MAX_TRANSCRIPT_LINES - usize::from(bottom.is_some()));
        lines.extend(bottom);
    }
    if lines.capacity() > MAX_TRANSCRIPT_LINES * 2 {
        lines.shrink_to(MAX_TRANSCRIPT_LINES);
    }
}
