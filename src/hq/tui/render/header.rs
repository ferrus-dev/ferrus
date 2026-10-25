//! Dashboard header and milestone widgets.

use super::*;

pub(in super::super) fn header_lines(app: &App, width: usize) -> Vec<DashboardLine> {
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

pub(in super::super) fn tip_line(width: usize) -> StyledLine {
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

pub(in super::super) fn version_box_lines(app: &App) -> Vec<String> {
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

pub(in super::super) fn agent_version_line(
    label: &str,
    agent_type: &str,
    agent_details: &str,
) -> String {
    if agent_details.is_empty() {
        format!("{label} {agent_type}")
    } else {
        format!("{label} {agent_type} {agent_details}")
    }
}

pub(in super::super) fn ferrus_logo_lines() -> &'static [&'static str] {
    &[
        "███████  ███████  █████   █████   ██   ██  ███████",
        "██       ██       ██  ██  ██  ██  ██   ██  ██",
        "█████    █████    █████   █████   ██   ██  ███████",
        "██       ██       ██  ██  ██  ██  ██   ██       ██",
        "██       ███████  ██  ██  ██  ██   █████   ███████",
    ]
}

pub(in super::super) fn separator_line(width: usize) -> StyledLine {
    StyledLine::plain(
        crate::shared::style::separator(width.max(1)),
        Color::DarkGrey,
    )
}

pub(in super::super) fn project_and_milestone_lines(app: &App, width: usize) -> Vec<DashboardLine> {
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

pub(in super::super) fn frame_cell(text: &str) -> String {
    format!(" {text}")
}

pub(in super::super) fn milestone_lines(app: &App, width: usize) -> Vec<String> {
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

pub(in super::super) fn milestone_marker_label(marker: &str) -> String {
    marker
        .strip_prefix('#')
        .map(|marker| format!("M{marker}"))
        .unwrap_or_else(|| marker.to_string())
}
