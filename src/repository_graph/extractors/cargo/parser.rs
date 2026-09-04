//! Parse Cargo manifests with bounded input and deadlines, including the parser worker protocol.

use super::*;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(super) enum ParserOutput {
    Parsed { manifest: toml::Table },
    Malformed { span: Option<ParserSpan> },
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct ParserSpan {
    start: usize,
    end: usize,
}

impl ParserSpan {
    pub(super) fn into_range(self) -> Range<usize> {
        self.start..self.end
    }
}

pub(super) enum ParserDeadline {
    Completed(ParserOutput),
    TimedOut,
    Unavailable,
}

pub(super) enum ChildDeadline {
    Exited(ExitStatus),
    TimedOut,
    Unavailable,
}

pub(super) fn parse_manifest(source: &str) -> ParserOutput {
    match toml::from_str::<toml::Table>(source) {
        Ok(manifest) => ParserOutput::Parsed { manifest },
        Err(error) => ParserOutput::Malformed {
            span: error.span().map(|span| ParserSpan {
                start: span.start,
                end: span.end,
            }),
        },
    }
}

/// Runs the isolated Cargo parser protocol before the public CLI is initialized.
///
/// This is an internal entry point used only by parser subprocesses spawned by
/// [`CargoExtractor`]. It is public so the `ferrus` binary can dispatch into
/// the library without exposing a user-facing CLI command.
#[doc(hidden)]
pub fn run_parser_worker_if_requested() -> io::Result<bool> {
    if std::env::args_os().nth(1).as_deref() != Some(OsStr::new(PARSER_WORKER_ARGUMENT)) {
        return Ok(false);
    }

    let mut source = String::new();
    io::stdin().read_to_string(&mut source)?;
    let output = parse_manifest(&source);
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer(&mut stdout, &output).map_err(io::Error::other)?;
    stdout.flush()?;
    Ok(true)
}

pub(super) fn run_parser_with_deadline(
    started: Instant,
    budget: Duration,
    source: String,
) -> ParserDeadline {
    if budget.saturating_sub(started.elapsed()).is_zero() {
        return ParserDeadline::TimedOut;
    }

    if cfg!(test) {
        let output = parse_manifest(&source);
        return if started.elapsed() >= budget {
            ParserDeadline::TimedOut
        } else {
            ParserDeadline::Completed(output)
        };
    }

    run_parser_process(started, budget, source)
}

pub(super) fn run_parser_in_process_with_deadline(
    started: Instant,
    budget: Duration,
    source: &str,
) -> ParserDeadline {
    if budget.saturating_sub(started.elapsed()).is_zero() {
        return ParserDeadline::TimedOut;
    }
    let output = parse_manifest(source);
    if started.elapsed() >= budget {
        ParserDeadline::TimedOut
    } else {
        ParserDeadline::Completed(output)
    }
}

pub(super) fn run_parser_process(
    started: Instant,
    budget: Duration,
    source: String,
) -> ParserDeadline {
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(_) => return ParserDeadline::Unavailable,
    };
    let mut child = match Command::new(executable)
        .arg(PARSER_WORKER_ARGUMENT)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return ParserDeadline::Unavailable,
    };
    let Some(mut stdin) = child.stdin.take() else {
        terminate_and_reap(&mut child);
        return ParserDeadline::Unavailable;
    };
    let Some(mut stdout) = child.stdout.take() else {
        terminate_and_reap(&mut child);
        return ParserDeadline::Unavailable;
    };
    let writer = match thread::Builder::new()
        .name("ferrus-cargo-parser-input".to_string())
        .spawn(move || stdin.write_all(source.as_bytes()))
    {
        Ok(writer) => writer,
        Err(_) => {
            terminate_and_reap(&mut child);
            return ParserDeadline::Unavailable;
        }
    };
    let reader = match thread::Builder::new()
        .name("ferrus-cargo-parser-output".to_string())
        .spawn(move || {
            let mut output = Vec::new();
            stdout.read_to_end(&mut output)?;
            Ok(output)
        }) {
        Ok(reader) => reader,
        Err(_) => {
            terminate_and_reap(&mut child);
            let _ = writer.join();
            return ParserDeadline::Unavailable;
        }
    };

    let status = wait_for_child(&mut child, started, budget);
    let output = finish_parser_io(writer, reader);
    match status {
        ChildDeadline::TimedOut => ParserDeadline::TimedOut,
        ChildDeadline::Unavailable => ParserDeadline::Unavailable,
        ChildDeadline::Exited(status) => {
            if !status.success() || started.elapsed() >= budget {
                return if started.elapsed() >= budget {
                    ParserDeadline::TimedOut
                } else {
                    ParserDeadline::Unavailable
                };
            }
            let Some(output) = output else {
                return ParserDeadline::Unavailable;
            };
            match serde_json::from_slice(&output) {
                Ok(parsed) if started.elapsed() < budget => ParserDeadline::Completed(parsed),
                Ok(_) => ParserDeadline::TimedOut,
                Err(_) => ParserDeadline::Unavailable,
            }
        }
    }
}

pub(super) fn wait_for_child(
    child: &mut Child,
    started: Instant,
    budget: Duration,
) -> ChildDeadline {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return ChildDeadline::Exited(status),
            Ok(None) => {}
            Err(_) => {
                terminate_and_reap(child);
                return ChildDeadline::Unavailable;
            }
        }
        let remaining = budget.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            terminate_and_reap(child);
            return ChildDeadline::TimedOut;
        }
        thread::sleep(remaining.min(PARSER_WAIT_POLL_INTERVAL));
    }
}

pub(super) fn terminate_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

pub(super) fn finish_parser_io(
    writer: JoinHandle<io::Result<()>>,
    reader: JoinHandle<io::Result<Vec<u8>>>,
) -> Option<Vec<u8>> {
    writer.join().ok()?.ok()?;
    reader.join().ok()?.ok()
}
