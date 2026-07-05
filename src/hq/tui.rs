use std::{
    env, fs,
    io::{self, Stdout, Write},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use anyhow::Result;
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEvent,
        KeyEventKind, KeyModifiers,
    },
    queue,
    style::{Attribute, Color, Print, PrintStyledContent, Stylize, style},
    terminal::{
        Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode, size,
    },
};
use futures_util::StreamExt;
use tokio::sync::{mpsc, oneshot, watch};

use crate::{
    platform,
    project::{RunRecord, TaskRecord, TaskStatus},
};

use super::state_watcher::{WatchedMilestone, WatchedState};
use crate::specs::MilestoneReadiness;

const MAX_HISTORY: usize = 100;
const MAX_COMPLETIONS: usize = 8;
const COMMANDS: &[(&str, &str)] = &[
    ("/plan", "spawn supervisor, plan a task"),
    ("/task", "queue one task and run the scheduler"),
    ("/milestones", "select current spec"),
    ("/reset-spec", "clear selected spec"),
    ("/spec", "draft and save an approved feature spec"),
    ("/check", "run the Ferrus check gate from HQ"),
    ("/supervisor", "open an interactive supervisor session"),
    ("/executor", "open an interactive executor session"),
    (
        "/resume",
        "resume the executor headlessly or recover consultation",
    ),
    ("/review", "spawn supervisor in review mode"),
    ("/status", "show task state and agents"),
    ("/tasks", "list SQLite task runtime rows"),
    ("/run", "plan a batch run from ready milestones"),
    ("/runs", "list SQLite run attempts"),
    ("/events", "list SQLite runtime events"),
    ("/attach", "show log path for a running headless session"),
    ("/stop", "stop all running sessions"),
    ("/reset", "reset state to Idle"),
    ("/init", "initialize ferrus in current directory"),
    ("/register", "register agent configs"),
    ("/model", "set or clear a role model override"),
    ("/help", "list all commands"),
    ("/quit", "exit HQ"),
];

pub enum UiMessage {
    Info(String),
    Success(String),
    Tip(String),
    Muted(String),
    Error(String),
    StatusUpdate(StatusSnapshot),
    Suspend {
        ack: oneshot::Sender<()>,
    },
    Resume,
    ConfirmationRequest {
        prompt: String,
        suffix: String,
        default: bool,
        accept_keys: Vec<char>,
        reject_keys: Vec<char>,
        reply: oneshot::Sender<bool>,
    },
    SelectionRequest {
        prompt: String,
        options: Vec<String>,
        reply: oneshot::Sender<Option<usize>>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HqInput {
    pub text: String,
    pub human_question_task_id: Option<String>,
}

#[derive(Clone, Default)]
pub struct StatusSnapshot {
    pub task_state: String,
    pub task_state_detail: String,
    #[allow(dead_code)]
    pub claimed_by: Option<String>,
    pub directory: String,
    pub branch: Option<String>,
    pub retries: u32,
    pub cycles: u32,
    pub supervisor_status: String,
    pub executor_status: String,
    pub selected_spec: Option<String>,
    pub selected_milestones: Vec<MilestoneSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MilestoneSnapshot {
    pub marker: String,
    pub title: String,
    pub completed: bool,
    pub readiness: MilestoneReadiness,
}

impl From<WatchedMilestone> for MilestoneSnapshot {
    fn from(milestone: WatchedMilestone) -> Self {
        Self {
            marker: milestone.marker,
            title: milestone.title,
            completed: milestone.completed,
            readiness: milestone.readiness,
        }
    }
}

impl StatusSnapshot {
    pub fn from_watched_state(watched: &WatchedState) -> StatusSnapshot {
        StatusSnapshot {
            task_state: String::new(),
            task_state_detail: String::new(),
            claimed_by: None,
            directory: String::new(),
            branch: None,
            retries: 0,
            cycles: 0,
            supervisor_status: "none".to_string(),
            executor_status: "none".to_string(),
            selected_spec: watched.selected_spec_display.clone(),
            selected_milestones: watched
                .selected_milestones
                .iter()
                .cloned()
                .map(MilestoneSnapshot::from)
                .collect(),
        }
    }
}

struct ConfirmationState {
    prompt: String,
    suffix: String,
    default: bool,
    accept_keys: Vec<char>,
    reject_keys: Vec<char>,
    reply: oneshot::Sender<bool>,
}

struct SelectionState {
    prompt: String,
    options: Vec<String>,
    selected: usize,
    reply: oneshot::Sender<Option<usize>>,
}

#[derive(Clone)]
struct TranscriptLine {
    text: String,
    kind: TranscriptKind,
    continuation: bool,
}

#[derive(Clone, Copy)]
enum TranscriptKind {
    Info,
    Success,
    Tip,
    Muted,
    Error,
}

pub struct App {
    status: StatusSnapshot,
    debug: bool,
    messages: Vec<TranscriptLine>,
    startup: Option<StartupHeader>,
    runtime_tasks: Vec<TaskRecord>,
    runtime_runs: Vec<RunRecord>,
    runtime_snapshot_at: Option<Instant>,
    question: Option<String>,
    question_task_id: Option<String>,
    answering_question_task_id: Option<String>,
    last_error: Option<String>,
    input: String,
    cursor_pos: usize,
    history: Vec<String>,
    history_idx: Option<usize>,
    history_saved: String,
    completion_candidates: Vec<(&'static str, &'static str)>,
    completion_selected: usize,
    completion_active: bool,
    completion_hidden: bool,
    confirmation: Option<ConfirmationState>,
    selection: Option<SelectionState>,
    suspended: bool,
    should_quit: bool,
    ctrl_c_pending: bool,
    ctrl_c_at: Option<Instant>,
    input_suppressed_until: Option<Instant>,
    redraw_on_resume: bool,
}

impl App {
    fn new() -> Self {
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

    fn clear_completion(&mut self) {
        self.completion_candidates.clear();
        self.completion_selected = 0;
        self.completion_active = false;
        self.completion_hidden = false;
    }

    fn hide_completion_popup(&mut self) {
        self.completion_active = false;
        self.completion_hidden = true;
    }

    fn insert_char(&mut self, ch: char) {
        if self.input.is_empty() && ch != '/' {
            self.answering_question_task_id = self.question_task_id.clone();
        }
        let idx = byte_index_for_char(&self.input, self.cursor_pos);
        self.input.insert(idx, ch);
        self.cursor_pos += 1;
        self.history_idx = None;
        self.update_command_context();
    }

    fn insert_newline(&mut self) {
        if self.input.is_empty() {
            self.answering_question_task_id = self.question_task_id.clone();
        }
        let idx = byte_index_for_char(&self.input, self.cursor_pos);
        self.input.insert(idx, '\n');
        self.cursor_pos += 1;
        self.history_idx = None;
        self.update_command_context();
    }

    fn insert_text(&mut self, text: &str) {
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

    fn delete_before_cursor(&mut self) {
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

    fn delete_after_cursor(&mut self) {
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

    fn move_left(&mut self) {
        self.cursor_pos = self.cursor_pos.saturating_sub(1);
    }

    fn move_right(&mut self) {
        let len = self.input.chars().count();
        self.cursor_pos = (self.cursor_pos + 1).min(len);
    }

    fn move_home(&mut self) {
        self.cursor_pos = 0;
    }

    fn move_end(&mut self) {
        self.cursor_pos = self.input.chars().count();
    }

    fn move_up_or_history(&mut self) {
        if self.completion_popup_visible() {
            self.previous_completion();
            return;
        }
        if self.move_cursor_up() {
            return;
        }
        self.history_up();
    }

    fn move_down_or_history(&mut self) {
        if self.completion_popup_visible() {
            self.next_completion();
            return;
        }
        if self.move_cursor_down() {
            return;
        }
        self.history_down();
    }

    fn history_up(&mut self) {
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

    fn history_down(&mut self) {
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

    fn move_cursor_up(&mut self) -> bool {
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

    fn move_cursor_down(&mut self) -> bool {
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

    fn completion_prefix(&self) -> &str {
        self.input.trim()
    }

    fn has_command_context(&self) -> bool {
        self.completion_prefix().starts_with('/') && !self.completion_candidates.is_empty()
    }

    fn completion_popup_visible(&self) -> bool {
        self.confirmation.is_none()
            && self.selection.is_none()
            && self.has_command_context()
            && !self.completion_hidden
    }

    fn compute_completions(&mut self) {
        let prefix = self.completion_prefix();
        self.completion_candidates = COMMANDS
            .iter()
            .copied()
            .filter(|(cmd, _)| cmd.starts_with(prefix))
            .take(MAX_COMPLETIONS)
            .collect();
        self.completion_selected = 0;
    }

    fn refresh_completions(&mut self) {
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

    fn update_command_context(&mut self) {
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

    fn accept_completion(&mut self) {
        if let Some((cmd, _)) = self.completion_candidates.get(self.completion_selected) {
            self.input = (*cmd).to_string();
            self.cursor_pos = self.input.chars().count();
        }
        self.clear_completion();
    }

    fn accept_completion_and_submit(&mut self, cmd_tx: &mpsc::UnboundedSender<HqInput>) {
        self.accept_completion();
        self.submit_input(cmd_tx);
    }

    fn next_completion(&mut self) {
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

    fn previous_completion(&mut self) {
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

    fn submit_input(&mut self, cmd_tx: &mpsc::UnboundedSender<HqInput>) {
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

struct StartupHeader {
    version: String,
    supervisor_type: String,
    supervisor_version: String,
    executor_type: String,
    executor_version: String,
}

struct TerminalUi {
    cursor_row: u16,
    cursor_col: u16,
    prompt_area_top: u16,
}

#[allow(clippy::too_many_arguments)]
pub async fn run_tui(
    mut msg_rx: mpsc::UnboundedReceiver<UiMessage>,
    cmd_tx: mpsc::UnboundedSender<HqInput>,
    mut state_rx: watch::Receiver<Option<WatchedState>>,
    debug: bool,
    supervisor_type: String,
    executor_type: String,
    supervisor_version: String,
    executor_version: String,
) -> Result<()> {
    let directory = current_dir_label();
    let branch = current_git_branch();
    let startup = StartupHeader {
        version: format!("v{}", env!("CARGO_PKG_VERSION")),
        supervisor_type,
        supervisor_version,
        executor_type,
        executor_version,
    };
    let mut app = App::new();
    app.debug = debug;
    app.startup = Some(startup);
    app.status.directory = directory.clone();
    app.status.branch = branch.clone();
    if let Some(watched) = state_rx.borrow().clone() {
        let mut status = StatusSnapshot::from_watched_state(&watched);
        status.directory = directory.clone();
        status.branch = branch.clone();
        app.status = status;
    }
    refresh_dashboard_snapshot(&mut app, true).await;

    let mut stdout = io::stdout();
    enter_tui()?;

    let mut ui = TerminalUi {
        cursor_row: 0,
        cursor_col: 0,
        prompt_area_top: 0,
    };
    redraw_dashboard(&mut stdout, &app, &mut ui)?;

    let mut event_stream = EventStream::new();
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
    loop {
        tokio::select! {
            maybe_event = event_stream.next(), if !app.suspended => {
                match maybe_event {
                    Some(Ok(event)) => handle_event(event, &mut app, &cmd_tx, &mut stdout, &mut ui)?,
                    Some(Err(err)) => {
                        let line = TranscriptLine {
                            text: format!("Event error: {err}"),
                            kind: TranscriptKind::Error,
                            continuation: false,
                        };
                        app.last_error = Some(line.text.clone());
                        app.messages.push(line);
                        redraw_dashboard(&mut stdout, &app, &mut ui)?;
                    }
                    None => app.should_quit = true,
                }
            }
            maybe_msg = msg_rx.recv() => {
                match maybe_msg {
                    Some(msg) => {
                        let refreshed_events =
                            handle_message(msg, &mut app, &mut stdout, &mut ui)?;
                        if refreshed_events {
                            event_stream = EventStream::new();
                        }
                    }
                    None => app.should_quit = true,
                }
            }
            _ = tick.tick() => {
                let refreshed_dashboard = refresh_dashboard_snapshot(&mut app, false).await;
                if app.ctrl_c_pending
                    && app
                        .ctrl_c_at
                        .is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(2))
                {
                    app.ctrl_c_pending = false;
                    app.ctrl_c_at = None;
                    if !app.suspended {
                        redraw_dashboard(&mut stdout, &app, &mut ui)?;
                    }
                } else if refreshed_dashboard && !app.suspended {
                    redraw_dashboard(&mut stdout, &app, &mut ui)?;
                }
            }
            changed = state_rx.changed() => {
                let watched = if changed.is_ok() {
                    state_rx.borrow_and_update().clone()
                } else {
                    None
                };
                if let Some(watched) = watched {
                    let supervisor_status = app.status.supervisor_status.clone();
                    let executor_status = app.status.executor_status.clone();
                    let directory = app.status.directory.clone();
                    let branch = app.status.branch.clone();
                    let previous_status = app.status.clone();
                    let mut next = StatusSnapshot::from_watched_state(&watched);
                    next.supervisor_status = supervisor_status;
                    next.executor_status = executor_status;
                    next.directory = directory;
                    next.branch = branch;
                    app.status = next;
                    let status_changed = status_dashboard_changed(&previous_status, &app.status);
                    let refreshed_dashboard =
                        refresh_dashboard_snapshot(&mut app, status_changed).await;
                    if (status_changed || refreshed_dashboard) && !app.suspended {
                        redraw_dashboard(&mut stdout, &app, &mut ui)?;
                    }
                }
            }
        }

        if app.should_quit {
            break;
        }
    }

    queue!(&mut stdout, Show)?;
    stdout.flush()?;
    save_history(&app.history);
    leave_tui()?;
    Ok(())
}

fn status_dashboard_changed(previous: &StatusSnapshot, next: &StatusSnapshot) -> bool {
    previous.task_state != next.task_state
        || previous.directory != next.directory
        || previous.branch != next.branch
        || previous.selected_spec != next.selected_spec
        || previous.selected_milestones != next.selected_milestones
        || previous.supervisor_status != next.supervisor_status
        || previous.executor_status != next.executor_status
}

async fn refresh_dashboard_snapshot(app: &mut App, force: bool) -> bool {
    if !force
        && app
            .runtime_snapshot_at
            .is_some_and(|last| last.elapsed() < Duration::from_secs(1))
    {
        return false;
    }

    let mut changed = force;
    if let Ok(tasks) = crate::project::list_tasks().await
        && app.runtime_tasks != tasks
    {
        app.runtime_tasks = tasks;
        changed = true;
    }
    if let Ok(runs) = crate::project::list_runs(8).await
        && app.runtime_runs != runs
    {
        app.runtime_runs = runs;
        changed = true;
    }

    let next_question = if let Ok(questions) = crate::project::list_human_questions().await
        && let Some(question) = questions.first()
    {
        let prefix = if questions.len() > 1 {
            format!("[{} queued] {}: ", questions.len(), question.task_id)
        } else {
            format!("{}: ", question.task_id)
        };
        let body = if question.question.is_empty() {
            "Type your answer and press Enter.".to_string()
        } else {
            question.question.clone()
        };
        Some((question.task_id.clone(), format!("{prefix}{body}")))
    } else {
        None
    };
    let (next_question_task_id, next_question) = next_question
        .map(|(task_id, question)| (Some(task_id), Some(question)))
        .unwrap_or((None, None));
    if app.question != next_question || app.question_task_id != next_question_task_id {
        app.question = next_question;
        app.question_task_id = next_question_task_id;
        changed = true;
    }

    app.runtime_snapshot_at = Some(Instant::now());
    changed
}

fn handle_event(
    event: Event,
    app: &mut App,
    cmd_tx: &mpsc::UnboundedSender<HqInput>,
    stdout: &mut Stdout,
    ui: &mut TerminalUi,
) -> Result<()> {
    if app.suspended {
        return Ok(());
    }

    match event {
        Event::Resize(_, _) => {
            redraw_dashboard(stdout, app, ui)?;
        }
        Event::Paste(text) => {
            app.insert_text(&text);
            redraw_prompt_area(stdout, app, ui)?;
        }
        Event::Key(key) => {
            if key.kind != KeyEventKind::Press {
                return Ok(());
            }

            if app
                .input_suppressed_until
                .is_some_and(|until| Instant::now() < until)
            {
                return Ok(());
            }
            app.input_suppressed_until = None;

            let mut full_redraw = false;
            if app.selection.is_some() {
                handle_selection_key(key, app);
            } else if app.confirmation.is_some() {
                handle_confirmation_key(key, app);
            } else {
                match (key.code, key.modifiers) {
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                        if app.ctrl_c_pending {
                            app.should_quit = true;
                        } else {
                            app.ctrl_c_pending = true;
                            app.ctrl_c_at = Some(std::time::Instant::now());
                        }
                    }
                    (KeyCode::Char('l'), KeyModifiers::CONTROL) => {
                        full_redraw = true;
                    }
                    (KeyCode::Char('a'), KeyModifiers::CONTROL) | (KeyCode::Home, _) => {
                        app.move_home()
                    }
                    (KeyCode::Char('e'), KeyModifiers::CONTROL) | (KeyCode::End, _) => {
                        app.move_end()
                    }
                    (KeyCode::Left, _) => app.move_left(),
                    (KeyCode::Right, _) => app.move_right(),
                    (KeyCode::Up, _) => app.move_up_or_history(),
                    (KeyCode::Down, _) => app.move_down_or_history(),
                    (KeyCode::Backspace, _) => app.delete_before_cursor(),
                    (KeyCode::Delete, _) => app.delete_after_cursor(),
                    (KeyCode::Esc, _) => {
                        if app.completion_popup_visible() {
                            app.hide_completion_popup();
                        } else {
                            app.input.clear();
                            app.cursor_pos = 0;
                            app.history_idx = None;
                            app.history_saved.clear();
                        }
                    }
                    (KeyCode::Tab, _) => app.next_completion(),
                    (KeyCode::BackTab, _) => app.previous_completion(),
                    (KeyCode::Char('j'), KeyModifiers::CONTROL) => app.insert_newline(),
                    (KeyCode::Char('\n' | '\r'), modifiers) if is_multiline_enter(modifiers) => {
                        app.insert_newline()
                    }
                    // Some Linux terminals report Shift+Enter as an ESC-prefixed Enter
                    // sequence, which crossterm surfaces as Alt+Enter via its fallback parser.
                    (KeyCode::Enter, modifiers) if is_multiline_enter(modifiers) => {
                        app.insert_newline()
                    }
                    (KeyCode::Enter, _) => {
                        if app.completion_popup_visible() {
                            app.accept_completion_and_submit(cmd_tx);
                        } else {
                            app.submit_input(cmd_tx);
                        }
                    }
                    (KeyCode::Char(ch), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                        app.insert_char(ch)
                    }
                    _ => {}
                }
            }

            if !app.should_quit {
                if full_redraw {
                    redraw_dashboard(stdout, app, ui)?;
                } else {
                    redraw_prompt_area(stdout, app, ui)?;
                }
            }
        }
        _ => {}
    }

    Ok(())
}

fn handle_confirmation_key(key: KeyEvent, app: &mut App) {
    match key.code {
        KeyCode::Enter => {
            if let Some(confirm_state) = app.confirmation.as_ref() {
                confirm(app, confirm_state.default);
            }
        }
        KeyCode::Char(ch) => {
            if let Some(confirm_state) = app.confirmation.as_ref() {
                let key = ch.to_ascii_lowercase();
                if confirm_state.accept_keys.contains(&key) {
                    confirm(app, true);
                } else if confirm_state.reject_keys.contains(&key) {
                    confirm(app, false);
                }
            }
        }
        KeyCode::Esc => confirm(app, false),
        _ => {}
    }
}

fn confirm(app: &mut App, accepted: bool) {
    if let Some(confirm) = app.confirmation.take() {
        let _ = confirm.reply.send(accepted);
    }
}

fn handle_selection_key(key: KeyEvent, app: &mut App) {
    let Some(selection) = app.selection.as_mut() else {
        return;
    };
    match key.code {
        KeyCode::Up | KeyCode::BackTab => {
            selection.selected = selection.selected.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Tab => {
            selection.selected = (selection.selected + 1).min(selection.options.len() - 1);
        }
        KeyCode::Enter => {
            let selected = selection.selected;
            if let Some(selection) = app.selection.take() {
                let _ = selection.reply.send(Some(selected));
            }
        }
        KeyCode::Esc => {
            if let Some(selection) = app.selection.take() {
                let _ = selection.reply.send(None);
            }
        }
        _ => {}
    }
}

fn handle_message(
    msg: UiMessage,
    app: &mut App,
    stdout: &mut Stdout,
    ui: &mut TerminalUi,
) -> Result<bool> {
    match msg {
        UiMessage::Info(text) => {
            let lines = split_transcript(&text, TranscriptKind::Info);
            app.messages.extend(lines.iter().cloned());
            if !app.suspended {
                redraw_dashboard(stdout, app, ui)?;
            }
        }
        UiMessage::Success(text) => {
            let lines = split_transcript(&text, TranscriptKind::Success);
            app.messages.extend(lines.iter().cloned());
            if !app.suspended {
                redraw_dashboard(stdout, app, ui)?;
            }
        }
        UiMessage::Tip(text) => {
            let line = TranscriptLine {
                text,
                kind: TranscriptKind::Tip,
                continuation: false,
            };
            app.messages.push(line.clone());
            if !app.suspended {
                redraw_dashboard(stdout, app, ui)?;
            }
        }
        UiMessage::Muted(text) => {
            let lines = split_transcript(&text, TranscriptKind::Muted);
            app.messages.extend(lines.iter().cloned());
            if !app.suspended {
                redraw_dashboard(stdout, app, ui)?;
            }
        }
        UiMessage::Error(text) => {
            app.last_error = Some(text);
            if !app.suspended {
                redraw_dashboard(stdout, app, ui)?;
            } else {
                app.redraw_on_resume = true;
            }
        }
        UiMessage::StatusUpdate(status) => {
            let mut next = status;
            if next.directory.is_empty() {
                next.directory = app.status.directory.clone();
            }
            if next.branch.is_none() {
                next.branch = app.status.branch.clone();
            }
            app.status = next;
            if !app.suspended {
                redraw_dashboard(stdout, app, ui)?;
            }
        }
        UiMessage::Suspend { ack } => {
            queue!(stdout, Show)?;
            stdout.flush()?;
            leave_tui()?;
            app.suspended = true;
            let _ = ack.send(());
            return Ok(false);
        }
        UiMessage::Resume => {
            enter_tui()?;
            flush_stdin_input_buffer();
            app.input.clear();
            app.cursor_pos = 0;
            app.clear_completion();
            app.input_suppressed_until = Some(Instant::now() + Duration::from_millis(500));
            app.suspended = false;
            let _redraw_pending = app.redraw_on_resume;
            app.redraw_on_resume = false;
            ui.cursor_row = 0;
            ui.cursor_col = 0;
            redraw_dashboard(stdout, app, ui)?;
            return Ok(true);
        }
        UiMessage::ConfirmationRequest {
            prompt,
            suffix,
            default,
            accept_keys,
            reject_keys,
            reply,
        } => {
            app.confirmation = Some(ConfirmationState {
                prompt,
                suffix,
                default,
                accept_keys,
                reject_keys,
                reply,
            });
            if !app.suspended {
                redraw_dashboard(stdout, app, ui)?;
            }
        }
        UiMessage::SelectionRequest {
            prompt,
            options,
            reply,
        } => {
            app.selection = Some(SelectionState {
                prompt,
                options,
                selected: 0,
                reply,
            });
            if !app.suspended {
                redraw_dashboard(stdout, app, ui)?;
            }
        }
    }
    Ok(false)
}

#[derive(Clone)]
struct StyledSegment {
    text: String,
    color: Color,
    bold: bool,
    link: Option<String>,
}

#[derive(Clone, Default)]
struct StyledLine {
    segments: Vec<StyledSegment>,
}

impl StyledLine {
    fn plain(text: impl Into<String>, color: Color) -> Self {
        Self {
            segments: vec![StyledSegment {
                text: text.into(),
                color,
                bold: false,
                link: None,
            }],
        }
    }

    fn bold(text: impl Into<String>, color: Color) -> Self {
        Self {
            segments: vec![StyledSegment {
                text: text.into(),
                color,
                bold: true,
                link: None,
            }],
        }
    }
}

#[derive(Clone, Copy)]
enum LineStyle {
    Normal,
    Logo,
    MetaBox,
    FramedBlock,
}

struct DashboardLine {
    line: StyledLine,
    style: LineStyle,
}

impl DashboardLine {
    fn new(line: StyledLine) -> Self {
        Self {
            line,
            style: LineStyle::Normal,
        }
    }

    fn styled(line: StyledLine, style: LineStyle) -> Self {
        Self { line, style }
    }
}

fn redraw_dashboard(stdout: &mut Stdout, app: &App, ui: &mut TerminalUi) -> Result<()> {
    let (width, height) = terminal_size_usize();
    let prompt = dashboard_prompt(app, width);
    let prompt_rows = prompt.lines.len().max(1);
    let prompt_accessory = prompt_accessory_lines(app, width);
    let accessory_rows = prompt_accessory.len();
    let footer_row = height.saturating_sub(1);
    let footer_separator_row = footer_row.saturating_sub(1);
    let prompt_top = footer_separator_row.saturating_sub(prompt_rows + accessory_rows);
    let prompt_separator_row = prompt_top.saturating_sub(1);
    let body_rows = prompt_separator_row;
    let lines = dashboard_lines(app, width, body_rows);

    queue!(stdout, Hide)?;
    for row in 0..height {
        queue!(
            stdout,
            MoveTo(0, row as u16),
            Clear(ClearType::UntilNewLine)
        )?;
    }

    for (row, line) in lines.iter().take(body_rows).enumerate() {
        queue!(stdout, MoveTo(0, row as u16))?;
        print_dashboard_line(stdout, line, width)?;
    }

    queue!(stdout, MoveTo(0, prompt_separator_row as u16))?;
    print_styled_line(stdout, &separator_line(width), width)?;

    for (idx, line) in prompt.lines.iter().enumerate() {
        let row = prompt_top + idx;
        if row >= footer_row {
            break;
        }
        queue!(stdout, MoveTo(0, row as u16))?;
        print_styled_line(stdout, line, width)?;
    }

    for (idx, line) in prompt_accessory.iter().enumerate() {
        let row = prompt_top + prompt_rows + idx;
        if row >= footer_row {
            break;
        }
        queue!(stdout, MoveTo(0, row as u16))?;
        print_styled_line(stdout, line, width)?;
    }

    queue!(stdout, MoveTo(0, footer_separator_row as u16))?;
    print_styled_line(stdout, &separator_line(width), width)?;

    queue!(stdout, MoveTo(0, footer_row as u16))?;
    print_styled_line(stdout, &footer_line(app, width), width)?;

    ui.cursor_row = (prompt_top + prompt.cursor_row as usize).min(footer_row) as u16;
    ui.cursor_col = prompt.cursor_col.min(width.saturating_sub(1) as u16);
    ui.prompt_area_top = prompt_separator_row as u16;
    queue!(stdout, MoveTo(ui.cursor_col, ui.cursor_row), Show)?;
    stdout.flush()?;
    Ok(())
}

fn redraw_prompt_area(stdout: &mut Stdout, app: &App, ui: &mut TerminalUi) -> Result<()> {
    let (width, height) = terminal_size_usize();
    let prompt = dashboard_prompt(app, width);
    let prompt_rows = prompt.lines.len().max(1);
    let prompt_accessory = prompt_accessory_lines(app, width);
    let accessory_rows = prompt_accessory.len();
    let footer_row = height.saturating_sub(1);
    let footer_separator_row = footer_row.saturating_sub(1);
    let prompt_top = footer_separator_row.saturating_sub(prompt_rows + accessory_rows);
    let prompt_separator_row = prompt_top.saturating_sub(1);
    let clear_from = usize::from(ui.prompt_area_top).min(prompt_separator_row);

    queue!(stdout, Hide)?;
    for row in clear_from..=footer_row {
        queue!(
            stdout,
            MoveTo(0, row as u16),
            Clear(ClearType::UntilNewLine)
        )?;
    }

    queue!(stdout, MoveTo(0, prompt_separator_row as u16))?;
    print_styled_line(stdout, &separator_line(width), width)?;

    for (idx, line) in prompt.lines.iter().enumerate() {
        let row = prompt_top + idx;
        if row >= footer_row {
            break;
        }
        queue!(stdout, MoveTo(0, row as u16))?;
        print_styled_line(stdout, line, width)?;
    }

    for (idx, line) in prompt_accessory.iter().enumerate() {
        let row = prompt_top + prompt_rows + idx;
        if row >= footer_row {
            break;
        }
        queue!(stdout, MoveTo(0, row as u16))?;
        print_styled_line(stdout, line, width)?;
    }

    queue!(stdout, MoveTo(0, footer_separator_row as u16))?;
    print_styled_line(stdout, &separator_line(width), width)?;

    queue!(stdout, MoveTo(0, footer_row as u16))?;
    print_styled_line(stdout, &footer_line(app, width), width)?;

    ui.cursor_row = (prompt_top + prompt.cursor_row as usize).min(footer_row) as u16;
    ui.cursor_col = prompt.cursor_col.min(width.saturating_sub(1) as u16);
    ui.prompt_area_top = prompt_separator_row as u16;
    queue!(stdout, MoveTo(ui.cursor_col, ui.cursor_row), Show)?;
    stdout.flush()?;
    Ok(())
}

fn dashboard_lines(app: &App, width: usize, max_lines: usize) -> Vec<DashboardLine> {
    let mut lines = Vec::new();
    lines.extend(header_lines(app, width));
    lines.extend(project_and_milestone_lines(app, width));
    lines.extend(activity_area_lines(
        app,
        width,
        max_lines.saturating_sub(lines.len()),
    ));

    lines.truncate(max_lines);
    lines
}

fn header_lines(app: &App, width: usize) -> Vec<DashboardLine> {
    const HEADER_INSET: usize = 2;
    let logo = ferrus_logo_lines();
    let logo_width = logo
        .iter()
        .map(|line| display_width(line))
        .max()
        .unwrap_or(18)
        .min(width.saturating_sub(HEADER_INSET));
    let mut lines = Vec::new();

    lines.push(DashboardLine::new(StyledLine::plain("", Color::DarkGrey)));
    for idx in 0..logo.len() {
        let logo = pad_or_truncate(logo.get(idx).copied().unwrap_or(""), logo_width);
        lines.push(DashboardLine::styled(
            StyledLine {
                segments: vec![StyledSegment {
                    text: format!("{}{logo}", " ".repeat(HEADER_INSET)),
                    color: orange(),
                    bold: true,
                    link: None,
                }],
            },
            LineStyle::Logo,
        ));
    }
    lines.push(DashboardLine::new(StyledLine::plain("", Color::DarkGrey)));
    for line in version_box_lines(app) {
        lines.push(DashboardLine::styled(
            StyledLine::plain(truncate_to_width(&format!("  {line}"), width), Color::Grey),
            LineStyle::MetaBox,
        ));
    }
    lines.push(DashboardLine::new(StyledLine::plain("", Color::DarkGrey)));
    lines.push(DashboardLine::new(tip_line(width)));
    lines.push(DashboardLine::new(StyledLine::plain("", Color::DarkGrey)));
    lines
}

fn tip_line(width: usize) -> StyledLine {
    const TIP_INSET: usize = 1;
    let tip = "Tip: /spec to create a spec · /task to start a task · /help for all commands";
    let mut line = StyledLine {
        segments: vec![StyledSegment {
            text: " ".repeat(TIP_INSET.min(width)),
            color: Color::DarkGrey,
            bold: false,
            link: None,
        }],
    };
    let mut remaining = width.saturating_sub(TIP_INSET);
    let mut saw_word = false;
    for part in tip.split(' ') {
        if remaining == 0 {
            break;
        }
        let spacer = usize::from(saw_word);
        if spacer > 0 {
            line.segments.push(StyledSegment {
                text: " ".to_string(),
                color: Color::DarkGrey,
                bold: false,
                link: None,
            });
            remaining = remaining.saturating_sub(1);
        }
        let text = truncate_to_width(part, remaining);
        if text.is_empty() {
            break;
        }
        let color = if text.starts_with('/') {
            orange()
        } else if text == "Tip:" {
            Color::DarkGrey
        } else {
            Color::Grey
        };
        remaining = remaining.saturating_sub(display_width(&text));
        line.segments.push(StyledSegment {
            text,
            color,
            bold: false,
            link: None,
        });
        saw_word = true;
    }
    line
}

fn version_box_lines(app: &App) -> Vec<String> {
    let Some(startup) = app.startup.as_ref() else {
        return Vec::new();
    };
    let body = [
        format!("version:    {}", startup.version),
        agent_version_line(
            "supervisor:",
            &startup.supervisor_type,
            &startup.supervisor_version,
        ),
        agent_version_line(
            "executor:  ",
            &startup.executor_type,
            &startup.executor_version,
        ),
    ];
    let inner = body
        .iter()
        .map(|line| display_width(line))
        .max()
        .unwrap_or(1);
    let border = "─".repeat(inner + 2);
    let mut lines = vec![format!("╭{border}╮")];
    lines.extend(body.into_iter().map(|line| {
        let padding = inner.saturating_sub(display_width(&line));
        format!("│ {line}{} │", " ".repeat(padding))
    }));
    lines.push(format!("╰{border}╯"));
    lines
}

fn agent_version_line(label: &str, agent_type: &str, agent_details: &str) -> String {
    if agent_details.is_empty() {
        format!("{label} {agent_type}")
    } else {
        format!("{label} {agent_type} {agent_details}")
    }
}

fn ferrus_logo_lines() -> &'static [&'static str] {
    &[
        "███████  ███████  █████   █████   ██   ██  ███████",
        "██       ██       ██  ██  ██  ██  ██   ██  ██",
        "█████    █████    █████   █████   ██   ██  ███████",
        "██       ██       ██  ██  ██  ██  ██   ██       ██",
        "██       ███████  ██  ██  ██  ██   █████   ███████",
    ]
}

fn separator_line(width: usize) -> StyledLine {
    StyledLine::plain("─".repeat(width.max(1)), Color::DarkGrey)
}

fn project_and_milestone_lines(app: &App, width: usize) -> Vec<DashboardLine> {
    const SECTION_INSET: usize = 2;
    if width < 12 + SECTION_INSET * 2 {
        return Vec::new();
    }
    let block_width = width.saturating_sub(SECTION_INSET * 2);
    let inner_width = block_width.saturating_sub(2);
    let left_width = (inner_width / 2)
        .clamp(32, 58)
        .min(inner_width.saturating_sub(3));
    let right_width = inner_width.saturating_sub(left_width + 1);
    let mut left = vec![
        frame_cell(&section_title("Project")),
        frame_cell(&format!("repo:        {}", app.status.directory)),
        format!(
            "branch:      {}",
            app.status.branch.as_deref().unwrap_or("-")
        ),
        format!(
            "spec:        {}",
            app.status.selected_spec.as_deref().unwrap_or("-")
        ),
    ];
    let mut right = vec![frame_cell(&section_title("Milestones"))];
    right.extend(milestone_lines(app, right_width));
    for line in &mut left[2..] {
        *line = frame_cell(line);
    }
    while left.len() < right.len() {
        left.push(String::new());
    }
    while right.len() < left.len() {
        right.push(String::new());
    }

    let mut rows = Vec::new();
    rows.push(format!(
        "╭{}┬{}╮",
        "─".repeat(left_width),
        "─".repeat(right_width)
    ));
    rows.extend(left.into_iter().zip(right).map(|(left, right)| {
        format!(
            "│{}│{}│",
            pad_or_truncate(&left, left_width),
            pad_or_truncate(&right, right_width)
        )
    }));
    rows.push(format!(
        "├{}┴{}┤",
        "─".repeat(left_width),
        "─".repeat(right_width)
    ));
    let task_counts = task_counts_line(app);
    rows.push(format!(
        "│{}│",
        pad_or_truncate(&format!(" {task_counts}"), inner_width)
    ));
    rows.push(format!("╰{}╯", "─".repeat(inner_width)));

    rows.into_iter()
        .map(|line| {
            let line = format!("{}{}", " ".repeat(SECTION_INSET), line);
            DashboardLine::styled(
                StyledLine::plain(truncate_to_width(&line, width), Color::Grey),
                LineStyle::FramedBlock,
            )
        })
        .collect()
}

fn frame_cell(text: &str) -> String {
    format!(" {text}")
}

fn milestone_lines(app: &App, width: usize) -> Vec<String> {
    if app.status.selected_milestones.is_empty() {
        return vec![frame_cell("no milestones")];
    }

    let content_width = width.saturating_sub(2);
    let status_width = display_width("pending");
    app.status
        .selected_milestones
        .iter()
        .map(|milestone| {
            let label = format!("{}:", milestone_marker_label(&milestone.marker));
            let status = milestone.readiness.as_str();
            let title_width = content_width
                .saturating_sub(display_width(&label) + status_width + 2)
                .max(8);
            let title = truncate_to_width(&milestone.title, title_width);
            frame_cell(&format!(
                "{label} {} {status:<status_width$}",
                pad_or_truncate(&title, title_width)
            ))
        })
        .collect()
}

fn milestone_marker_label(marker: &str) -> String {
    marker
        .strip_prefix('#')
        .map(|marker| format!("M{marker}"))
        .unwrap_or_else(|| marker.to_string())
}

fn activity_area_lines(app: &App, width: usize, max_lines: usize) -> Vec<DashboardLine> {
    if max_lines == 0 {
        return Vec::new();
    }

    let mut lines = Vec::new();
    if app.question.is_some() {
        lines.push(DashboardLine::new(StyledLine::plain("", Color::DarkGrey)));
        lines.extend(question_lines(app, width));
    } else if let Some(error) = app.last_error.as_deref() {
        lines.push(DashboardLine::new(StyledLine::plain("", Color::DarkGrey)));
        lines.extend(error_lines(error, width));
    }

    let remaining = max_lines.saturating_sub(lines.len());
    if remaining == 0 {
        lines.truncate(max_lines);
        return lines;
    }

    let activity_blocks = recent_transcript_blocks(&app.messages, remaining);

    if activity_blocks.is_empty() {
        lines.extend(runtime_task_activity_lines(app, width, remaining));
        let remaining = max_lines.saturating_sub(lines.len());
        lines.extend(app.runtime_runs.iter().take(remaining).map(|run| {
            DashboardLine::new(StyledLine::plain(
                truncate_to_width(
                    &format!(
                        "  {}  {}  {}  {}",
                        short_time(&run.updated_at),
                        run.task_id,
                        run.role,
                        run.status
                    ),
                    width,
                ),
                Color::DarkGrey,
            ))
        }));
    } else {
        let rendered_block_lines = activity_blocks.iter().map(Vec::len).sum::<usize>()
            + activity_blocks.len().saturating_sub(1);
        let include_leading_gap = lines.len() + rendered_block_lines < max_lines;

        for (block_idx, block) in activity_blocks.into_iter().enumerate() {
            if block_idx > 0 || include_leading_gap {
                push_activity_gap(&mut lines);
                if lines.len() >= max_lines {
                    break;
                }
            }
            for line in block {
                lines.push(DashboardLine::new(transcript_activity_line(&line, width)));
                if lines.len() >= max_lines {
                    break;
                }
            }
        }
    }

    lines.truncate(max_lines);
    lines
}

fn recent_transcript_blocks(
    messages: &[TranscriptLine],
    max_lines: usize,
) -> Vec<Vec<TranscriptLine>> {
    if max_lines == 0 || messages.is_empty() {
        return Vec::new();
    }

    let mut blocks = Vec::new();
    let mut current = Vec::new();
    for line in messages {
        if !line.continuation && !current.is_empty() {
            blocks.push(std::mem::take(&mut current));
        }
        current.push(line.clone());
    }
    if !current.is_empty() {
        blocks.push(current);
    }

    let mut selected = Vec::new();
    let mut remaining = max_lines;
    for block in blocks.into_iter().rev() {
        if remaining == 0 {
            break;
        }

        let gap_before_newer_block = usize::from(!selected.is_empty());
        if remaining <= gap_before_newer_block {
            break;
        }

        let take = block.len().min(remaining - gap_before_newer_block);
        selected.push(block.into_iter().take(take).collect::<Vec<_>>());
        remaining -= gap_before_newer_block + take;
    }
    selected.reverse();
    selected
}

fn runtime_task_activity_lines(app: &App, width: usize, max_lines: usize) -> Vec<DashboardLine> {
    app.runtime_tasks
        .iter()
        .filter(|task| dashboard_task_status(task.status.as_str()))
        .take(max_lines)
        .map(|task| {
            DashboardLine::new(StyledLine::plain(
                truncate_to_width(&runtime_task_activity_text(task), width),
                task_activity_color(task.status.as_str()),
            ))
        })
        .collect()
}

fn runtime_task_activity_text(task: &TaskRecord) -> String {
    let detail = task
        .claimed_by
        .as_deref()
        .or(task.milestone_id.as_deref())
        .or(task.spec_path.as_deref())
        .unwrap_or(task.path.as_str());
    format!("  {:<7} {:<14} {}", task.id, task.status, detail)
}

fn dashboard_task_status(status: &str) -> bool {
    matches!(
        status,
        "pending" | "executing" | "addressing" | "reviewing" | "consultation" | "awaiting_human"
    )
}

fn task_activity_color(status: &str) -> Color {
    match status {
        "pending" => Color::Yellow,
        "awaiting_human" => Color::Magenta,
        "reviewing" | "consultation" => Color::Cyan,
        "executing" | "addressing" => Color::Green,
        _ => Color::Grey,
    }
}

fn push_activity_gap(lines: &mut Vec<DashboardLine>) {
    let last_is_blank = lines.last().is_some_and(|line| {
        line.line
            .segments
            .iter()
            .all(|segment| segment.text.is_empty())
    });
    if !last_is_blank {
        lines.push(DashboardLine::new(StyledLine::plain("", Color::DarkGrey)));
    }
}

fn question_lines(app: &App, width: usize) -> Vec<DashboardLine> {
    let mut lines = vec![DashboardLine::new(StyledLine::bold("  Question", orange()))];
    let question = app
        .question
        .as_deref()
        .unwrap_or("Type your answer and press Enter.");
    for line in question.lines().take(3) {
        lines.push(DashboardLine::new(StyledLine::plain(
            truncate_to_width(&format!("  {line}"), width),
            Color::Grey,
        )));
    }
    lines
}

fn error_lines(error: &str, width: usize) -> Vec<DashboardLine> {
    let mut lines = vec![DashboardLine::new(StyledLine::bold("  Error", Color::Red))];
    for line in error.lines().filter(|line| !line.trim().is_empty()).take(8) {
        lines.push(DashboardLine::new(StyledLine::plain(
            truncate_to_width(&format!("  {line}"), width),
            Color::Red,
        )));
    }
    lines
}

fn selection_dashboard_lines(app: &App, width: usize) -> Vec<StyledLine> {
    let mut lines = vec![separator_line(width), StyledLine::bold("Select", orange())];
    if let Some(selection) = app.selection.as_ref() {
        lines.extend(
            visible_selection_rows(selection)
                .into_iter()
                .map(|(selected, text)| {
                    let marker = if selected { "> " } else { "  " };
                    StyledLine::plain(
                        truncate_to_width(&format!("{marker}{text}"), width),
                        if selected { Color::Yellow } else { Color::Grey },
                    )
                }),
        );
    }
    lines
}

fn completion_dashboard_lines(app: &App, width: usize) -> Vec<StyledLine> {
    let mut lines = vec![
        separator_line(width),
        StyledLine::bold("Commands", orange()),
    ];
    lines.extend(visible_completion_rows(app).into_iter().map(
        |(selected, command, description)| {
            let marker = if selected { "> " } else { "  " };
            StyledLine::plain(
                truncate_to_width(&format!("{marker}{command:<14} {description}"), width),
                if selected { Color::Yellow } else { Color::Grey },
            )
        },
    ));
    lines
}

struct DashboardPrompt {
    lines: Vec<StyledLine>,
    cursor_row: u16,
    cursor_col: u16,
}

fn dashboard_prompt(app: &App, width: usize) -> DashboardPrompt {
    if let Some(confirm) = app.confirmation.as_ref() {
        let prompt = truncate_to_width(
            &format!("{} {}", confirm.prompt, confirm.suffix),
            width.max(1),
        );
        return DashboardPrompt {
            cursor_row: 0,
            cursor_col: prompt.chars().count() as u16,
            lines: vec![StyledLine::plain(prompt, Color::Yellow)],
        };
    }
    if let Some(selection) = app.selection.as_ref() {
        let prompt = truncate_to_width(&selection.prompt, width.max(1));
        return DashboardPrompt {
            cursor_row: 0,
            cursor_col: prompt.chars().count() as u16,
            lines: vec![StyledLine::plain(prompt, Color::Yellow)],
        };
    }

    let prefix = if app.question.is_some() {
        "Answer: > "
    } else {
        "> "
    };
    render_prompt_with_prefix(app, width, prefix)
}

fn prompt_accessory_lines(app: &App, width: usize) -> Vec<StyledLine> {
    if app.selection.is_some() {
        selection_dashboard_lines(app, width)
    } else if app.completion_popup_visible() {
        completion_dashboard_lines(app, width)
    } else {
        Vec::new()
    }
}

fn footer_line(app: &App, width: usize) -> StyledLine {
    if app.ctrl_c_pending {
        return footer_with_debug(
            "Press Ctrl+C again within 2s to exit",
            Color::Yellow,
            true,
            app.debug,
            width,
        );
    }

    let footer = format!(
        "Tab complete  •  ↑/↓ history  •  Ctrl+L refresh  •  {} running  •  {} waiting  •  {} pending  •  {} ready  •  {} done",
        count_tasks(
            app,
            &[
                TaskStatus::Executing,
                TaskStatus::Addressing,
                TaskStatus::Reviewing,
                TaskStatus::Consultation,
            ],
        ),
        count_tasks(app, &[TaskStatus::AwaitingHuman]),
        count_milestones(app, MilestoneReadiness::Pending),
        count_milestones(app, MilestoneReadiness::Ready),
        count_tasks(app, &[TaskStatus::Complete])
    );
    footer_with_debug(&footer, Color::DarkGrey, false, app.debug, width)
}

fn footer_with_debug(
    left: &str,
    left_color: Color,
    left_bold: bool,
    debug: bool,
    width: usize,
) -> StyledLine {
    if !debug {
        return StyledLine {
            segments: vec![StyledSegment {
                text: truncate_to_width(left, width),
                color: left_color,
                bold: left_bold,
                link: None,
            }],
        };
    }

    let indicator = "debug";
    let indicator_width = display_width(indicator);
    if width <= indicator_width {
        return StyledLine::bold(truncate_to_width(indicator, width), Color::DarkBlue);
    }

    let left_limit = width.saturating_sub(indicator_width + 2);
    let left = truncate_to_width(left, left_limit);
    let spacing = width
        .saturating_sub(display_width(&left) + indicator_width)
        .max(1);
    StyledLine {
        segments: vec![
            StyledSegment {
                text: left,
                color: left_color,
                bold: left_bold,
                link: None,
            },
            StyledSegment {
                text: " ".repeat(spacing),
                color: Color::DarkGrey,
                bold: false,
                link: None,
            },
            StyledSegment {
                text: indicator.to_string(),
                color: Color::DarkBlue,
                bold: true,
                link: None,
            },
        ],
    }
}

fn print_styled_line(stdout: &mut Stdout, line: &StyledLine, width: usize) -> Result<()> {
    let mut remaining = width;
    for segment in &line.segments {
        if remaining == 0 {
            break;
        }
        let text = truncate_to_width(&segment.text, remaining);
        if text.is_empty() {
            continue;
        }
        if let Some(link) = segment.link.as_deref() {
            queue!(stdout, Print(format!("\x1b]8;;{link}\x1b\\")))?;
        }
        let styled = style(text.clone()).with(segment.color);
        if segment.bold {
            queue!(
                stdout,
                PrintStyledContent(styled.attribute(Attribute::Bold))
            )?;
        } else {
            queue!(stdout, PrintStyledContent(styled))?;
        }
        if segment.link.is_some() {
            queue!(stdout, Print("\x1b]8;;\x1b\\"))?;
        }
        remaining = remaining.saturating_sub(display_width(&text));
    }
    Ok(())
}

fn print_dashboard_line(stdout: &mut Stdout, line: &DashboardLine, width: usize) -> Result<()> {
    match line.style {
        LineStyle::Logo => print_logo_dashboard_line(stdout, &line.line, width),
        LineStyle::MetaBox => print_meta_dashboard_line(stdout, &line.line, width),
        LineStyle::FramedBlock => print_framed_dashboard_line(stdout, &line.line, width),
        LineStyle::Normal => print_styled_line(stdout, &line.line, width),
    }
}

fn print_logo_dashboard_line(stdout: &mut Stdout, line: &StyledLine, width: usize) -> Result<()> {
    let text = line
        .segments
        .first()
        .map(|segment| segment.text.as_str())
        .unwrap_or("");
    let visible = truncate_to_width(text, width);
    let len = visible.chars().count().max(1);
    for (idx, ch) in visible.chars().enumerate() {
        queue!(
            stdout,
            PrintStyledContent(
                style(ch.to_string())
                    .with(logo_gradient_color(idx, len))
                    .attribute(Attribute::Bold)
            )
        )?;
    }
    if line.segments.len() > 1 {
        let mut remaining = width.saturating_sub(display_width(&visible));
        if let Some(spacer) = line.segments.get(1) {
            let text = truncate_to_width(&spacer.text, remaining);
            queue!(
                stdout,
                PrintStyledContent(style(text.clone()).with(spacer.color))
            )?;
            remaining = remaining.saturating_sub(display_width(&text));
        }
        if let Some(meta) = line.segments.get(2) {
            print_meta_text(stdout, &truncate_to_width(&meta.text, remaining))?;
        }
    }
    Ok(())
}

fn print_meta_dashboard_line(stdout: &mut Stdout, line: &StyledLine, width: usize) -> Result<()> {
    let mut rendered = String::new();
    for segment in &line.segments {
        rendered.push_str(&segment.text);
    }
    let visible = truncate_to_width(&rendered, width);
    print_meta_text(stdout, &visible)?;
    Ok(())
}

fn print_meta_text(stdout: &mut Stdout, text: &str) -> Result<()> {
    let chars = text.chars().collect::<Vec<_>>();
    let first_border = chars.iter().position(|ch| *ch == '│');
    let last_border = chars.iter().rposition(|ch| *ch == '│');

    if let (Some(first), Some(last)) = (first_border, last_border) {
        for ch in &chars[..=first] {
            queue!(
                stdout,
                PrintStyledContent(style(ch.to_string()).with(meta_border_color(*ch)))
            )?;
        }
        print_version_box_body(stdout, &chars[first + 1..last].iter().collect::<String>())?;
        for ch in &chars[last..] {
            queue!(
                stdout,
                PrintStyledContent(style(ch.to_string()).with(meta_border_color(*ch)))
            )?;
        }
        return Ok(());
    }

    for ch in chars {
        queue!(
            stdout,
            PrintStyledContent(style(ch.to_string()).with(meta_border_color(ch)))
        )?;
    }
    Ok(())
}

fn print_version_box_body(stdout: &mut Stdout, body: &str) -> Result<()> {
    let Some(colon_idx) = body.find(':') else {
        queue!(
            stdout,
            PrintStyledContent(style(body.to_string()).with(Color::Grey))
        )?;
        return Ok(());
    };

    let (label, rest) = body.split_at(colon_idx + 1);
    queue!(
        stdout,
        PrintStyledContent(style(label.to_string()).with(Color::DarkGrey))
    )?;

    let leading_spaces_len = rest
        .chars()
        .take_while(|ch| ch.is_whitespace())
        .map(char::len_utf8)
        .sum::<usize>();
    let spaces = &rest[..leading_spaces_len];
    queue!(
        stdout,
        PrintStyledContent(style(spaces.to_string()).with(Color::DarkGrey))
    )?;

    let value = &rest[leading_spaces_len..];
    if label.trim_end_matches(':').trim() == "supervisor"
        || label.trim_end_matches(':').trim() == "executor"
    {
        let command_len = value
            .chars()
            .take_while(|ch| !ch.is_whitespace())
            .map(char::len_utf8)
            .sum::<usize>();
        let (command, tail) = value.split_at(command_len);
        queue!(
            stdout,
            PrintStyledContent(
                style(command.to_string())
                    .with(orange())
                    .attribute(Attribute::Bold)
            ),
            PrintStyledContent(style(tail.to_string()).with(Color::White))
        )?;
    } else {
        queue!(
            stdout,
            PrintStyledContent(style(value.to_string()).with(Color::White))
        )?;
    }

    Ok(())
}

fn meta_border_color(ch: char) -> Color {
    match ch {
        '╭' | '╮' | '╰' | '╯' | '─' | '│' => Color::DarkGrey,
        _ => Color::Grey,
    }
}

fn print_framed_dashboard_line(stdout: &mut Stdout, line: &StyledLine, width: usize) -> Result<()> {
    let text = line
        .segments
        .first()
        .map(|segment| truncate_to_width(&segment.text, width))
        .unwrap_or_default();
    let mut idx = 0;
    while idx < text.len() {
        let rest = &text[idx..];
        if let Some(title) = rest
            .strip_prefix("Project")
            .map(|_| "Project")
            .or_else(|| rest.strip_prefix("Milestones").map(|_| "Milestones"))
        {
            queue!(
                stdout,
                PrintStyledContent(
                    style(title.to_string())
                        .with(orange())
                        .attribute(Attribute::Bold)
                )
            )?;
            idx += title.len();
            continue;
        }
        if let Some(status) = rest
            .strip_prefix("done")
            .map(|_| ("done", Color::Green))
            .or_else(|| rest.strip_prefix("ready").map(|_| ("ready", Color::Blue)))
            .or_else(|| {
                rest.strip_prefix("pending")
                    .map(|_| ("pending", Color::Yellow))
            })
        {
            queue!(
                stdout,
                PrintStyledContent(style(status.0.to_string()).with(status.1))
            )?;
            idx += status.0.len();
            continue;
        }

        let ch = rest.chars().next().unwrap_or_default();
        let color = match ch {
            '╭' | '╮' | '╰' | '╯' | '─' | '│' | '├' | '┤' | '┬' | '┴' => {
                Color::DarkGrey
            }
            _ => Color::Grey,
        };
        queue!(
            stdout,
            PrintStyledContent(style(ch.to_string()).with(color))
        )?;
        idx += ch.len_utf8();
    }
    Ok(())
}

fn enter_tui() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    queue!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        Clear(ClearType::All),
        MoveTo(0, 0),
        Hide
    )?;
    platform::enter_tui(&mut stdout);
    let _ = stdout.flush();
    Ok(())
}

fn leave_tui() -> Result<()> {
    let mut stdout = io::stdout();
    platform::leave_tui(&mut stdout);
    queue!(stdout, Show, DisableBracketedPaste, LeaveAlternateScreen)?;
    let _ = stdout.flush();
    disable_raw_mode()?;
    Ok(())
}

fn flush_stdin_input_buffer() {
    platform::flush_stdin_input_buffer();

    // Some agents restore the terminal by writing ANSI sequences as they exit.
    // Those bytes can already be decoded into crossterm events, or arrive just
    // after raw mode is re-enabled. Drain until the terminal stays quiet briefly.
    const QUIET_WINDOW: Duration = Duration::from_millis(40);
    const MAX_DRAIN_TIME: Duration = Duration::from_millis(600);

    let deadline = Instant::now() + MAX_DRAIN_TIME;
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }

        let timeout = deadline.saturating_duration_since(now).min(QUIET_WINDOW);
        match event::poll(timeout) {
            Ok(true) => {
                while matches!(event::poll(Duration::ZERO), Ok(true)) {
                    if event::read().is_err() {
                        break;
                    }
                }
            }
            Ok(false) | Err(_) => break,
        }
    }
}

fn render_prompt_with_prefix(app: &App, width: usize, prefix: &str) -> DashboardPrompt {
    let prefix_width = prefix.chars().count();
    let available = width.saturating_sub(prefix_width).max(1);
    let chars: Vec<char> = app.input.chars().collect();
    let mut raw_lines = Vec::new();
    let mut cursor_row = 0u16;
    let mut cursor_col = prefix_width as u16;
    let mut line = String::new();
    let mut line_width = 0usize;
    let mut row = 0u16;

    for (idx, ch) in chars.iter().enumerate() {
        if idx == app.cursor_pos {
            cursor_row = row;
            cursor_col = prefix_width as u16 + line_width as u16;
        }

        if *ch == '\n' {
            raw_lines.push(std::mem::take(&mut line));
            line_width = 0;
            row += 1;
            continue;
        }

        if line_width == available {
            raw_lines.push(std::mem::take(&mut line));
            line_width = 0;
            row += 1;
            if idx == app.cursor_pos {
                cursor_row = row;
                cursor_col = prefix_width as u16;
            }
        }

        line.push(*ch);
        line_width += 1;
    }

    if app.cursor_pos == chars.len() {
        cursor_row = row;
        cursor_col = prefix_width as u16 + line_width as u16;
    }

    raw_lines.push(line);
    let lines = raw_lines
        .into_iter()
        .enumerate()
        .map(|(idx, line)| {
            let line_prefix = if idx == 0 {
                prefix.to_string()
            } else {
                " ".repeat(prefix_width)
            };
            StyledLine {
                segments: vec![
                    StyledSegment {
                        text: line_prefix,
                        color: orange(),
                        bold: true,
                        link: None,
                    },
                    StyledSegment {
                        text: line,
                        color: Color::White,
                        bold: false,
                        link: None,
                    },
                ],
            }
        })
        .collect();

    DashboardPrompt {
        lines,
        cursor_row,
        cursor_col,
    }
}

fn terminal_size_usize() -> (usize, usize) {
    size()
        .map(|(w, h)| (w as usize, h as usize))
        .unwrap_or((100, 30))
}

fn orange() -> Color {
    Color::Rgb {
        r: 226,
        g: 128,
        b: 18,
    }
}

fn logo_gradient_color(idx: usize, len: usize) -> Color {
    let start = (148u8, 36u8, 20u8);
    let end = (226u8, 128u8, 18u8);
    let t = if len <= 1 {
        0.0
    } else {
        idx as f32 / (len.saturating_sub(1)) as f32
    };
    let mix = |a: u8, b: u8| -> u8 { (a as f32 + (b as f32 - a as f32) * t).round() as u8 };
    Color::Rgb {
        r: mix(start.0, end.0),
        g: mix(start.1, end.1),
        b: mix(start.2, end.2),
    }
}

fn section_title(title: &str) -> String {
    title.to_string()
}

fn task_counts_line(app: &App) -> String {
    format!(
        "tasks:       {} running  {} waiting  {} queued  {} done",
        count_tasks(
            app,
            &[
                TaskStatus::Executing,
                TaskStatus::Addressing,
                TaskStatus::Reviewing,
                TaskStatus::Consultation,
            ],
        ),
        count_tasks(app, &[TaskStatus::AwaitingHuman]),
        count_tasks(app, &[TaskStatus::Pending]),
        count_tasks(app, &[TaskStatus::Complete])
    )
}

fn count_tasks(app: &App, statuses: &[TaskStatus]) -> usize {
    app.runtime_tasks
        .iter()
        .filter(|task| {
            task.status
                .parse::<TaskStatus>()
                .is_ok_and(|status| statuses.contains(&status))
        })
        .count()
}

fn count_milestones(app: &App, readiness: MilestoneReadiness) -> usize {
    app.status
        .selected_milestones
        .iter()
        .filter(|milestone| milestone.readiness == readiness)
        .count()
}

fn pad_or_truncate(text: &str, width: usize) -> String {
    let text = truncate_to_width(text, width);
    let padding = width.saturating_sub(display_width(&text));
    if padding == 0 {
        text
    } else {
        format!("{text}{}", " ".repeat(padding))
    }
}

fn short_time(value: &str) -> String {
    value
        .split('T')
        .nth(1)
        .and_then(|time| time.get(..8))
        .unwrap_or(value)
        .to_string()
}

fn activity_text(line: &TranscriptLine) -> String {
    match line.kind {
        TranscriptKind::Muted if !line.text.chars().next().is_some_and(char::is_whitespace) => {
            format!("  {}", line.text)
        }
        TranscriptKind::Success if !line.continuation => format!("• {}", line.text),
        TranscriptKind::Error if !line.continuation => format!("! {}", line.text),
        _ => line.text.clone(),
    }
}

fn transcript_activity_line(line: &TranscriptLine, width: usize) -> StyledLine {
    let text = format!("  {}", activity_text(line));
    let color = transcript_color(line.kind);
    log_path_line(&text, color, width)
        .unwrap_or_else(|| StyledLine::plain(truncate_to_width(&text, width), color))
}

fn log_path_line(text: &str, color: Color, width: usize) -> Option<StyledLine> {
    let (prefix, path) = split_log_path(text)?;
    if path.trim().is_empty() {
        return None;
    }

    let prefix = truncate_to_width(prefix, width);
    let remaining = width.saturating_sub(display_width(&prefix));
    let path_text = truncate_to_width(path, remaining);
    if path_text.is_empty() {
        return Some(StyledLine::plain(prefix, color));
    }

    Some(StyledLine {
        segments: vec![
            StyledSegment {
                text: prefix,
                color,
                bold: false,
                link: None,
            },
            StyledSegment {
                text: path_text,
                color,
                bold: false,
                link: Some(file_url_for_path(Path::new(path.trim()))),
            },
        ],
    })
}

fn split_log_path(text: &str) -> Option<(&str, &str)> {
    ["Logs: ", "tail logs: ", "tail -f "]
        .iter()
        .filter_map(|marker| {
            text.rsplit_once(marker)
                .map(|(prefix, path)| (prefix, *marker, path))
        })
        .max_by_key(|(prefix, marker, _)| prefix.len() + marker.len())
        .map(|(prefix, marker, path)| (&text[..prefix.len() + marker.len()], path))
}

fn file_url_for_path(path: &Path) -> String {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let display = absolute.to_string_lossy().replace('\\', "/");
    if display.starts_with('/') {
        format!("file://{}", percent_encode_uri_path(&display))
    } else {
        format!("file:///{}", percent_encode_uri_path(&display))
    }
}

fn percent_encode_uri_path(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                encoded.push(*byte as char)
            }
            byte => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

fn transcript_color(kind: TranscriptKind) -> Color {
    match kind {
        TranscriptKind::Info => Color::Grey,
        TranscriptKind::Success => Color::Green,
        TranscriptKind::Tip => Color::Yellow,
        TranscriptKind::Muted => Color::DarkGrey,
        TranscriptKind::Error => Color::Red,
    }
}

#[cfg(test)]
struct PromptLine {
    lines: Vec<String>,
    cursor_row: u16,
    cursor_col: u16,
}

#[cfg(test)]
fn render_prompt(app: &App, width: usize) -> PromptLine {
    let available = width.saturating_sub(2).max(1);
    let chars: Vec<char> = app.input.chars().collect();
    let mut lines = Vec::new();
    let mut cursor_row = 0u16;
    let mut cursor_col = 2u16;
    let mut line = String::new();
    let mut line_width = 0usize;
    let mut row = 0u16;

    for (idx, ch) in chars.iter().enumerate() {
        if idx == app.cursor_pos {
            cursor_row = row;
            cursor_col = 2 + line_width as u16;
        }

        if *ch == '\n' {
            lines.push(std::mem::take(&mut line));
            line_width = 0;
            row += 1;
            continue;
        }

        if line_width == available {
            lines.push(std::mem::take(&mut line));
            line_width = 0;
            row += 1;
            if idx == app.cursor_pos {
                cursor_row = row;
                cursor_col = 2;
            }
        }

        line.push(*ch);
        line_width += 1;
    }

    if app.cursor_pos == chars.len() {
        cursor_row = row;
        cursor_col = 2 + line_width as u16;
    }

    lines.push(line);
    PromptLine {
        lines,
        cursor_row,
        cursor_col,
    }
}

fn line_start(chars: &[char], pos: usize) -> usize {
    let mut idx = pos.min(chars.len());
    while idx > 0 && chars[idx - 1] != '\n' {
        idx -= 1;
    }
    idx
}

fn line_end(chars: &[char], pos: usize) -> usize {
    let mut idx = pos.min(chars.len());
    while idx < chars.len() && chars[idx] != '\n' {
        idx += 1;
    }
    idx
}

#[allow(dead_code)]
fn print_status_line(
    stdout: &mut Stdout,
    status: &StatusSnapshot,
    ctrl_c_pending: bool,
    debug: bool,
    width: usize,
) -> Result<()> {
    let max_width = width.max(1);
    if ctrl_c_pending {
        let warning = truncate_to_width("Press Ctrl+C again to exit", max_width);
        queue!(
            stdout,
            PrintStyledContent(style(warning).with(Color::Yellow))
        )?;
        return Ok(());
    }

    let state = if status.task_state.is_empty() {
        "Idle".to_string()
    } else {
        status.task_state.clone()
    };
    let mut segments = vec![(state, task_state_color(&status.task_state))];

    if let Some(elapsed) = status
        .task_state_detail
        .strip_prefix(&format!("{} (", status.task_state))
        .and_then(|detail| detail.strip_suffix(')'))
    {
        segments.push((format!(" ({elapsed})"), Color::DarkGrey));
    }

    if !status.directory.is_empty() {
        segments.push((" | ".to_string(), Color::DarkGrey));
        segments.push(("directory: ".to_string(), Color::DarkGrey));
        segments.push((status.directory.clone(), Color::Grey));
    }

    if let Some(branch) = status.branch.as_deref() {
        segments.push((" | ".to_string(), Color::DarkGrey));
        segments.push(("branch: ".to_string(), Color::DarkGrey));
        segments.push((branch.to_string(), Color::Grey));
    }

    if let Some(spec) = status.selected_spec.as_deref() {
        segments.push((" | ".to_string(), Color::DarkGrey));
        segments.push(("spec: ".to_string(), Color::DarkGrey));
        segments.push((spec.to_string(), Color::Grey));
    }

    segments.push((" | ".to_string(), Color::DarkGrey));
    segments.push(("retries: ".to_string(), Color::DarkGrey));
    segments.push((status.retries.to_string(), Color::Grey));
    segments.push((" | ".to_string(), Color::DarkGrey));
    segments.push(("cycles: ".to_string(), Color::DarkGrey));
    segments.push((status.cycles.to_string(), Color::Grey));

    let mut remaining = max_width;
    let mut left_width = 0;
    for (text, color) in segments {
        if remaining == 0 {
            break;
        }
        let visible = truncate_to_width(&text, remaining);
        if visible.is_empty() {
            break;
        }
        queue!(
            stdout,
            PrintStyledContent(style(visible.clone()).with(color))
        )?;
        let visible_width = display_width(&visible);
        left_width += visible_width;
        remaining = remaining.saturating_sub(visible_width);
    }

    // When the executor is waiting for a human answer, show a prominent hint.
    if status.task_state == "AwaitingHuman" {
        let hint = "  ← type your answer and press Enter";
        let hint_text = truncate_to_width(hint, remaining);
        if !hint_text.is_empty() {
            queue!(
                stdout,
                PrintStyledContent(
                    style(hint_text.clone())
                        .with(Color::Magenta)
                        .attribute(Attribute::Bold)
                )
            )?;
            let hint_width = display_width(&hint_text);
            left_width += hint_width;
            remaining = remaining.saturating_sub(hint_width);
        }
    } else if status.task_state == "Consultation" {
        let hint = "  ← consulting supervisor";
        let hint_text = truncate_to_width(hint, remaining);
        if !hint_text.is_empty() {
            queue!(
                stdout,
                PrintStyledContent(style(hint_text.clone()).with(Color::Cyan))
            )?;
            let hint_width = display_width(&hint_text);
            left_width += hint_width;
            remaining = remaining.saturating_sub(hint_width);
        }
    }

    if debug && remaining >= 7 {
        let pad = max_width.saturating_sub(left_width + 5);
        if pad > 0 {
            queue!(stdout, Print(" ".repeat(pad)))?;
        }
        queue!(
            stdout,
            PrintStyledContent(style("debug").with(Color::DarkBlue))
        )?;
    }

    Ok(())
}

#[allow(dead_code)]
fn print_live_area_border(stdout: &mut Stdout, width: usize) -> Result<()> {
    let border_width = width.max(1);
    queue!(
        stdout,
        PrintStyledContent(style("─".repeat(border_width)).with(Color::DarkGrey))
    )?;
    Ok(())
}

#[allow(dead_code)]
enum LiveAreaLine {
    Status,
    Selection {
        selected: bool,
        text: String,
    },
    Completion {
        selected: bool,
        command: String,
        description: String,
    },
}

#[allow(dead_code)]
fn render_lower_live_area(app: &App, width: usize) -> Vec<LiveAreaLine> {
    if let Some(selection) = app.selection.as_ref() {
        visible_selection_rows(selection)
            .into_iter()
            .map(|(selected, text)| LiveAreaLine::Selection {
                selected,
                text: truncate_to_width(text, width.max(1)),
            })
            .collect()
    } else if app.completion_popup_visible() {
        visible_completion_rows(app)
            .into_iter()
            .map(
                |(selected, command, description)| LiveAreaLine::Completion {
                    selected,
                    command: truncate_to_width(command, width.max(1)),
                    description: truncate_to_width(description, width.max(1)),
                },
            )
            .collect()
    } else {
        vec![LiveAreaLine::Status]
    }
}

fn visible_selection_rows(selection: &SelectionState) -> Vec<(bool, &String)> {
    let total = selection.options.len();
    if total == 0 {
        return Vec::new();
    }
    let window = total.min(6);
    let half = window / 2;
    let start = selection
        .selected
        .saturating_sub(half)
        .min(total.saturating_sub(window));
    selection.options[start..start + window]
        .iter()
        .enumerate()
        .map(|(offset, option)| (start + offset == selection.selected, option))
        .collect()
}

fn visible_completion_rows(app: &App) -> Vec<(bool, &'static str, &'static str)> {
    let total = app.completion_candidates.len();
    if total == 0 {
        return Vec::new();
    }
    let window = total.min(3);
    let start = if total <= window {
        0
    } else {
        app.completion_selected.min(total.saturating_sub(window))
    };
    app.completion_candidates[start..start + window]
        .iter()
        .enumerate()
        .map(|(offset, (cmd, desc))| (start + offset == app.completion_selected, *cmd, *desc))
        .collect()
}

#[allow(dead_code)]
fn print_live_area_line(
    stdout: &mut Stdout,
    line: &LiveAreaLine,
    ctrl_c_pending: bool,
    status: &StatusSnapshot,
    debug: bool,
    width: usize,
) -> Result<()> {
    match line {
        LiveAreaLine::Status => print_status_line(stdout, status, ctrl_c_pending, debug, width),
        LiveAreaLine::Selection { selected, text } => {
            print_selection_line(stdout, *selected, text, width)
        }
        LiveAreaLine::Completion {
            selected,
            command,
            description,
        } => print_completion_line(stdout, *selected, command, description, width),
    }
}

#[allow(dead_code)]
fn print_selection_line(
    stdout: &mut Stdout,
    selected: bool,
    text: &str,
    width: usize,
) -> Result<()> {
    let marker = if selected { "› " } else { "  " };
    let text = truncate_to_width(text, width.saturating_sub(marker.chars().count()).max(1));
    if selected {
        queue!(
            stdout,
            PrintStyledContent(style(marker).with(Color::Yellow)),
            PrintStyledContent(style(text).with(Color::Yellow).attribute(Attribute::Bold))
        )?;
    } else {
        queue!(
            stdout,
            PrintStyledContent(style(marker).with(Color::DarkGrey)),
            PrintStyledContent(style(text).with(Color::Grey))
        )?;
    }
    Ok(())
}

#[allow(dead_code)]
fn print_completion_line(
    stdout: &mut Stdout,
    selected: bool,
    command: &str,
    description: &str,
    width: usize,
) -> Result<()> {
    let marker = if selected { "› " } else { "  " };
    let command_width = command.chars().count();
    let separator = if description.is_empty() { "" } else { "  " };
    let used = marker.chars().count() + command_width + separator.chars().count();
    let desc_width = width.saturating_sub(used).max(1);
    let desc = truncate_to_width(description, desc_width);

    if selected {
        queue!(
            stdout,
            PrintStyledContent(style(marker).with(Color::Yellow)),
            PrintStyledContent(
                style(command)
                    .with(Color::Yellow)
                    .attribute(Attribute::Bold)
            )
        )?;
    } else {
        queue!(
            stdout,
            PrintStyledContent(style(marker).with(Color::DarkGrey)),
            PrintStyledContent(style(command).with(Color::Grey))
        )?;
    }

    if !desc.is_empty() {
        queue!(
            stdout,
            PrintStyledContent(style(separator).with(Color::DarkGrey)),
            PrintStyledContent(style(desc).with(Color::DarkGrey))
        )?;
    }
    Ok(())
}

#[allow(dead_code)]
fn task_state_color(task_state: &str) -> Color {
    match task_state {
        "Idle" => Color::DarkGrey,
        "Executing" => Color::Yellow,
        "Consultation" => Color::Blue,
        "Reviewing" | "Addressing" => Color::Cyan,
        "Complete" => Color::Green,
        "Failed" => Color::Red,
        "AwaitingHuman" => Color::Magenta,
        _ => Color::White,
    }
}

fn split_transcript(text: &str, kind: TranscriptKind) -> Vec<TranscriptLine> {
    let mut lines = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        lines.push(TranscriptLine {
            text: line.to_string(),
            kind,
            continuation: idx > 0,
        });
    }
    if lines.is_empty() {
        lines.push(TranscriptLine {
            text: String::new(),
            kind,
            continuation: false,
        });
    }
    lines
}

#[allow(dead_code)]
fn terminal_width() -> u16 {
    size().map(|(w, _)| w).unwrap_or(80)
}

fn truncate_to_width(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

fn display_width(text: &str) -> usize {
    text.chars().count()
}

fn is_multiline_enter(modifiers: KeyModifiers) -> bool {
    let multiline = KeyModifiers::SHIFT | KeyModifiers::ALT;
    let disallowed =
        KeyModifiers::CONTROL | KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META;
    modifiers.intersects(multiline) && !modifiers.intersects(disallowed)
}

fn history_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_default()
        .join("ferrus")
        .join("history")
}

fn load_history() -> Vec<String> {
    let Ok(contents) = fs::read_to_string(history_path()) else {
        return Vec::new();
    };
    let mut lines: Vec<String> = contents.lines().map(ToOwned::to_owned).collect();
    if lines.len() > MAX_HISTORY {
        lines = lines.split_off(lines.len() - MAX_HISTORY);
    }
    lines
}

fn save_history(history: &[String]) {
    let path = history_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let keep_from = history.len().saturating_sub(MAX_HISTORY);
    let data = history[keep_from..].join("\n");
    let _ = fs::write(path, data);
}

fn current_dir_label() -> String {
    env::current_dir()
        .ok()
        .map(|path| abbreviate_home(&path))
        .unwrap_or_else(|| ".".to_string())
}

fn current_git_branch() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() || branch == "HEAD" {
        None
    } else {
        Some(branch)
    }
}

fn longest_common_prefix(candidates: &[(&'static str, &'static str)]) -> &'static str {
    let Some((first, _)) = candidates.first() else {
        return "";
    };

    let mut end = first.len();
    for (candidate, _) in candidates.iter().skip(1) {
        end = first
            .bytes()
            .zip(candidate.bytes())
            .take_while(|(a, b)| a == b)
            .count()
            .min(end);
    }
    &first[..end]
}

fn abbreviate_home(path: &Path) -> String {
    let Some(home) = dirs::home_dir() else {
        return path.display().to_string();
    };

    if path == home {
        "~".to_string()
    } else if let Ok(suffix) = path.strip_prefix(&home) {
        let suffix = suffix
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        format!("~/{suffix}")
    } else {
        path.display().to_string()
    }
}

fn byte_index_for_char(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(idx, _)| idx)
        .unwrap_or_else(|| s.len())
}

#[cfg(test)]
mod tui_tests {
    use super::*;

    #[test]
    fn first_tab_on_multiple_matches_selects_first_candidate() {
        let mut app = App::new();
        app.input = "/".into();
        app.cursor_pos = app.input.len();

        app.next_completion();

        assert!(app.completion_active);
        assert_eq!(app.completion_selected, 0);
        assert_eq!(app.completion_candidates[0].0, "/plan");
    }

    #[test]
    fn tab_extends_to_shared_prefix_before_cycling() {
        let mut app = App::new();
        app.input = "/rese".into();
        app.cursor_pos = app.input.len();

        app.next_completion();

        assert_eq!(app.input, "/reset");
        assert!(app.completion_active);
        assert_eq!(
            app.completion_candidates
                .iter()
                .map(|(cmd, _)| *cmd)
                .collect::<Vec<_>>(),
            vec!["/reset-spec", "/reset"]
        );
    }

    #[test]
    fn abbreviate_home_replaces_home_prefix() {
        let home = dirs::home_dir().expect("test environment should have a home directory");
        let path = home.join("Repos").join("ferrus");
        assert_eq!(abbreviate_home(&path), "~/Repos/ferrus");
    }

    #[test]
    fn typing_slash_command_updates_context_without_tab() {
        let mut app = App::new();

        app.insert_char('/');
        app.insert_char('s');

        assert!(app.has_command_context());
        assert_eq!(
            app.completion_candidates
                .iter()
                .map(|(cmd, _)| *cmd)
                .collect::<Vec<_>>(),
            vec!["/spec", "/supervisor", "/status", "/stop"]
        );
        assert!(!app.completion_active);
    }

    #[test]
    fn autocomplete_includes_new_hq_commands_and_omits_execute() {
        let commands: Vec<&str> = COMMANDS.iter().map(|(cmd, _)| *cmd).collect();

        assert!(commands.contains(&"/task"));
        assert!(commands.contains(&"/spec"));
        assert!(commands.contains(&"/check"));
        assert!(commands.contains(&"/tasks"));
        assert!(commands.contains(&"/run"));
        assert!(commands.contains(&"/runs"));
        assert!(commands.contains(&"/events"));
        assert!(commands.contains(&"/model"));
        assert!(commands.contains(&"/resume"));
        assert!(commands.contains(&"/reset-spec"));
        assert!(commands.contains(&"/supervisor"));
        assert!(commands.contains(&"/executor"));
        assert!(!commands.contains(&"/execute"));
    }

    #[test]
    fn render_prompt_wraps_multiline_input() {
        let mut app = App::new();
        app.input = "abcd\nef".into();
        app.cursor_pos = app.input.chars().count();

        let prompt = render_prompt(&app, 6);

        assert_eq!(prompt.lines, vec!["abcd", "ef"]);
        assert_eq!(prompt.cursor_row, 1);
        assert_eq!(prompt.cursor_col, 4);
    }

    #[test]
    fn render_prompt_preserves_trailing_newline() {
        let mut app = App::new();
        app.input = "abcd\n".into();
        app.cursor_pos = app.input.chars().count();

        let prompt = render_prompt(&app, 10);

        assert_eq!(prompt.lines, vec!["abcd", ""]);
        assert_eq!(prompt.cursor_row, 1);
        assert_eq!(prompt.cursor_col, 2);
    }

    #[test]
    fn dashboard_prompt_uses_command_completion_context() {
        let mut app = App::new();
        app.input = "/".into();
        app.cursor_pos = app.input.len();
        app.next_completion();

        let lines = prompt_accessory_lines(&app, 80)
            .into_iter()
            .flat_map(|line| line.segments.into_iter().map(|segment| segment.text))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(lines.contains("Commands"));
    }

    #[test]
    fn header_places_version_box_under_logo() {
        let mut app = App::new();
        app.startup = Some(StartupHeader {
            version: "v0.3.0-alpha.1".into(),
            supervisor_type: "claude-code".into(),
            supervisor_version: "2.1.143 (Claude Code)".into(),
            executor_type: "codex".into(),
            executor_version: "codex-cli 0.132.0".into(),
        });

        let rendered = header_lines(&app, 120)
            .into_iter()
            .map(|line| {
                line.line
                    .segments
                    .into_iter()
                    .map(|segment| segment.text)
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(rendered[0], "");
        assert!(rendered[1].starts_with("  ███████"));
        assert_eq!(rendered[6], "");
        assert!(rendered[7].starts_with("  ╭"));
        assert!(rendered[8].contains("version:"));
        assert!(rendered[9].contains("supervisor:"));
    }

    #[test]
    fn dashboard_omits_separator_between_tip_and_project_frame() {
        let app = App::new();
        let rendered = dashboard_lines(&app, 120, 40)
            .into_iter()
            .map(|line| {
                line.line
                    .segments
                    .into_iter()
                    .map(|segment| segment.text)
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        let tip_idx = rendered
            .iter()
            .position(|line| line.contains("Tip:"))
            .expect("tip line should be rendered");
        assert!(rendered[tip_idx].starts_with(" Tip:"));
        let next_non_empty = rendered[tip_idx + 1..]
            .iter()
            .find(|line| !line.is_empty())
            .expect("project frame should follow tip");

        assert!(next_non_empty.starts_with("  ╭"));
    }

    #[test]
    fn footer_debug_indicator_is_right_aligned() {
        let mut app = App::new();
        app.debug = true;

        let line = footer_line(&app, 60);
        let segments = line.segments;
        let rendered = segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect::<String>();
        let debug = segments.last().expect("debug segment should be present");

        assert_eq!(debug.text, "debug");
        assert_eq!(debug.color, Color::DarkBlue);
        assert_eq!(display_width(&rendered), 60);
        assert!(rendered.ends_with("debug"));
    }

    #[test]
    fn footer_line_shows_pending_and_ready_milestones_before_done() {
        let mut app = App::new();
        app.status.selected_milestones = vec![
            MilestoneSnapshot {
                marker: "#1.0".into(),
                title: "Pending".into(),
                completed: false,
                readiness: MilestoneReadiness::Pending,
            },
            MilestoneSnapshot {
                marker: "#1.1".into(),
                title: "Ready".into(),
                completed: false,
                readiness: MilestoneReadiness::Ready,
            },
        ];

        let rendered = footer_line(&app, 120)
            .segments
            .into_iter()
            .map(|segment| segment.text)
            .collect::<String>();

        assert!(rendered.contains("0 waiting  •  1 pending  •  1 ready  •  0 done"));
    }

    #[test]
    fn project_milestone_frame_uses_header_inset() {
        let app = App::new();
        let width = 120;
        let rendered = project_and_milestone_lines(&app, 120)
            .into_iter()
            .map(|row| {
                row.line
                    .segments
                    .into_iter()
                    .map(|segment| segment.text)
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert!(rendered.iter().all(|line| line.starts_with("  ")));
        assert!(rendered[0].starts_with("  ╭"));
        assert!(rendered.iter().all(|line| display_width(line) == width - 2));
    }

    #[test]
    fn project_milestone_frame_rows_keep_terminal_width() {
        let mut app = App::new();
        app.status.directory = "~/Repos/ferrus".into();
        app.status.branch = Some("feature/multi-task".into());
        app.status.selected_milestones = vec![
            MilestoneSnapshot {
                marker: "#1.0".into(),
                title: "Define dashboard layout".into(),
                completed: true,
                readiness: MilestoneReadiness::Done,
            },
            MilestoneSnapshot {
                marker: "#1.1".into(),
                title: "Wire runtime activity".into(),
                completed: false,
                readiness: MilestoneReadiness::Ready,
            },
            MilestoneSnapshot {
                marker: "#2.0".into(),
                title: "Wait for previous milestone".into(),
                completed: false,
                readiness: MilestoneReadiness::Pending,
            },
        ];
        let width = 120;

        let rows = project_and_milestone_lines(&app, width);
        let rendered = rows
            .iter()
            .map(|row| {
                row.line
                    .segments
                    .first()
                    .map(|segment| segment.text.as_str())
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>();

        assert!(rows.len() >= 4);
        for text in &rendered {
            assert_eq!(display_width(text), width - 2);
        }
        assert_eq!(rendered[1].find("Project"), rendered[6].find("tasks:"));
        assert!(rendered[1].contains("│ Project"));
        assert!(rendered[1].contains("│ Milestones"));
        let done_col = rendered[2].find("done").unwrap();
        let ready_col = rendered[3].find("ready").unwrap();
        let pending_col = rendered[4].find("pending").unwrap();
        assert_eq!(done_col, ready_col);
        assert_eq!(done_col, pending_col);
        assert_eq!(char_before_last_border(rendered[2]), Some(' '));
        assert_eq!(char_before_last_border(rendered[3]), Some(' '));
        assert_eq!(char_before_last_border(rendered[4]), Some(' '));
    }

    #[test]
    fn task_counts_line_shows_queued_runtime_tasks() {
        let mut app = App::new();
        app.runtime_tasks = vec![TaskRecord {
            id: "t-001".into(),
            path: ".ferrus/tasks/t-001.md".into(),
            spec_path: None,
            milestone_id: None,
            status: TaskStatus::Pending.as_str().into(),
            paused_status: None,
            claimed_by: None,
            lease_until: None,
            last_heartbeat: None,
            check_retries: 0,
            review_cycles: 0,
            failure_reason: None,
        }];

        assert_eq!(
            task_counts_line(&app),
            "tasks:       0 running  0 waiting  1 queued  0 done"
        );
    }

    #[test]
    fn command_output_renders_in_unframed_activity_area() {
        let mut app = App::new();
        app.messages.push(TranscriptLine {
            text: "status output".into(),
            kind: TranscriptKind::Info,
            continuation: false,
        });

        let rendered = dashboard_lines(&app, 120, 40)
            .into_iter()
            .map(|line| {
                line.line
                    .segments
                    .into_iter()
                    .map(|segment| segment.text)
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        let activity = rendered
            .iter()
            .find(|line| line.contains("status output"))
            .expect("command output should be visible");
        assert_eq!(activity, "  status output");
        let activity_idx = rendered
            .iter()
            .position(|line| line.contains("status output"))
            .unwrap();
        assert_eq!(rendered[activity_idx - 1], "");
    }

    #[test]
    fn success_activity_gets_leading_dot() {
        let line = TranscriptLine {
            text: "Task t-001 completed.".into(),
            kind: TranscriptKind::Success,
            continuation: false,
        };

        assert_eq!(activity_text(&line), "• Task t-001 completed.");
    }

    #[test]
    fn log_activity_path_is_clickable_file_link() {
        let line = transcript_activity_line(
            &TranscriptLine {
                text: "  ╰─ Logs: .ferrus/logs/executor.log".into(),
                kind: TranscriptKind::Muted,
                continuation: true,
            },
            120,
        );

        let link = line
            .segments
            .iter()
            .find_map(|segment| segment.link.as_deref())
            .expect("log path should be linked");
        assert!(link.starts_with("file://"));
        assert!(link.ends_with("/.ferrus/logs/executor.log"));
    }

    #[test]
    fn error_panel_keeps_interactive_stderr_detail() {
        let rendered = error_lines(
            "supervisor agent (codex) exited with exit status: 1\n\nstderr:\nError: Invalid MCP configuration",
            120,
        )
        .into_iter()
        .map(|line| {
            line.line
                .segments
                .into_iter()
                .map(|segment| segment.text)
                .collect::<String>()
        })
        .collect::<Vec<_>>();

        assert!(
            rendered
                .iter()
                .any(|line| line.contains("Error: Invalid MCP configuration")),
            "rendered error did not include stderr detail: {rendered:?}"
        );
    }

    #[test]
    fn runtime_activity_shows_tasks_before_recent_runs() {
        let mut app = App::new();
        app.runtime_tasks = vec![
            TaskRecord {
                id: "t-005".into(),
                path: ".ferrus/tasks/t-005.md".into(),
                spec_path: Some("docs/specs/example.md".into()),
                milestone_id: Some("m1.0".into()),
                status: "pending".into(),
                paused_status: None,
                claimed_by: None,
                lease_until: None,
                last_heartbeat: None,
                check_retries: 0,
                review_cycles: 0,
                failure_reason: None,
            },
            TaskRecord {
                id: "t-006".into(),
                path: ".ferrus/tasks/t-006.md".into(),
                spec_path: None,
                milestone_id: None,
                status: "reviewing".into(),
                paused_status: None,
                claimed_by: Some("supervisor:codex:t-006".into()),
                lease_until: None,
                last_heartbeat: None,
                check_retries: 0,
                review_cycles: 0,
                failure_reason: None,
            },
        ];
        app.runtime_runs = vec![RunRecord {
            id: "run-1".into(),
            task_id: "t-006".into(),
            role: "supervisor".into(),
            agent: "supervisor:codex:t-006".into(),
            status: "running".into(),
            started_at: "2026-05-24T10:00:00Z".into(),
            updated_at: "2026-05-24T10:00:01Z".into(),
            pid: Some(123),
            workspace_path: "/tmp/work".into(),
        }];

        let rendered = activity_area_lines(&app, 120, 10)
            .into_iter()
            .map(|line| {
                line.line
                    .segments
                    .into_iter()
                    .map(|segment| segment.text)
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        let pending_idx = rendered
            .iter()
            .position(|line| line.contains("t-005") && line.contains("pending"))
            .unwrap();
        let reviewing_idx = rendered
            .iter()
            .position(|line| line.contains("t-006") && line.contains("reviewing"))
            .unwrap();
        let run_idx = rendered
            .iter()
            .position(|line| line.contains("supervisor") && line.contains("running"))
            .unwrap();

        assert!(pending_idx < run_idx);
        assert!(reviewing_idx < run_idx);
        assert!(rendered[pending_idx].contains("m1.0"));
        assert!(rendered[reviewing_idx].contains("supervisor:codex:t-006"));
    }

    #[test]
    fn command_outputs_are_spaced_without_splitting_continuations() {
        let mut app = App::new();
        app.messages.extend([
            TranscriptLine {
                text: "first command".into(),
                kind: TranscriptKind::Info,
                continuation: false,
            },
            TranscriptLine {
                text: "first detail".into(),
                kind: TranscriptKind::Info,
                continuation: true,
            },
            TranscriptLine {
                text: "second command".into(),
                kind: TranscriptKind::Info,
                continuation: false,
            },
        ]);

        let rendered = activity_area_lines(&app, 120, 20)
            .into_iter()
            .map(|line| {
                line.line
                    .segments
                    .into_iter()
                    .map(|segment| segment.text)
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        let first_idx = rendered
            .iter()
            .position(|line| line.contains("first command"))
            .unwrap();
        let detail_idx = rendered
            .iter()
            .position(|line| line.contains("first detail"))
            .unwrap();
        let second_idx = rendered
            .iter()
            .position(|line| line.contains("second command"))
            .unwrap();

        assert_eq!(detail_idx, first_idx + 1);
        assert_eq!(rendered[first_idx - 1], "");
        assert_eq!(rendered[second_idx - 1], "");
    }

    #[test]
    fn activity_area_keeps_latest_multiline_message_start_when_tight() {
        let mut app = App::new();
        app.messages.extend(split_transcript(
            "  • Started supervisor:codex:t-006…\n  ╰─ Logs: .ferrus/logs/supervisor.log",
            TranscriptKind::Muted,
        ));

        let rendered = activity_area_lines(&app, 120, 1)
            .into_iter()
            .map(|line| {
                line.line
                    .segments
                    .into_iter()
                    .map(|segment| segment.text)
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(rendered.len(), 1);
        assert!(rendered[0].contains("Started supervisor:codex:t-006"));
    }

    #[test]
    fn activity_area_does_not_spend_only_line_on_gap() {
        let mut app = App::new();
        app.messages.extend(split_transcript(
            "Task t-006 completed.",
            TranscriptKind::Success,
        ));

        let rendered = activity_area_lines(&app, 120, 1)
            .into_iter()
            .map(|line| {
                line.line
                    .segments
                    .into_iter()
                    .map(|segment| segment.text)
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(rendered, vec!["  • Task t-006 completed."]);
    }

    fn char_before_last_border(text: &str) -> Option<char> {
        let mut before = None;
        let mut before_last_border = None;
        for ch in text.chars() {
            if ch == '│' {
                before_last_border = before;
            }
            before = Some(ch);
        }
        before_last_border
    }

    #[test]
    fn elapsed_only_status_update_does_not_change_dashboard() {
        let previous = StatusSnapshot {
            task_state: "Executing".into(),
            task_state_detail: "Executing (1s)".into(),
            ..StatusSnapshot::default()
        };
        let next = StatusSnapshot {
            task_state: "Executing".into(),
            task_state_detail: "Executing (2s)".into(),
            ..StatusSnapshot::default()
        };

        assert!(!status_dashboard_changed(&previous, &next));
    }

    #[test]
    fn multiline_submission_does_not_enter_history() {
        let mut app = App::new();
        let original_history_len = app.history.len();
        app.input = "first\nsecond".into();
        app.cursor_pos = app.input.chars().count();
        app.question_task_id = Some("t-002".to_string());
        app.answering_question_task_id = Some("t-002".to_string());
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();

        app.submit_input(&cmd_tx);

        assert_eq!(
            cmd_rx.try_recv().unwrap(),
            HqInput {
                text: "first\nsecond".to_string(),
                human_question_task_id: Some("t-002".to_string()),
            }
        );
        assert_eq!(app.history.len(), original_history_len);
    }

    #[test]
    fn human_answer_keeps_the_question_target_from_typing_start() {
        let mut app = App::new();
        app.question_task_id = Some("t-001".to_string());
        app.insert_text("first line\nsecond line");
        app.question_task_id = Some("t-002".to_string());
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();

        app.submit_input(&cmd_tx);

        assert_eq!(
            cmd_rx.try_recv().unwrap(),
            HqInput {
                text: "first line\nsecond line".to_string(),
                human_question_task_id: Some("t-001".to_string()),
            }
        );
    }

    #[test]
    fn pasted_text_preserves_multiline_prompt_newline() {
        let mut app = App::new();
        app.input = "first".into();
        app.cursor_pos = app.input.chars().count();

        app.insert_text("\r\nsecond");

        let prompt = dashboard_prompt(&app, 80);
        let rendered = prompt
            .lines
            .iter()
            .map(|line| {
                line.segments
                    .iter()
                    .map(|segment| segment.text.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(app.input, "first\nsecond");
        assert_eq!(rendered, vec!["> first", "  second"]);
        assert_eq!(prompt.cursor_row, 1);
    }

    #[test]
    fn up_moves_within_multiline_before_history() {
        let mut app = App::new();
        app.history = vec!["/status".into()];
        app.input = "one\ntwo\nthree".into();
        app.cursor_pos = app.input.chars().count();

        app.move_up_or_history();
        assert_eq!(
            app.cursor_pos,
            "one\n".chars().count() + "two".chars().count()
        );
        assert_eq!(app.input, "one\ntwo\nthree");
        assert_eq!(app.history_idx, None);

        app.move_up_or_history();
        assert_eq!(app.cursor_pos, "one".chars().count());
        assert_eq!(app.input, "one\ntwo\nthree");
        assert_eq!(app.history_idx, None);

        app.move_up_or_history();
        assert_eq!(app.input, "/status");
        assert_eq!(app.history_idx, Some(0));
    }

    #[test]
    fn down_moves_within_multiline_before_history() {
        let mut app = App::new();
        app.input = "one\ntwo\nthree".into();
        app.cursor_pos = 0;

        app.move_down_or_history();
        assert_eq!(app.cursor_pos, "one\n".chars().count());
        assert_eq!(app.input, "one\ntwo\nthree");
        assert_eq!(app.history_idx, None);

        app.move_down_or_history();
        assert_eq!(app.cursor_pos, "one\ntwo\n".chars().count());
        assert_eq!(app.input, "one\ntwo\nthree");
        assert_eq!(app.history_idx, None);

        app.move_down_or_history();
        assert_eq!(app.cursor_pos, "one\ntwo\n".chars().count());
        assert_eq!(app.input, "one\ntwo\nthree");
        assert_eq!(app.history_idx, None);

        app.move_down_or_history();
        assert_eq!(app.cursor_pos, "one\ntwo\n".chars().count());
        assert_eq!(app.input, "one\ntwo\nthree");
        assert_eq!(app.history_idx, None);
    }

    #[test]
    fn up_uses_history_immediately_for_single_line_input() {
        let mut app = App::new();
        app.history = vec!["/status".into()];
        app.input = "draft".into();
        app.cursor_pos = app.input.chars().count();

        app.move_up_or_history();

        assert_eq!(app.input, "/status");
        assert_eq!(app.history_idx, Some(0));
    }

    #[test]
    fn multiline_enter_accepts_shift_and_alt_enter() {
        assert!(is_multiline_enter(KeyModifiers::SHIFT));
        assert!(is_multiline_enter(KeyModifiers::ALT));
        assert!(is_multiline_enter(KeyModifiers::SHIFT | KeyModifiers::ALT));
        assert!(!is_multiline_enter(KeyModifiers::NONE));
        assert!(!is_multiline_enter(KeyModifiers::CONTROL));
        assert!(!is_multiline_enter(
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        ));
        assert!(!is_multiline_enter(
            KeyModifiers::CONTROL | KeyModifiers::ALT
        ));
        assert!(!is_multiline_enter(KeyModifiers::SUPER | KeyModifiers::ALT));
    }
}
