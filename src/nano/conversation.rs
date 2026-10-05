//! Bounded conversation projection and command transport for the HQ frontend.
//! The journal and managed task remain authoritative; previews never enter replay.

use super::{
    private,
    replay::Replay,
    session::{Budget, Record, SessionEvent},
    wire::{self, CommandKind},
};
use anyhow::{Result, ensure};
use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

const DISPLAY_BYTES: usize = 32 * 1024;
const ENTRY_BYTES: usize = 2048;
const ENTRIES: usize = 128;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub run_id: String,
    pub task_id: String,
    pub sequence: u64,
    pub entries: VecDeque<String>,
    pub status: String,
    pub budget: Budget,
    pub ended: bool,
    pub turn: u64,
}

/// Strip terminal control sequences as text, and bound bytes without splitting UTF-8.
pub(crate) fn display_text(text: &str, cap: usize) -> String {
    const TRUNCATED: &str = "\n[display truncated; full content is in the journal]";
    let truncated = text.len() > cap;
    let content_cap = if truncated {
        cap.saturating_sub(TRUNCATED.len())
    } else {
        cap
    };
    let mut result = String::new();
    for ch in text.chars() {
        let ch = if ch == '\t' {
            ' '
        } else if ch.is_control() && ch != '\n' {
            '?'
        } else {
            ch
        };
        if result.len() + ch.len_utf8() > content_cap {
            break;
        }
        result.push(ch);
    }
    if truncated {
        result.push_str(&TRUNCATED[..TRUNCATED.len().min(cap.saturating_sub(result.len()))]);
    }

    result
}

impl Snapshot {
    fn push(&mut self, label: &str, text: &str) {
        self.entries
            .push_back(format!("{label}: {}", display_text(text, ENTRY_BYTES)));
        while self.entries.len() > ENTRIES
            || self.entries.iter().map(String::len).sum::<usize>() > DISPLAY_BYTES
        {
            self.entries.pop_front();
        }
    }
    fn apply(&mut self, record: &Record) {
        self.sequence = record.sequence;
        self.budget = record.budget.clone();
        match &record.event {
            SessionEvent::Started {
                identity, input, ..
            } => {
                self.run_id = identity.session_id.clone();
                self.task_id = identity.task_id.clone().unwrap_or_default();
                self.status = "Starting".into();
                let initial = serde_json::Deserializer::from_str(input)
                    .into_iter::<serde_json::Value>()
                    .next()
                    .and_then(Result::ok);
                let task = initial
                    .as_ref()
                    .and_then(|value| value["documents"].as_array())
                    .and_then(|documents| {
                        documents
                            .iter()
                            .find(|document| document["kind"] == "task")
                            .and_then(|document| document["text"].as_str())
                    })
                    .unwrap_or(input);
                self.push("Task", task);
            }
            SessionEvent::UserInput { text } => self.push("You", text),
            SessionEvent::InputRequested => self.status = "Waiting for your input".into(),
            SessionEvent::ModelStarted { turn } => {
                self.turn = *turn;
                self.status = format!("Generating response (turn {turn})");
            }
            SessionEvent::ModelCompleted { response, .. } => {
                if !response.text.is_empty() {
                    self.push("Nano", &response.text);
                }
                self.status = "Processing response".into();
            }
            SessionEvent::ModelFailed { error, .. } => self.push("Provider", &format!("{error:?}")),
            SessionEvent::ToolIntent { call, .. } => {
                self.status = format!("Tool: {}", display_text(&call.name, 128));
                self.push("Tool", &format!("{} {}", call.name, call.arguments));
            }
            SessionEvent::ToolResult { outcome, .. } => self.push(
                "Result",
                &serde_json::to_string(outcome).unwrap_or_default(),
            ),
            SessionEvent::Ended { reason } => {
                self.ended = true;
                self.status = format!("Ended: {reason:?}");
                self.push("Session", &self.status.clone());
            }
            _ => (),
        }
    }
}

pub(crate) struct Reader {
    path: PathBuf,
    expected_run: String,
    offset: u64,
    replay: Replay,
    snapshot: Snapshot,
}
impl Reader {
    pub(crate) fn new(path: PathBuf, run_id: String) -> Self {
        Self {
            path,
            expected_run: run_id,
            offset: 0,
            replay: Replay::default(),
            snapshot: Snapshot::default(),
        }
    }
    pub(crate) fn poll(&mut self) -> Result<Option<Snapshot>> {
        let mut file = match private::read_only_file(&self.path) {
            Ok(file) => file,
            Err(error) if !self.path.exists() => {
                let _ = error;
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let length = file.metadata()?.len();
        let quotas = super::journal::Quotas::default();
        ensure!(
            length >= self.offset && length <= quotas.journal_bytes,
            "Invalid conversation journal size"
        );
        file.seek(SeekFrom::Start(self.offset))?;
        let mut reader = BufReader::new(file);
        let before = self.offset;
        for _ in 0..64 {
            let mut line = Vec::new();
            reader
                .by_ref()
                .take(quotas.record_bytes as u64 + 1)
                .read_until(b'\n', &mut line)?;
            ensure!(
                line.len() <= quotas.record_bytes,
                "Conversation record exceeds its limit"
            );
            if line.last() != Some(&b'\n') {
                break;
            }
            let record: Record = serde_json::from_slice(&line)?;
            ensure!(
                record.session_id == self.expected_run,
                "Conversation session mismatch"
            );
            self.replay.apply(&record)?;
            self.snapshot.apply(&record);
            self.offset += line.len() as u64;
        }
        Ok((self.offset != before).then(|| self.snapshot.clone()))
    }
}

/// A single blocking pipe writer with bounded queued input. Rendering never owns pipe I/O.
pub(crate) struct InputWriter {
    sender: mpsc::SyncSender<CommandKind>,
    cancel: Arc<AtomicBool>,
}
impl InputWriter {
    pub(crate) fn new(mut writer: impl Write + Send + 'static) -> Self {
        let (sender, receiver) = mpsc::sync_channel(8);
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = cancel.clone();
        std::thread::spawn(move || {
            loop {
                if stop.load(Ordering::SeqCst) {
                    let _ = wire::write_command(&mut writer, CommandKind::Cancel);
                    break;
                }
                match receiver.recv_timeout(Duration::from_millis(25)) {
                    Ok(command) => {
                        if wire::write_command(&mut writer, command).is_err() {
                            break;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => (),
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        });
        Self { sender, cancel }
    }
    pub(crate) fn open(&self) -> Result<()> {
        self.sender
            .try_send(CommandKind::Interact)
            .map_err(|_| anyhow::anyhow!("Nano input queue is full or disconnected"))
    }
    pub(crate) fn steer(&self, text: String) -> Result<()> {
        ensure!(!text.trim().is_empty(), "Input cannot be empty");
        let command = CommandKind::Steer { text };
        wire::write_command(&mut Vec::new(), command.clone())?;
        self.sender
            .try_send(command)
            .map_err(|_| anyhow::anyhow!("Nano input queue is full or disconnected"))
    }
    pub(crate) fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}
impl Drop for InputWriter {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Coalesced display state can be dropped or reattached without stopping persistence.
pub(crate) struct View {
    pub name: String,
    pub run_id: String,
    pub snapshots: tokio::sync::watch::Receiver<Option<Snapshot>>,
    stop: Arc<AtomicBool>,
}
impl View {
    pub(crate) fn spawn(name: String, path: PathBuf, run: String) -> Self {
        let (sender, snapshots) = tokio::sync::watch::channel(None);
        let stop = Arc::new(AtomicBool::new(false));
        let cancelled = stop.clone();
        let run_id = run.clone();
        std::thread::spawn(move || {
            let mut reader = Reader::new(path, run);
            while !cancelled.load(Ordering::SeqCst) {
                match reader.poll() {
                    Ok(Some(snapshot)) => {
                        let ended = snapshot.ended;
                        sender.send_replace(Some(snapshot));
                        if ended {
                            break;
                        }
                    }
                    Ok(None) => (),
                    Err(error) => {
                        let mut snapshot = reader.snapshot.clone();
                        snapshot.ended = true;
                        snapshot.status = format!("Conversation unavailable: {error}");
                        sender.send_replace(Some(snapshot));
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        Self {
            name,
            run_id,
            snapshots,
            stop,
        }
    }
}
impl Drop for View {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nano::{
        journal::{FileJournal, Journal, Quotas},
        session::{Limits, SessionIdentity},
    };

    fn journal(root: &std::path::Path) -> FileJournal {
        let mut journal = FileJournal::create(root, "view-test", Quotas::default()).unwrap();
        journal
            .append(
                SessionEvent::Started {
                    identity: SessionIdentity {
                        session_id: "view-test".into(),
                        project_id: "project".into(),
                        task_id: Some("t-001".into()),
                        run_id: Some("view-test".into()),
                    },
                    limits: Limits::default(),
                    input: "Implement the active task".into(),
                    system_prompt: None,
                    launch_evidence: None,
                    inherited_budget: None,
                    provider: None,
                },
                &Budget::default(),
            )
            .unwrap();
        journal
            .append(SessionEvent::InteractionOpened, &Budget::default())
            .unwrap();
        journal
    }

    #[test]
    fn reconnect_projects_bounded_history_without_repairing_an_uncommitted_tail() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut journal = journal(&root);
        for _ in 0..150 {
            journal
                .append(
                    SessionEvent::UserInput {
                        text: format!("\x1b[2J{}", "x".repeat(3500)),
                    },
                    &Budget::default(),
                )
                .unwrap();
        }
        let path = root.join("nano/sessions/view-test/events.jsonl");
        let mut reader = Reader::new(path.clone(), "view-test".into());
        let mut snapshot = None;
        while let Some(next) = reader.poll().unwrap() {
            snapshot = Some(next)
        }
        let snapshot = snapshot.unwrap();
        assert_eq!(snapshot.task_id, "t-001");
        assert_eq!(snapshot.sequence, 152);
        assert!(snapshot.entries.len() <= ENTRIES);
        assert!(snapshot.entries.iter().map(String::len).sum::<usize>() <= DISPLAY_BYTES);
        assert!(!snapshot.entries.iter().any(|text| text.contains('\x1b')));
        assert!(
            snapshot
                .entries
                .back()
                .unwrap()
                .contains("display truncated")
        );
        journal
            .append(
                SessionEvent::Ended {
                    reason: super::super::session::EndReason::Cancelled,
                },
                &Budget::default(),
            )
            .unwrap();
        let complete = std::fs::read(&path).unwrap();
        let mut file = private::file(&path, false).unwrap();
        file.seek(SeekFrom::End(0)).unwrap();
        file.write_all(b"{\"interrupted\":").unwrap();
        assert!(reader.poll().unwrap().unwrap().ended);
        assert!(reader.poll().unwrap().is_none());
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            complete.len() as u64 + 15
        );
        let mut reattached = Reader::new(path, "view-test".into());
        let mut restored = None;
        while let Some(next) = reattached.poll().unwrap() {
            restored = Some(next)
        }
        assert_eq!(restored.unwrap(), reader.snapshot);
    }

    #[test]
    fn reader_rejects_foreign_sessions_and_complete_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let _journal = journal(&root);
        let path = root.join("nano/sessions/view-test/events.jsonl");
        assert!(Reader::new(path.clone(), "foreign".into()).poll().is_err());
        let mut file = private::file(&path, false).unwrap();
        file.seek(SeekFrom::End(0)).unwrap();
        file.write_all(b"invalid\n").unwrap();
        assert!(Reader::new(path, "view-test".into()).poll().is_err());
    }

    #[test]
    fn blocked_input_pipe_is_bounded_and_cancel_preempts_queued_steering() {
        struct SlowWriter {
            entered: Option<mpsc::Sender<()>>,
            release: mpsc::Receiver<()>,
            bytes: Arc<std::sync::Mutex<Vec<u8>>>,
        }
        impl Write for SlowWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if let Some(entered) = self.entered.take() {
                    entered.send(()).unwrap();
                    self.release.recv().unwrap();
                }
                self.bytes.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (entered, waiting) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let bytes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer = InputWriter::new(SlowWriter {
            entered: Some(entered),
            release: released,
            bytes: bytes.clone(),
        });
        writer.open().unwrap();
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        for _ in 0..8 {
            writer.steer("queued".into()).unwrap();
        }
        assert!(writer.steer("overflow".into()).is_err());
        assert!(writer.steer("x".repeat(wire::FRAME_BYTES)).is_err());
        writer.cancel();
        release.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let text = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
            if text.contains("cancel") {
                assert!(!text.contains("queued"));
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
