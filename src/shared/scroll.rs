//! A conversation viewport follows output until the reader scrolls away.

use std::{cell::Cell, ops::Range};

#[derive(Default)]
pub(crate) struct Scroll {
    start: Option<usize>,
    maximum: Cell<usize>,
}

impl Scroll {
    pub(crate) fn range(&self, lines: usize, height: usize) -> Range<usize> {
        let maximum = lines.saturating_sub(height);
        self.maximum.set(maximum);
        let start = self.start.unwrap_or(maximum).min(maximum);
        start..start.saturating_add(height).min(lines)
    }

    pub(crate) fn up(&mut self, lines: usize) {
        self.start = Some(
            self.start
                .unwrap_or(self.maximum.get())
                .min(self.maximum.get())
                .saturating_sub(lines),
        );
    }

    pub(crate) fn down(&mut self, lines: usize) {
        let start = self
            .start
            .unwrap_or(self.maximum.get())
            .saturating_add(lines);
        self.start = (start < self.maximum.get()).then_some(start);
    }

    pub(crate) fn home(&mut self) {
        self.start = Some(0);
    }

    pub(crate) fn end(&mut self) {
        self.start = None;
    }

    pub(crate) fn following(&self) -> bool {
        self.start.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reading_stays_put_when_output_grows_and_end_resumes_following() {
        let mut scroll = Scroll::default();
        assert_eq!(scroll.range(100, 20), 80..100);
        scroll.up(10);
        assert_eq!(scroll.range(120, 20), 70..90);
        assert_eq!(scroll.range(140, 30), 70..100);
        scroll.home();
        assert_eq!(scroll.range(200, 20), 0..20);
        scroll.end();
        assert_eq!(scroll.range(200, 20), 180..200);
        assert_eq!(scroll.range(220, 20), 200..220);
    }

    #[test]
    fn scrolling_clamps_to_resized_content_and_bottom_rejoins_output() {
        let mut scroll = Scroll::default();
        scroll.range(200, 20);
        scroll.up(10);
        assert_eq!(scroll.range(30, 20), 10..30);
        scroll.up(3);
        assert_eq!(scroll.range(30, 20), 7..27);
        scroll.down(usize::MAX);
        assert!(scroll.following());
        assert_eq!(scroll.range(0, 0), 0..0);
    }
}
