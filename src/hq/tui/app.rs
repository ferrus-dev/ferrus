//! Editable prompt state, command completion, and interactive selection behavior.

use super::*;

impl App {
    pub(super) fn new() -> Self {
        Self {
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
        if self.input.is_empty() && ch != '/' {
            self.answering_question_task_id = self.question_task_id.clone();
        }
        let idx = byte_index_for_char(&self.input, self.cursor_pos);
        self.input.insert(idx, ch);
        self.cursor_pos += 1;
        self.history_idx = None;
        self.update_command_context();
    }

    pub(super) fn insert_newline(&mut self) {
        if self.input.is_empty() {
            self.answering_question_task_id = self.question_task_id.clone();
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
        self.last_error = None;
        if line == "/quit" {
            self.should_quit = true;
        }
        let human_question_task_id = if line.starts_with('/') {
            None
        } else {
            self.answering_question_task_id.clone()
        };
        let _ = cmd_tx.send(HqInput {
            text: line.clone(),
            human_question_task_id,
        });
        if !line.contains('\n') && self.history.last() != Some(&line) {
            self.history.push(line);
            if self.history.len() > MAX_HISTORY {
                let extra = self.history.len() - MAX_HISTORY;
                self.history.drain(0..extra);
            }
        }
        self.input.clear();
        self.answering_question_task_id = None;
        self.cursor_pos = 0;
        self.history_idx = None;
        self.history_saved.clear();
        self.clear_completion();
    }
}
