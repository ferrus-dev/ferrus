//! Render transient status, selection, and completion rows below the transcript.

use super::*;

pub(super) fn print_status_line(
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
        let hint = "  <- type your answer and press Enter";
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
        let hint = "  <- consulting supervisor";
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
pub(super) fn print_live_area_border(stdout: &mut Stdout, width: usize) -> Result<()> {
    let border_width = width.max(1);
    queue!(
        stdout,
        PrintStyledContent(style("─".repeat(border_width)).with(Color::DarkGrey))
    )?;
    Ok(())
}

#[allow(dead_code)]
pub(super) enum LiveAreaLine {
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
pub(super) fn render_lower_live_area(app: &App, width: usize) -> Vec<LiveAreaLine> {
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

pub(super) fn visible_selection_rows(selection: &SelectionState) -> Vec<(bool, &String)> {
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

pub(super) fn visible_completion_rows(app: &App) -> Vec<(bool, &'static str, &'static str)> {
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
pub(super) fn print_live_area_line(
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
pub(super) fn print_selection_line(
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
pub(super) fn print_completion_line(
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
pub(super) fn task_state_color(task_state: &str) -> Color {
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

pub(super) fn split_transcript(text: &str, kind: TranscriptKind) -> Vec<TranscriptLine> {
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

pub(super) fn table_transcript(rows: &[String]) -> Vec<TranscriptLine> {
    if rows.is_empty() {
        return Vec::new();
    }

    let mut lines = Vec::with_capacity(rows.len() + 2);
    lines.push(TranscriptLine {
        text: String::new(),
        kind: TranscriptKind::TableTop,
        continuation: false,
    });
    lines.extend(rows.iter().enumerate().map(|(idx, row)| TranscriptLine {
        text: sanitize_table_row(row),
        kind: if idx == 0 {
            TranscriptKind::TableHeader
        } else {
            TranscriptKind::TableRow
        },
        continuation: true,
    }));
    lines.push(TranscriptLine {
        text: String::new(),
        kind: TranscriptKind::TableBottom,
        continuation: true,
    });
    lines
}

fn sanitize_table_row(row: &str) -> String {
    row.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

#[allow(dead_code)]
pub(super) fn terminal_width() -> u16 {
    size().map(|(w, _)| w).unwrap_or(80)
}

pub(super) fn truncate_to_width(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

pub(super) fn display_width(text: &str) -> usize {
    text.chars().count()
}

pub(super) fn is_multiline_enter(modifiers: KeyModifiers) -> bool {
    let multiline = KeyModifiers::SHIFT | KeyModifiers::ALT;
    let disallowed =
        KeyModifiers::CONTROL | KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META;
    modifiers.intersects(multiline) && !modifiers.intersects(disallowed)
}
