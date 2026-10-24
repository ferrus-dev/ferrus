//! Ferrus terminal chrome shared by HQ and Nano.

use crossterm::style::Color;

pub(crate) const ACCENT: Color = Color::Rgb {
    r: 226,
    g: 128,
    b: 18,
};

pub(crate) fn separator(width: usize) -> String {
    "─".repeat(width)
}
