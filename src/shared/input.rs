//! Text editing and prompt geometry; no HQ commands or host policy.

pub(crate) struct Prompt {
    pub(crate) lines: Vec<String>,
    pub(crate) cursor_row: u16,
    pub(crate) cursor_col: u16,
}
pub(crate) fn prompt(input: &str, cursor_pos: usize, width: usize, prefix: &str) -> Prompt {
    let prefix_width = prefix.chars().count();
    let available = width.saturating_sub(prefix_width).max(1);
    let chars: Vec<char> = input.chars().collect();
    let mut raw_lines = Vec::new();
    let mut cursor_row = 0u16;
    let mut cursor_col = prefix_width as u16;
    let mut line = String::new();
    let mut line_width = 0usize;
    let mut row = 0u16;

    for (idx, ch) in chars.iter().enumerate() {
        if idx == cursor_pos {
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
            if idx == cursor_pos {
                cursor_row = row;
                cursor_col = prefix_width as u16;
            }
        }

        line.push(*ch);
        line_width += 1;
    }

    if cursor_pos == chars.len() {
        cursor_row = row;
        cursor_col = prefix_width as u16 + line_width as u16;
    }

    raw_lines.push(line);
    Prompt {
        lines: raw_lines,
        cursor_row,
        cursor_col,
    }
}

pub(crate) fn byte_index_for_char(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(idx, _)| idx)
        .unwrap_or(s.len())
}
pub(crate) fn insert(input: &mut String, cursor: &mut usize, ch: char) {
    input.insert(byte_index_for_char(input, *cursor), ch);
    *cursor += 1;
}
pub(crate) fn delete(input: &mut String, cursor: &mut usize, before: bool) {
    let at = if before {
        let Some(at) = cursor.checked_sub(1) else {
            return;
        };
        at
    } else {
        *cursor
    };
    let start = byte_index_for_char(input, at);
    let end = byte_index_for_char(input, at + 1);
    input.replace_range(start..end, "");
    if before {
        *cursor = at;
    }
}
pub(crate) fn multiline_enter(modifiers: crossterm::event::KeyModifiers) -> bool {
    use crossterm::event::KeyModifiers as M;
    modifiers.intersects(M::SHIFT | M::ALT)
        && !modifiers.intersects(M::CONTROL | M::SUPER | M::HYPER | M::META)
}
