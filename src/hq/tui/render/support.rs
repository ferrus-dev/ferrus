use super::*;

pub(in crate::hq::tui) fn enter_tui() -> Result<()> {
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

pub(in crate::hq::tui) fn leave_tui() -> Result<()> {
    let mut stdout = io::stdout();
    platform::leave_tui(&mut stdout);
    queue!(stdout, Show, DisableBracketedPaste, LeaveAlternateScreen)?;
    let _ = stdout.flush();
    disable_raw_mode()?;
    Ok(())
}

pub(in crate::hq::tui) fn flush_stdin_input_buffer() {
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

pub(in crate::hq::tui) fn render_prompt_with_prefix(
    app: &App,
    width: usize,
    prefix: &str,
) -> DashboardPrompt {
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

pub(in crate::hq::tui) fn terminal_size_usize() -> (usize, usize) {
    size()
        .map(|(w, h)| (w as usize, h as usize))
        .unwrap_or((100, 30))
}

pub(in crate::hq::tui) fn orange() -> Color {
    Color::Rgb {
        r: 226,
        g: 128,
        b: 18,
    }
}

pub(in crate::hq::tui) fn logo_gradient_color(idx: usize, len: usize) -> Color {
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

pub(in crate::hq::tui) fn section_title(title: &str) -> String {
    title.to_string()
}

pub(in crate::hq::tui) fn task_counts_line(app: &App) -> String {
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

pub(in crate::hq::tui) fn count_tasks(app: &App, statuses: &[TaskStatus]) -> usize {
    app.runtime_tasks
        .iter()
        .filter(|task| {
            task.status
                .parse::<TaskStatus>()
                .is_ok_and(|status| statuses.contains(&status))
        })
        .count()
}

pub(in crate::hq::tui) fn count_milestones(app: &App, readiness: MilestoneReadiness) -> usize {
    app.status
        .selected_milestones
        .iter()
        .filter(|milestone| milestone.readiness == readiness)
        .count()
}

pub(in crate::hq::tui) fn pad_or_truncate(text: &str, width: usize) -> String {
    let text = truncate_to_width(text, width);
    let padding = width.saturating_sub(display_width(&text));
    if padding == 0 {
        text
    } else {
        format!("{text}{}", " ".repeat(padding))
    }
}

pub(in crate::hq::tui) fn short_time(value: &str) -> String {
    value
        .split('T')
        .nth(1)
        .and_then(|time| time.get(..8))
        .unwrap_or(value)
        .to_string()
}

pub(in crate::hq::tui) fn activity_text(line: &TranscriptLine) -> String {
    match line.kind {
        TranscriptKind::Muted if !line.text.chars().next().is_some_and(char::is_whitespace) => {
            format!("  {}", line.text)
        }
        TranscriptKind::Success if !line.continuation => format!("• {}", line.text),
        TranscriptKind::Error if !line.continuation => format!("! {}", line.text),
        _ => line.text.clone(),
    }
}

pub(in crate::hq::tui) fn transcript_activity_line(
    line: &TranscriptLine,
    width: usize,
) -> StyledLine {
    let text = format!("  {}", activity_text(line));
    let color = transcript_color(line.kind);
    log_path_line(&text, color, width)
        .unwrap_or_else(|| StyledLine::plain(truncate_to_width(&text, width), color))
}

pub(in crate::hq::tui) fn log_path_line(
    text: &str,
    color: Color,
    width: usize,
) -> Option<StyledLine> {
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

pub(in crate::hq::tui) fn split_log_path(text: &str) -> Option<(&str, &str)> {
    ["Logs: ", "tail logs: ", "tail -f "]
        .iter()
        .filter_map(|marker| {
            text.rsplit_once(marker)
                .map(|(prefix, path)| (prefix, *marker, path))
        })
        .max_by_key(|(prefix, marker, _)| prefix.len() + marker.len())
        .map(|(prefix, marker, path)| (&text[..prefix.len() + marker.len()], path))
}

pub(in crate::hq::tui) fn file_url_for_path(path: &Path) -> String {
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

pub(in crate::hq::tui) fn percent_encode_uri_path(value: &str) -> String {
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

pub(in crate::hq::tui) fn transcript_color(kind: TranscriptKind) -> Color {
    match kind {
        TranscriptKind::Info => Color::Grey,
        TranscriptKind::Success => Color::Green,
        TranscriptKind::Tip => Color::Yellow,
        TranscriptKind::Muted => Color::DarkGrey,
        TranscriptKind::Error => Color::Red,
    }
}
