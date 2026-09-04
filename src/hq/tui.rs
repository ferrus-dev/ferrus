//! Terminal event loop, input state, and message handling for the HQ dashboard.

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
    ("/archive-spec", "archive selected spec artifacts"),
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
    Table(Vec<String>),
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
    TableTop,
    TableHeader,
    TableRow,
    TableBottom,
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

mod app;

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
        UiMessage::Table(rows) => {
            app.messages.extend(table_transcript(&rows));
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

mod render;
use render::*;

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
mod live_area;
use live_area::*;

mod history;
use history::*;

#[cfg(test)]
#[path = "tui_tests.rs"]
mod tui_tests;
