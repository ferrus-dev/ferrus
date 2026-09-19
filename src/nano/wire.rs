//! Bounded frontend protocol. Durable session records remain authoritative.

use super::{
    journal::Journal,
    session::{Budget, EndReason, Record, SessionEvent},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
use std::sync::{Arc, Condvar, Mutex};

pub(crate) const VERSION: u32 = 1;
pub(crate) const FRAME_BYTES: usize = 4096;

/// Own a pipe handle without holding Rust's global stdout lock during blocked I/O.
pub(crate) fn stdout_file() -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::fd::AsFd;
        Ok(std::fs::File::from(
            std::io::stdout().as_fd().try_clone_to_owned()?,
        ))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsHandle;
        Ok(std::fs::File::from(
            std::io::stdout().as_handle().try_clone_to_owned()?,
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CommandKind {
    Start,
    Cancel,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Command {
    pub version: u32,
    pub command: CommandKind,
}
impl Command {
    pub(crate) fn new(command: CommandKind) -> Self {
        Self {
            version: VERSION,
            command,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Event {
    Ready,
    Progress { sequence: u64, phase: Phase },
    Ended { reason: EndReason, durable: bool },
    Error { code: String },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    Started,
    Model,
    Tool,
    ToolFinished,
}

impl Event {
    pub(crate) fn summary(&self) -> String {
        match self {
            Self::Ready => "ready".into(),
            Self::Progress { phase, .. } => format!("{phase:?}"),
            Self::Ended { reason, durable } => {
                format!("session ended: {reason:?}, durable={durable}")
            }
            Self::Error { .. } => "session error; see diagnostics".into(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Frame {
    pub version: u32,
    pub event: Event,
}

pub(crate) fn read_line(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    std::io::Read::take(reader, FRAME_BYTES as u64 + 1).read_until(b'\n', &mut line)?;
    if line.is_empty() {
        return Ok(None);
    }
    ensure!(line.len() <= FRAME_BYTES, "Nano frame exceeds byte limit");
    ensure!(line.last() == Some(&b'\n'), "Incomplete nano frame");
    Ok(Some(line))
}
pub(crate) fn read_command(reader: &mut impl BufRead) -> Result<Option<CommandKind>> {
    let Some(line) = read_line(reader)? else {
        return Ok(None);
    };
    let frame: Command =
        serde_json::from_slice(&line).map_err(|_| anyhow::anyhow!("Malformed nano command"))?;
    ensure!(
        frame.version == VERSION,
        "Unsupported nano protocol version"
    );
    Ok(Some(frame.command))
}
pub(crate) fn read_event(reader: &mut impl BufRead) -> Result<Option<Event>> {
    let Some(line) = read_line(reader)? else {
        return Ok(None);
    };
    let frame: Frame =
        serde_json::from_slice(&line).map_err(|_| anyhow::anyhow!("Malformed nano event"))?;
    ensure!(
        frame.version == VERSION,
        "Unsupported nano protocol version"
    );
    Ok(Some(frame.event))
}
pub(crate) fn write_command(writer: &mut impl Write, command: CommandKind) -> Result<()> {
    writer.write_all(&super::journal::encode(
        &Command::new(command),
        FRAME_BYTES - 1,
    )?)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

#[derive(Default)]
struct Mailbox {
    latest: Option<Event>,
    closed: bool,
}

/// One coalesced pending event; a blocked UI never holds up a journal append or heartbeat.
#[derive(Clone)]
pub(crate) struct Output(Arc<(Mutex<Mailbox>, Condvar)>);
impl Output {
    pub(crate) fn spawn(
        writer: impl Write + Send + 'static,
    ) -> (Self, tokio::sync::oneshot::Receiver<()>) {
        let output = Self(Arc::new((Mutex::new(Mailbox::default()), Condvar::new())));
        let owned = output.clone();
        let (done, receiver) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let _ = owned.write(writer);
            let _ = done.send(());
        });
        (output, receiver)
    }
    pub(crate) fn publish(&self, event: Event) {
        let mut mailbox = self.0.0.lock().unwrap();
        if !mailbox.closed {
            mailbox.latest = Some(event);
            self.0.1.notify_one();
        }
    }
    pub(crate) fn close(&self) {
        self.0.0.lock().unwrap().closed = true;
        self.0.1.notify_one();
    }
    fn write(&self, mut writer: impl Write) -> Result<()> {
        loop {
            let event = {
                let mut mailbox = self.0.0.lock().unwrap();
                while mailbox.latest.is_none() && !mailbox.closed {
                    mailbox = self.0.1.wait(mailbox).unwrap();
                }
                match mailbox.latest.take() {
                    Some(event) => event,
                    None => return Ok(()),
                }
            };
            writer.write_all(&super::journal::encode(
                &Frame {
                    version: VERSION,
                    event,
                },
                FRAME_BYTES - 1,
            )?)?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }
    }
}

pub(crate) struct ObservedJournal<J> {
    pub journal: J,
    pub output: Output,
}
impl<J: Journal> Journal for ObservedJournal<J> {
    fn append(&mut self, event: SessionEvent, budget: &Budget) -> Result<Record> {
        let record = self.journal.append(event, budget)?;
        let phase = match record.event {
            SessionEvent::Started { .. } => Some(Phase::Started),
            SessionEvent::ModelStarted { .. } => Some(Phase::Model),
            SessionEvent::ToolIntent { .. } => Some(Phase::Tool),
            SessionEvent::ToolResult { .. } => Some(Phase::ToolFinished),
            _ => None,
        };
        if let Some(phase) = phase {
            self.output.publish(Event::Progress {
                sequence: record.sequence,
                phase,
            });
        }
        Ok(record)
    }
    fn checkpoint(&mut self) -> Result<()> {
        self.journal.checkpoint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn command_frames_are_bounded_versioned_and_strict() {
        let mut bytes = Vec::new();
        write_command(&mut bytes, CommandKind::Start).unwrap();
        assert_eq!(
            read_command(&mut Cursor::new(bytes)).unwrap(),
            Some(CommandKind::Start)
        );
        for invalid in [
            b"{\"version\":2,\"command\":\"start\"}\n".to_vec(),
            b"{\"version\":1,\"command\":\"start\",\"task\":\"foreign\"}\n".to_vec(),
            b"{\"version\":1,\"command\":\"start\"}".to_vec(),
            b"not json\n".to_vec(),
            vec![b' '; FRAME_BYTES + 1],
        ] {
            assert!(read_command(&mut Cursor::new(invalid)).is_err());
        }
        assert!(read_event(&mut Cursor::new(b"command output\n")).is_err());
        assert!(
            read_event(&mut Cursor::new(
                b"{\"version\":2,\"event\":{\"type\":\"ready\"}}\n"
            ))
            .is_err()
        );
    }

    #[tokio::test]
    async fn slow_output_coalesces_without_blocking_producers_and_keeps_terminal() {
        struct Blocked {
            entered: Option<std::sync::mpsc::Sender<()>>,
            release: std::sync::mpsc::Receiver<()>,
            bytes: Arc<Mutex<Vec<u8>>>,
        }
        impl Write for Blocked {
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
        let (entered, observed) = std::sync::mpsc::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let (output, done) = Output::spawn(Blocked {
            entered: Some(entered),
            release: blocked,
            bytes: bytes.clone(),
        });
        output.publish(Event::Ready);
        observed
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        for sequence in 0..10000 {
            output.publish(Event::Progress {
                sequence,
                phase: Phase::Model,
            });
        }
        let end = Event::Ended {
            reason: EndReason::Submitted,
            durable: true,
        };
        output.publish(end.clone());
        output.close();
        assert_eq!(output.0.0.lock().unwrap().latest, Some(end.clone()));
        release.send(()).unwrap();
        done.await.unwrap();
        let mut reader = Cursor::new(bytes.lock().unwrap().clone());
        assert_eq!(read_event(&mut reader).unwrap(), Some(Event::Ready));
        assert_eq!(read_event(&mut reader).unwrap(), Some(end));
        assert_eq!(read_event(&mut reader).unwrap(), None);
    }
}
