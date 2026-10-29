//! Compose and redraw the HQ dashboard, activity feed, and prompt area.

use super::*;

#[derive(Clone)]
pub(super) struct StyledSegment {
    pub(super) text: String,
    pub(super) color: Color,
    pub(super) bold: bool,
    pub(super) link: Option<String>,
}

#[derive(Clone, Default)]
pub(super) struct StyledLine {
    pub(super) segments: Vec<StyledSegment>,
}

impl StyledLine {
    pub(super) fn plain(text: impl Into<String>, color: Color) -> Self {
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
pub(super) enum LineStyle {
    Normal,
    Logo,
    MetaBox,
    FramedBlock,
}

pub(super) struct DashboardLine {
    pub(super) line: StyledLine,
    style: LineStyle,
}

impl DashboardLine {
    pub(super) fn new(line: StyledLine) -> Self {
        Self {
            line,
            style: LineStyle::Normal,
        }
    }

    fn styled(line: StyledLine, style: LineStyle) -> Self {
        Self { line, style }
    }
}

pub(super) fn redraw_dashboard(stdout: &mut Stdout, app: &App, ui: &mut TerminalUi) -> Result<()> {
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

pub(super) fn redraw_prompt_area(
    stdout: &mut Stdout,
    app: &App,
    ui: &mut TerminalUi,
) -> Result<()> {
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

pub(super) fn dashboard_lines(app: &App, width: usize, max_lines: usize) -> Vec<DashboardLine> {
    if let Some(view) = &app.nano {
        let mut panels = if app.question.is_some() {
            question_lines(app, width)
        } else {
            Vec::new()
        };
        if let Some(error) = &app.last_error {
            panels.extend(error_lines(error, width));
        }

        panels.truncate(max_lines);
        let notice_rows = max_lines.saturating_sub(panels.len() + 3).min(36) / 3;
        for block in recent_transcript_blocks(&view.notices, notice_rows) {
            for line in block {
                panels.push(DashboardLine::new(transcript_activity_line(line, width)));
            }
        }

        let mut lines = nano::lines(view, width, max_lines.saturating_sub(panels.len()));
        lines.extend(panels);
        lines.truncate(max_lines);
        return lines;
    }

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

pub(super) fn activity_area_lines(app: &App, width: usize, max_lines: usize) -> Vec<DashboardLine> {
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
                lines.push(DashboardLine::new(transcript_activity_line(line, width)));
                if lines.len() >= max_lines {
                    break;
                }
            }
        }
    }

    lines.truncate(max_lines);
    lines
}

pub(super) fn recent_transcript_blocks(
    messages: &[TranscriptLine],
    max_lines: usize,
) -> Vec<Vec<&TranscriptLine>> {
    if max_lines == 0 || messages.is_empty() {
        return Vec::new();
    }

    let mut selected = Vec::new();
    let mut remaining = max_lines;
    let mut end = messages.len();
    while end > 0 {
        let gap_before_newer_block = usize::from(!selected.is_empty());
        if remaining <= gap_before_newer_block {
            break;
        }

        let start = messages[..end]
            .iter()
            .rposition(|line| !line.continuation)
            .unwrap_or(0);
        let block = &messages[start..end];
        let take = block.len().min(remaining - gap_before_newer_block);
        selected.push(take_transcript_block(block, take));
        remaining -= gap_before_newer_block + take;
        end = start;
    }

    selected.reverse();
    selected
}

pub(super) fn take_transcript_block(block: &[TranscriptLine], take: usize) -> Vec<&TranscriptLine> {
    if block.len() <= take {
        return block.iter().collect();
    }

    let is_table = matches!(
        block.first().map(|line| line.kind),
        Some(TranscriptKind::TableTop)
    ) && matches!(
        block.last().map(|line| line.kind),
        Some(TranscriptKind::TableBottom)
    );
    if is_table
        && take >= 2
        && let Some(bottom) = block.last()
    {
        let mut visible = block.iter().take(take - 1).collect::<Vec<_>>();
        visible.push(bottom);
        return visible;
    }

    block.iter().take(take).collect()
}

pub(super) fn runtime_task_activity_lines(
    app: &App,
    width: usize,
    max_lines: usize,
) -> Vec<DashboardLine> {
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

pub(super) fn runtime_task_activity_text(task: &TaskRecord) -> String {
    let detail = task
        .claimed_by
        .as_deref()
        .or(task.milestone_id.as_deref())
        .or(task.spec_path.as_deref())
        .unwrap_or(task.path.as_str());
    format!("  {:<7} {:<14} {}", task.id, task.status, detail)
}

pub(super) fn dashboard_task_status(status: &str) -> bool {
    matches!(
        status,
        "pending" | "executing" | "addressing" | "reviewing" | "consultation" | "awaiting_human"
    )
}

pub(super) fn task_activity_color(status: &str) -> Color {
    match status {
        "pending" => Color::Yellow,
        "awaiting_human" => Color::Magenta,
        "reviewing" | "consultation" => Color::Cyan,
        "executing" | "addressing" => Color::Green,
        _ => Color::Grey,
    }
}

pub(super) fn push_activity_gap(lines: &mut Vec<DashboardLine>) {
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

pub(super) fn question_lines(app: &App, width: usize) -> Vec<DashboardLine> {
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

pub(super) fn error_lines(error: &str, width: usize) -> Vec<DashboardLine> {
    let mut lines = vec![DashboardLine::new(StyledLine::bold("  Error", Color::Red))];
    for line in error.lines().filter(|line| !line.trim().is_empty()).take(8) {
        lines.push(DashboardLine::new(StyledLine::plain(
            truncate_to_width(&format!("  {line}"), width),
            Color::Red,
        )));
    }

    lines
}

pub(super) fn selection_dashboard_lines(app: &App, width: usize) -> Vec<StyledLine> {
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

pub(super) fn completion_dashboard_lines(app: &App, width: usize) -> Vec<StyledLine> {
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

pub(super) struct DashboardPrompt {
    pub(super) lines: Vec<StyledLine>,
    pub(super) cursor_row: u16,
    cursor_col: u16,
}

pub(super) fn dashboard_prompt(app: &App, width: usize) -> DashboardPrompt {
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

pub(super) fn prompt_accessory_lines(app: &App, width: usize) -> Vec<StyledLine> {
    if app.selection.is_some() {
        selection_dashboard_lines(app, width)
    } else if app.completion_popup_visible() {
        completion_dashboard_lines(app, width)
    } else if let Some(view) = &app.nano {
        vec![StyledLine::plain(
            truncate_to_width(
                &format!(
                    "{} · {}",
                    crate::shared::conversation::EDITING_HINT,
                    crate::shared::conversation::navigation_hint(view)
                ),
                width,
            ),
            Color::DarkGrey,
        )]
    } else {
        Vec::new()
    }
}

pub(super) fn footer_line(app: &App, width: usize) -> StyledLine {
    if app.ctrl_c_pending {
        return footer_with_debug(
            "Press Ctrl+C again within 2s to exit",
            Color::Yellow,
            true,
            app.debug,
            width,
        );
    }

    if app.nano.is_some() {
        return footer_with_debug(
            "/cancel: stop session · /detach: return to HQ",
            Color::DarkGrey,
            false,
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

pub(super) fn footer_with_debug(
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

pub(super) fn print_styled_line(
    stdout: &mut Stdout,
    line: &StyledLine,
    width: usize,
) -> Result<()> {
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

pub(super) fn print_dashboard_line(
    stdout: &mut Stdout,
    line: &DashboardLine,
    width: usize,
) -> Result<()> {
    match line.style {
        LineStyle::Logo => print_logo_dashboard_line(stdout, &line.line, width),
        LineStyle::MetaBox => print_meta_dashboard_line(stdout, &line.line, width),
        LineStyle::FramedBlock => print_framed_dashboard_line(stdout, &line.line, width),
        LineStyle::Normal => print_styled_line(stdout, &line.line, width),
    }
}

pub(super) fn print_logo_dashboard_line(
    stdout: &mut Stdout,
    line: &StyledLine,
    width: usize,
) -> Result<()> {
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

pub(super) fn print_meta_dashboard_line(
    stdout: &mut Stdout,
    line: &StyledLine,
    width: usize,
) -> Result<()> {
    let mut rendered = String::new();
    for segment in &line.segments {
        rendered.push_str(&segment.text);
    }

    let visible = truncate_to_width(&rendered, width);
    print_meta_text(stdout, &visible)?;
    Ok(())
}

pub(super) fn print_meta_text(stdout: &mut Stdout, text: &str) -> Result<()> {
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

pub(super) fn print_version_box_body(stdout: &mut Stdout, body: &str) -> Result<()> {
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

pub(super) fn meta_border_color(ch: char) -> Color {
    match ch {
        '╭' | '╮' | '╰' | '╯' | '─' | '│' => Color::DarkGrey,
        _ => Color::Grey,
    }
}

pub(super) fn print_framed_dashboard_line(
    stdout: &mut Stdout,
    line: &StyledLine,
    width: usize,
) -> Result<()> {
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

mod support;
pub(super) use support::*;
pub(super) use support::{enter_tui, flush_stdin_input_buffer, leave_tui};

mod header;
pub(super) use header::*;
