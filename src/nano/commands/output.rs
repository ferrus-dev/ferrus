//! Private output spools. Pipes are drained concurrently under session and process quotas.

use super::*;
use std::{io::Write, sync::atomic::Ordering};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    process::{ChildStderr, ChildStdout},
};

pub(super) struct Prepared {
    pub stdout: File,
    pub stderr: File,
    pub writer: Writer,
}

pub(super) struct Writer {
    stdout: File,
    stderr: File,
    state: File,
}

pub(super) fn prepare(directory: &Path, state: &Snapshot) -> Result<Prepared> {
    // Keep read handles to the original objects. Model handles never reopen a path.
    let stdout_path = directory.join(&state.stdout.handle);
    let stderr_path = directory.join(&state.stderr.handle);
    let stdout = private::file(&stdout_path, true)?;
    let stderr = private::file(&stderr_path, true)?;
    let mut status = private::file(&directory.join(format!("{}.json", state.process_id)), true)?;

    status.write_all(&encode(state, STATE_BYTES as usize)?)?;
    status.sync_all()?;
    private::sync_directory(directory)?;

    Ok(Prepared {
        stdout: File::from_std(private::read_only_file(&stdout_path)?),
        stderr: File::from_std(private::read_only_file(&stderr_path)?),
        writer: Writer {
            stdout: File::from_std(stdout),
            stderr: File::from_std(stderr),
            state: File::from_std(status),
        },
    })
}

pub(super) fn reserve(charged: &AtomicU64, total: u64, bytes: u64) -> bool {
    charged
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
            used.checked_add(bytes).filter(|next| *next <= total)
        })
        .is_ok()
}

impl Writer {
    async fn capture(
        &mut self,
        stdout: bool,
        bytes: &[u8],
        snapshot: &mut Snapshot,
        limits: &Limits,
        charged: &AtomicU64,
    ) -> std::result::Result<(), Completion> {
        let process_remaining = limits
            .process_output_bytes
            .saturating_sub(snapshot.stdout.bytes + snapshot.stderr.bytes);

        let mut count = (bytes.len() as u64).min(process_remaining);
        // A sibling can consume the shared allowance between load and reservation.
        loop {
            count = count.min(
                limits
                    .total_bytes
                    .saturating_sub(charged.load(Ordering::Acquire)),
            );
            if reserve(charged, limits.total_bytes, count) {
                break;
            }
        }

        let (file, output) = if stdout {
            (&mut self.stdout, &mut snapshot.stdout)
        } else {
            (&mut self.stderr, &mut snapshot.stderr)
        };

        file.write_all(&bytes[..count as usize])
            .await
            .map_err(|_| Completion::Unknown)?;

        file.flush().await.map_err(|_| Completion::Unknown)?;
        output.bytes += count;

        if count < bytes.len() as u64 {
            Err(Completion::OutputLimit)
        } else {
            Ok(())
        }
    }

    async fn finish(&mut self, snapshot: &mut Snapshot) -> Result<()> {
        self.stdout.sync_all().await?;
        self.stderr.sync_all().await?;

        // A cancelled async write can have published a bounded partial chunk.
        // Reconcile the actual retained prefix before exposing its byte cursors.
        snapshot.stdout.bytes = self.stdout.metadata().await?.len();
        snapshot.stderr.bytes = self.stderr.metadata().await?.len();

        let bytes = encode(snapshot, STATE_BYTES as usize)?;

        self.state.rewind().await?;
        self.state.write_all(&bytes).await?;
        self.state.set_len(bytes.len() as u64).await?;
        self.state.flush().await?;
        self.state.sync_all().await?;

        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn supervise(
    mut process: Spawned,
    mut stdout: ChildStdout,
    mut stderr: ChildStderr,
    mut writer: Writer,
    status: watch::Sender<Snapshot>,
    stop: Cancellation,
    session: Cancellation,
    duration: Duration,
    limits: Limits,
    charged: Arc<AtomicU64>,
) {
    let mut snapshot = status.borrow().clone();
    let mut out = [0u8; 8192];
    let mut err = [0u8; 8192];
    let mut out_done = false;
    let mut err_done = false;
    let mut interrupted_capture = false;
    let mut poll = tokio::time::interval(Duration::from_millis(10));

    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let deadline = Instant::now() + duration;
    let reason = loop {
        let read = tokio::select! {
            biased;
            _ = stop.cancelled() => break Completion::Cancelled,
            _ = session.cancelled() => break Completion::Cancelled,
            _ = tokio::time::sleep_until(deadline) => break Completion::TimedOut,
            _ = poll.tick() => {
                match backend::exited(&mut process) {
                    Ok(true) => break Completion::Exited { code: None, success: false },
                    Ok(false) => (),
                    Err(_) => break Completion::Unknown,
                }
                continue;
            },
            read = async {
                tokio::select! {
                    read = stdout.read(&mut out), if !out_done => (true, read),
                    read = stderr.read(&mut err), if !err_done => (false, read),
                }
            }, if !(out_done && err_done) => read,
        };

        let (is_out, read) = read;
        let count = match read {
            Ok(n) => n,
            Err(_) => break Completion::Unknown,
        };

        if count == 0 {
            if is_out {
                out_done = true;
            } else {
                err_done = true;
            }
            continue;
        }

        let capture = writer.capture(
            is_out,
            if is_out { &out[..count] } else { &err[..count] },
            &mut snapshot,
            &limits,
            &charged,
        );

        let result = tokio::select! {
            biased;
            _ = stop.cancelled() => { interrupted_capture = true; Err(Completion::Cancelled) },
            _ = session.cancelled() => { interrupted_capture = true; Err(Completion::Cancelled) },
            _ = tokio::time::sleep_until(deadline) => { interrupted_capture = true; Err(Completion::TimedOut) },
            result = capture => result,
        };

        if let Err(reason) = result {
            break reason;
        }

        status.send_replace(snapshot.clone());
    };

    process.tree.lock().unwrap().stop();
    snapshot.completion = reason;
    let cleanup = async {
        let exit = process.child.wait().await?;
        if matches!(snapshot.completion, Completion::Exited { .. }) {
            snapshot.completion = Completion::Exited {
                code: exit.code(),
                success: exit.success(),
            };
        }
        // Read the bounded tail after terminating descendants that inherited pipes.
        // Once quota is exceeded, close the pipes rather than drain unlimited data.
        while !(out_done && err_done)
            && !interrupted_capture
            && !matches!(
                snapshot.completion,
                Completion::OutputLimit | Completion::Unknown
            )
        {
            let (is_out, read) = tokio::select! {
                read = stdout.read(&mut out), if !out_done => (true, read),
                read = stderr.read(&mut err), if !err_done => (false, read),
            };
            let count = read?;
            if count == 0 {
                if is_out {
                    out_done = true;
                } else {
                    err_done = true;
                }
                continue;
            }
            if let Err(reason) = writer
                .capture(
                    is_out,
                    if is_out { &out[..count] } else { &err[..count] },
                    &mut snapshot,
                    &limits,
                    &charged,
                )
                .await
            {
                snapshot.completion = reason;
                break;
            }
        }
        snapshot.output_complete = !interrupted_capture && out_done && err_done;
        writer.finish(&mut snapshot).await
    };
    if !matches!(
        tokio::time::timeout(Duration::from_millis(CLEANUP_MS), cleanup).await,
        Ok(Ok(()))
    ) {
        snapshot.completion = Completion::Unknown;
        snapshot.output_complete = false;
    }
    status.send_replace(snapshot);
}

pub(super) async fn page(
    file: &mut File,
    handle: &str,
    offset: u64,
    max_bytes: usize,
    available: u64,
    state: &Snapshot,
) -> std::result::Result<OutputPage, ToolError> {
    if offset > available {
        return Err(ToolError::InvalidArguments);
    }
    let count = (available - offset).min(max_bytes as u64) as usize;
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(|_| ToolError::Failed)?;
    let mut bytes = vec![0; count];
    file.read_exact(&mut bytes)
        .await
        .map_err(|_| ToolError::Failed)?;
    let next = offset + count as u64;
    Ok(OutputPage {
        handle: handle.into(),
        offset,
        next_offset: next,
        available_bytes: available,
        text: String::from_utf8_lossy(&bytes).into_owned(),
        complete: state.output_complete && next == available,
        truncated: next < available || !state.output_complete,
    })
}
