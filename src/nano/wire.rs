//! Bounded frontend protocol. Durable session records remain authoritative.

use super::{
    journal::Journal,
    provider::ProviderErrorKind,
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
    Progress {
        sequence: u64,
        phase: Phase,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<ProgressDetail>,
    },
    Ended {
        reason: EndReason,
        durable: bool,
    },
    Error {
        code: String,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    Started,
    Model,
    ModelFailed,
    ModelTruncated,
    Tool,
    ToolFinished,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ProgressDetail {
    Model { turn: u64 },
    Tool { name: String },
    ModelFailed { error: Option<ProviderErrorKind> },
}

impl Event {
    pub(crate) fn summary(&self) -> String {
        match self {
            Self::Ready => "ready".into(),
            Self::Progress { phase, detail, .. } => match detail {
                Some(ProgressDetail::Model { turn }) => {
                    format!("generating response (turn {turn})")
                }
                Some(ProgressDetail::Tool { name }) => {
                    format!("tool request: {}", tool_label(name))
                }
                Some(ProgressDetail::ModelFailed { error: Some(error) }) => {
                    format!("model request failed: {error:?}")
                }
                _ => match phase {
                    Phase::Started => "session started",
                    Phase::Model => "generating response",
                    Phase::ModelFailed => "model request failed; see diagnostics",
                    Phase::ModelTruncated => "response reached output limit; continuing",
                    Phase::Tool => "tool request",
                    Phase::ToolFinished => "tool finished",
                }
                .into(),
            },
            Self::Ended { reason, durable } => {
                if *reason == EndReason::ProviderTruncated {
                    return format!(
                        "session ended: ProviderTruncated (model output limit reached), durable={durable}"
                    );
                }
                format!("session ended: {reason:?}, durable={durable}")
            }
            Self::Error { .. } => "session error; see diagnostics".into(),
        }
    }

    /// Keep internal model/finish markers in the scoped log, not the HQ transcript.
    pub(crate) fn show_in_hq(&self) -> bool {
        matches!(
            self,
            Self::Ended { .. }
                | Self::Error { .. }
                | Self::Progress {
                    phase: Phase::ModelFailed,
                    ..
                }
                | Self::Progress {
                    phase: Phase::ModelTruncated,
                    ..
                }
                | Self::Progress {
                    phase: Phase::Tool,
                    detail: Some(ProgressDetail::Tool { .. }),
                    ..
                }
        )
    }
}

fn tool_label(name: &str) -> &str {
    if !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
    {
        name
    } else {
        "[invalid tool name]"
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
        if let Some(event) = progress(&record) {
            self.output.publish(event);
        }
        Ok(record)
    }
    fn checkpoint(&mut self) -> Result<()> {
        self.journal.checkpoint()
    }
}

fn progress(record: &Record) -> Option<Event> {
    let (phase, detail) = match &record.event {
        SessionEvent::Started { .. } => (Phase::Started, None),
        SessionEvent::ModelStarted { turn } => {
            (Phase::Model, Some(ProgressDetail::Model { turn: *turn }))
        }
        SessionEvent::ModelFailed { error, .. } => (
            Phase::ModelFailed,
            Some(ProgressDetail::ModelFailed {
                error: error.clone(),
            }),
        ),
        SessionEvent::ModelCompleted { response, .. }
            if response.finish == super::provider::FinishReason::Length =>
        {
            (Phase::ModelTruncated, None)
        }
        SessionEvent::ToolIntent { call, .. } => (
            Phase::Tool,
            Some(ProgressDetail::Tool {
                name: tool_label(&call.name).into(),
            }),
        ),
        SessionEvent::ToolResult { .. } => (Phase::ToolFinished, None),
        _ => return None,
    };
    Some(Event::Progress {
        sequence: record.sequence,
        phase,
        detail,
    })
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

    #[test]
    fn progress_reports_safe_activity_without_flooding_hq_with_internal_markers() {
        let mut record = Record {
            version: 1,
            session_id: "run".into(),
            sequence: 7,
            budget: Budget::default(),
            event: SessionEvent::ModelStarted { turn: 3 },
        };
        let event = progress(&record).unwrap();
        assert_eq!(event.summary(), "generating response (turn 3)");
        assert!(!event.show_in_hq());
        for (name, label) in [
            ("read_file", "read_file"),
            ("bad\nname", "[invalid tool name]"),
        ] {
            record.event = SessionEvent::ToolIntent {
                call_id: "call-1".into(),
                call: crate::nano::tools::ToolCall {
                    provider_call_id: "provider-secret".into(),
                    name: name.into(),
                    arguments: "secret arguments and file content".into(),
                },
                effect_plan: None,
                start_recorded: true,
            };
            let event = progress(&record).unwrap();
            assert_eq!(event.summary(), format!("tool request: {label}"));
            assert!(event.show_in_hq());
            let bytes = super::super::journal::encode(
                &Frame {
                    version: VERSION,
                    event: event.clone(),
                },
                FRAME_BYTES - 1,
            )
            .unwrap();
            assert!(!String::from_utf8_lossy(&bytes).contains("secret"));
            let mut line = bytes;
            line.push(b'\n');
            assert_eq!(read_event(&mut Cursor::new(line)).unwrap(), Some(event));
        }
        record.event = SessionEvent::ModelFailed {
            retryable: true,
            usage: crate::nano::provider::Usage {
                input_tokens: 1,
                output_tokens: 1,
                reported: false,
            },
            error: Some(ProviderErrorKind::Timeout),
            diagnostic: None,
        };
        let event = progress(&record).unwrap();
        assert_eq!(event.summary(), "model request failed: Timeout");
        assert!(event.show_in_hq());
        record.event = SessionEvent::ModelCompleted {
            response: crate::nano::provider::ModelResponse {
                finish: crate::nano::provider::FinishReason::Length,
                text: "secret partial model response".into(),
                calls: Vec::new(),
                continuation: None,
            },
            usage: crate::nano::provider::Usage {
                input_tokens: 1,
                output_tokens: 1,
                reported: true,
            },
        };
        let event = progress(&record).unwrap();
        assert_eq!(event.summary(), "response reached output limit; continuing");
        assert!(event.show_in_hq());
        assert!(!serde_json::to_string(&event).unwrap().contains("secret"));
        let legacy = read_event(&mut Cursor::new(b"{\"version\":1,\"event\":{\"type\":\"progress\",\"sequence\":1,\"phase\":\"model\"}}\n")).unwrap().unwrap();
        assert_eq!(legacy.summary(), "generating response");
        assert!(!legacy.show_in_hq());
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
                detail: None,
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
