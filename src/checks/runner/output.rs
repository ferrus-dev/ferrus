//! Temporary stream spools and byte/line bounded feedback. Full logs stay on disk.

use std::{
    collections::VecDeque,
    fs::{File, OpenOptions},
    io::{self, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::Result;

const MAX_FEEDBACK_BYTES: usize = 64 * 1024;
static SPOOL_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
pub struct CapturedOutput {
    pub tail: String,
    pub total_lines: usize,
    pub truncated: bool,
}

pub(super) struct Spool {
    file: Option<File>,
    path: PathBuf,
}

impl Spool {
    pub(super) fn new(log_path: &Path, stream: &str) -> io::Result<Self> {
        loop {
            let sequence = SPOOL_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path =
                log_path.with_extension(format!("{}-{sequence}.{stream}.tmp", std::process::id()));
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => {
                    return Ok(Self {
                        file: Some(file),
                        path,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }

    pub(super) fn file(&self) -> &File {
        self.file.as_ref().expect("spool is open")
    }
}

impl Drop for Spool {
    fn drop(&mut self) {
        self.file.take();
        let _ = std::fs::remove_file(&self.path);
    }
}

pub(super) fn finish_log(
    log_path: &Path,
    command: &str,
    passed: bool,
    stdout: Spool,
    stderr: Spool,
    max_lines: usize,
) -> Result<CapturedOutput> {
    let mut log = BufWriter::new(
        OpenOptions::new()
            .append(true)
            .create(true)
            .open(log_path)?,
    );
    writeln!(
        log,
        "=== [{}] {command}\n",
        if passed { "PASS" } else { "FAIL" }
    )?;
    let mut tail = OutputTail::new(max_lines);
    for (label, spool) in [("stdout", stdout), ("stderr", stderr)] {
        let mut file = spool.file();
        file.seek(SeekFrom::Start(0))?;
        if file.metadata()?.len() == 0 {
            continue;
        }
        writeln!(log, "--- {label} ---")?;
        let mut buffer = [0u8; 8192];
        let mut last = None;
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            log.write_all(&buffer[..read])?;
            tail.push(&buffer[..read]);
            last = buffer.get(read - 1).copied();
        }
        if last != Some(b'\n') {
            writeln!(log)?;
        }
    }
    writeln!(log)?;
    log.flush()?;
    Ok(tail.finish())
}

/// Retains only the requested final lines, with an independent cap for a single
/// unterminated line. Feed stdout then stderr to preserve existing report order.
struct OutputTail {
    bytes: VecDeque<u8>,
    max_lines: usize,
    newlines: usize,
    total_newlines: usize,
    last: Option<u8>,
    dropped: bool,
}

impl OutputTail {
    fn new(max_lines: usize) -> Self {
        Self {
            bytes: VecDeque::new(),
            max_lines,
            newlines: 0,
            total_newlines: 0,
            last: None,
            dropped: false,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.last = Some(byte);
            self.total_newlines = self
                .total_newlines
                .saturating_add(usize::from(byte == b'\n'));
            self.bytes.push_back(byte);
            self.newlines += usize::from(byte == b'\n');
            let lines = self.newlines + usize::from(byte != b'\n');
            if lines > self.max_lines {
                while let Some(removed) = self.bytes.pop_front() {
                    self.dropped = true;
                    if removed == b'\n' {
                        self.newlines -= 1;
                        break;
                    }
                }
            }
            while self.bytes.len() > MAX_FEEDBACK_BYTES {
                let removed = self.bytes.pop_front().expect("tail exceeds cap");
                self.newlines -= usize::from(removed == b'\n');
                self.dropped = true;
            }
        }
    }

    fn finish(mut self) -> CapturedOutput {
        let total_lines = self
            .total_newlines
            .saturating_add(usize::from(self.last.is_some_and(|byte| byte != b'\n')));
        // A byte cap can split the first UTF-8 codepoint. Keep the intact suffix;
        // invalid source bytes elsewhere retain the previous lossy-display policy.
        if self.dropped {
            while self.bytes.front().is_some_and(|byte| byte & 0xc0 == 0x80) {
                self.bytes.pop_front();
            }
        }
        let bytes: Vec<_> = self.bytes.into_iter().collect();
        let decoded = String::from_utf8_lossy(&bytes);
        // Replacement characters can expand invalid source bytes. Bound the
        // displayed UTF-8 string as well as the raw stream tail.
        let mut start = decoded.len().saturating_sub(MAX_FEEDBACK_BYTES);
        while !decoded.is_char_boundary(start) {
            start += 1;
        }
        self.dropped |= start > 0;
        let mut tail = String::with_capacity(decoded.len() - start);
        for (index, line) in decoded[start..].lines().enumerate() {
            if index > 0 {
                tail.push('\n');
            }
            tail.push_str(line);
        }
        CapturedOutput {
            tail,
            total_lines,
            truncated: self.dropped,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_matches_line_selection_across_chunk_and_stream_boundaries() {
        for input in ["", "one\ntwo\nthree\n", "one\r\ntwo\nlast", "\n\n", "a\nb"] {
            for limit in 0..5 {
                let mut tail = OutputTail::new(limit);
                for byte in input.as_bytes() {
                    tail.push(&[*byte]);
                }
                let output = tail.finish();
                let lines: Vec<_> = input.lines().collect();
                assert_eq!(
                    output.tail,
                    lines[lines.len().saturating_sub(limit)..].join("\n")
                );
                assert_eq!(output.total_lines, lines.len());
            }
        }
    }

    #[test]
    fn caps_unterminated_lines_and_preserves_split_unicode() {
        let mut tail = OutputTail::new(2);
        for _ in 0..100 {
            tail.push(&[b'x'; 8192]);
        }
        for byte in "\u{1f980}\nlast".as_bytes() {
            tail.push(&[*byte]);
        }
        assert!(tail.bytes.len() <= MAX_FEEDBACK_BYTES);
        let output = tail.finish();
        assert!(output.truncated);
        assert!(output.tail.ends_with("\u{1f980}\nlast"));
        assert_eq!(output.total_lines, 2);

        let mut invalid = OutputTail::new(2);
        invalid.push(&vec![0xff; MAX_FEEDBACK_BYTES]);
        let output = invalid.finish();
        assert!(output.tail.len() <= MAX_FEEDBACK_BYTES);
        assert!(output.truncated);
        assert!(output.tail.chars().all(|character| character == '\u{fffd}'));
    }
}
