//! Bounded agent logs and prompt transport.

use super::*;

pub(super) fn spawn_native_log_reader(
    stdout: std::process::ChildStdout,
    logger: Arc<Mutex<SlimLogger>>,
    pid: u32,
) -> (
    std::thread::JoinHandle<()>,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::watch::Receiver<crate::nano::wire::ObservedEvents>,
) {
    use crate::nano::wire::{self, Event};
    let (ready_tx, ready) = tokio::sync::oneshot::channel();
    let (events_tx, events) = tokio::sync::watch::channel(wire::ObservedEvents::default());
    let thread = std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut ready_tx = Some(ready_tx);
        loop {
            match wire::read_event(&mut reader) {
                Ok(Some(Event::Ready)) if ready_tx.is_some() => {
                    if let Some(ready) = ready_tx.take() {
                        let _ = ready.send(());
                    }
                }
                Ok(Some(event)) if ready_tx.is_none() && !matches!(event, Event::Ready) => {
                    if !matches!(event, Event::Text { .. }) {
                        let _ = logger
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .log_event("Nano", event.summary());
                    }

                    // Fast tool results and the following model turn must not overwrite
                    // the useful activity before HQ's slower scheduler tick consumes it.
                    if event.show_in_hq() || matches!(event, Event::Text { .. }) {
                        events_tx.send_modify(|state| state.observe(event));
                    }
                }
                Ok(None) => break,
                _ => {
                    let _ = logger
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .log_event("Nano error", "invalid event stream");
                    platform::signal_process_group(pid, ShutdownSignal::Kill);
                    break;
                }
            }
        }
    });
    (thread, ready, events)
}

pub(super) fn headless_log_path(
    log_dir: &std::path::Path,
    role: &str,
    task_id: Option<&str>,
    run_id: &str,
    timestamp: &str,
) -> PathBuf {
    let scope = task_id.unwrap_or("session");
    log_dir.join(format!(
        "{}_{}_{}_{}.log",
        sanitize_log_component(role),
        sanitize_log_component(scope),
        sanitize_log_component(timestamp),
        sanitize_log_component(run_id),
    ))
}

pub(super) fn sanitize_log_component(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized
    }
}

pub(super) fn stream_prompt_to_stdin(child: &mut std::process::Child, prompt: &str) -> Result<()> {
    use std::io::Write as _;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("stdin pipe is unavailable"))?;
    stdin
        .write_all(prompt.as_bytes())
        .context("failed writing prompt to stdin")?;
    drop(stdin);
    Ok(())
}

pub(super) fn append_debug_agent_flags(agent_type: &str, command: &mut StdCommand) {
    match agent_type {
        "claude-code" => {
            command.arg("--verbose");
        }
        // `codex --help` and `codex exec --help` expose no verbose/debug flag.
        "codex" => {}
        _ => {}
    }
}

pub(super) fn format_command(command: &StdCommand) -> String {
    let program = command.get_program().to_string_lossy();
    let args = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    format!("command={program} args={args:?}")
}

pub(super) fn spawn_slim_log_reader<R>(
    reader: R,
    logger: Arc<Mutex<SlimLogger>>,
) -> std::thread::JoinHandle<()>
where
    R: std::io::Read + Send + 'static,
{
    std::thread::spawn(move || {
        let reader = BufReader::new(reader);
        for line in reader.lines() {
            let Ok(line) = line else {
                break;
            };
            if line.trim().is_empty() {
                continue;
            }

            let mut logger = logger
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _ = logger.handle_agent_output(&line);
        }
    })
}

pub(super) struct SlimLogger {
    pub(super) file: File,
    pub(super) pending_failed_tool: Option<String>,
}

impl SlimLogger {
    pub(super) fn new(file: File) -> Self {
        Self {
            file,
            pending_failed_tool: None,
        }
    }

    pub(super) fn handle_agent_output(&mut self, line: &str) -> std::io::Result<()> {
        if let Some((tool, status)) = parse_mcp_tool_status(line) {
            self.flush_pending_error();
            match status {
                ToolCallStatus::Started => {}
                ToolCallStatus::Completed => {
                    self.log_event("MCP tool call", format!("{tool} - ok"))?;
                }
                ToolCallStatus::Failed => {
                    self.pending_failed_tool = Some(tool);
                }
            }

            return Ok(());
        }

        if let Some(tool) = self.pending_failed_tool.take() {
            return self.log_event("MCP tool call", format!("{tool} - error: {line}"));
        }

        Ok(())
    }

    pub(super) fn log_initial_prompt(&mut self, prompt: &str) -> std::io::Result<()> {
        if prompt.trim().is_empty() {
            return self.log_event("Initial prompt", "(empty)");
        }

        for line in prompt.lines() {
            self.log_event("Initial prompt", line)?;
        }

        Ok(())
    }

    pub(super) fn flush_pending_error(&mut self) {
        if let Some(tool) = self.pending_failed_tool.take() {
            let _ = self.log_event("MCP tool call", format!("{tool} - error"));
        }
    }

    pub(super) fn log_event(&mut self, label: &str, value: impl AsRef<str>) -> std::io::Result<()> {
        writeln!(
            self.file,
            "{} {label}: {}",
            chrono::Utc::now().to_rfc3339(),
            value.as_ref()
        )?;
        self.file.flush()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ToolCallStatus {
    Started,
    Completed,
    Failed,
}

pub(super) fn parse_mcp_tool_status(line: &str) -> Option<(String, ToolCallStatus)> {
    let rest = line.strip_prefix("mcp: ")?;
    let (tool_path, status_text) = if let Some(tool_path) = rest.strip_suffix(" started") {
        (tool_path, ToolCallStatus::Started)
    } else if let Some(tool_path) = rest.strip_suffix(" (completed)") {
        (tool_path, ToolCallStatus::Completed)
    } else {
        let tool_path = rest.strip_suffix(" (failed)")?;
        (tool_path, ToolCallStatus::Failed)
    };

    let tool = tool_path.rsplit('/').next()?.to_string();
    Some((tool, status_text))
}
