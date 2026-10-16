//! Terminal lifecycle shared by both frontends, including error-path restoration.

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{DisableBracketedPaste, EnableBracketedPaste},
    queue,
    style::ResetColor,
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::io::{self, Write};

pub(crate) struct Guard {
    active: bool,
}
impl Guard {
    pub(crate) fn enter() -> io::Result<Self> {
        if let Err(error) = enter() {
            let _ = leave();
            return Err(error);
        }
        Ok(Self { active: true })
    }
    pub(crate) fn finish(mut self) -> io::Result<()> {
        self.active = false;
        leave()
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        if self.active {
            let _ = leave();
        }
    }
}
pub(crate) fn enter() -> io::Result<()> {
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    queue!(stdout, EnterAlternateScreen)?;
    crate::platform::enter_tui(&mut stdout);
    queue!(
        stdout,
        EnableBracketedPaste,
        Clear(ClearType::All),
        MoveTo(0, 0),
        Hide
    )?;
    stdout.flush()
}
pub(crate) fn leave() -> io::Result<()> {
    let mut stdout = io::stdout();
    crate::platform::leave_tui(&mut stdout);
    // Always restore cooked input, even if the output stream has failed.
    let output = restore_display(&mut stdout);
    let raw = terminal::disable_raw_mode();
    output.and(raw)
}
fn restore_display(writer: &mut impl Write) -> io::Result<()> {
    queue!(
        writer,
        ResetColor,
        Show,
        DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cleanup_restores_cursor_paste_and_screen() {
        let mut bytes = Vec::new();
        restore_display(&mut bytes).unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "\x1b[0m\x1b[?25h\x1b[?2004l\x1b[?1049l"
        );
    }
}
