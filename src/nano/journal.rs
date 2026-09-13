//! Durable single-writer JSONL storage, bounded artifacts, and atomic prefix checkpoints.

use super::{
    private,
    replay::{JOURNAL_VERSION, Replay},
    session::{Budget, EndReason, LimitKind, Record, SessionEvent},
};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone)]
pub(crate) struct Quotas {
    pub record_bytes: usize,
    pub journal_bytes: u64,
    pub artifact_bytes: usize,
    pub total_bytes: u64,
    pub files: usize,
}

impl Default for Quotas {
    fn default() -> Self {
        Self {
            record_bytes: 512 * 1024,
            journal_bytes: 16 * 1024 * 1024,
            artifact_bytes: 1024 * 1024,
            total_bytes: 32 * 1024 * 1024,
            files: 256,
        }
    }
}

pub(crate) trait Journal {
    fn append(&mut self, event: SessionEvent, budget: &Budget) -> Result<Record>;

    fn checkpoint(&mut self) -> Result<()>;
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Checkpoint {
    pub version: u32,
    pub session_id: String,
    pub sequence: u64,
    pub budget: Budget,
    pub prefix_sha256: String,
}

pub(crate) struct FileJournal {
    directory: PathBuf,
    file: File,
    quotas: Quotas,
    state: Replay,
    session_id: String,
    journal_bytes: u64,
    total_bytes: u64,
    files: usize,
    digest: Sha256,
    poisoned: bool,
    // Drop the journal file before releasing its writer lock.
    _lock: WriterLock,
}

struct WriterLock(File);

impl WriterLock {
    fn acquire(file: File) -> Result<Self> {
        file.try_lock_exclusive()
            .context("Nano journal already has a writer")?;
        Ok(Self(file))
    }
}

impl Drop for WriterLock {
    fn drop(&mut self) {
        // Closing alone leaves flock held by duplicates inherited across fork,
        // even with CLOEXEC, until the child reaches exec or closes its copy.
        let _ = FileExt::unlock(&self.0);
    }
}

pub(crate) fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 96
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
}

/// Bound serialization itself, not just the resulting allocation.
pub(crate) fn encode<T: Serialize>(value: &T, limit: usize) -> Result<Vec<u8>> {
    struct Bounded {
        bytes: Vec<u8>,
        limit: usize,
    }

    impl Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                return Err(std::io::Error::other(
                    "Serialized output exceeds byte limit",
                ));
            }

            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut output = Bounded {
        bytes: Vec::new(),
        limit,
    };

    serde_json::to_writer(&mut output, value)?;
    Ok(output.bytes)
}

impl FileJournal {
    pub(crate) fn create(project_data: &Path, session_id: &str, quotas: Quotas) -> Result<Self> {
        ensure!(valid_id(session_id), "Invalid nano session ID");
        ensure!(
            project_data.is_absolute() && project_data.is_dir(),
            "Registered project data must exist"
        );
        ensure!(
            quotas.record_bytes > 0
                && quotas.journal_bytes > 0
                && quotas.total_bytes > 0
                && quotas.artifact_bytes > 0
                && quotas.files >= 2,
            "Invalid journal quotas"
        );

        let root = project_data.canonicalize()?;
        let nano = root.join("nano");
        private::directory(&nano, false)?;

        let sessions = nano.join("sessions");
        private::directory(&sessions, false)?;

        let directory = sessions.join(session_id);
        private::directory(&directory, true)?;
        private::directory(&directory.join("outputs"), true)?;
        private::directory(&directory.join("checkpoints"), true)?;

        let lock = WriterLock::acquire(private::file(&directory.join("writer.lock"), true)?)?;

        let file = private::file(&directory.join("events.jsonl"), true)?;
        private::sync_directory(&directory)?;
        private::sync_directory(&sessions)?;
        private::sync_directory(&nano)?;
        private::sync_directory(&root)?;

        Ok(Self {
            directory,
            _lock: lock,
            file,
            quotas,
            state: Replay::default(),
            session_id: session_id.into(),
            journal_bytes: 0,
            total_bytes: 0,
            files: 2,
            digest: Sha256::new(),
            poisoned: false,
        })
    }

    /// Repair only an unterminated final line. Complete corrupt records fail closed.
    /// Charge the remaining elapsed budget and end any interrupted attempt durably.
    /// Opening a journal never executes or resumes recorded effects.
    pub(crate) fn recover(directory: &Path, quotas: Quotas) -> Result<(Self, Vec<Record>)> {
        private::check(directory, true)?;
        let session_id = directory
            .file_name()
            .and_then(|name| name.to_str())
            .context("Invalid session directory")?
            .to_string();

        ensure!(valid_id(&session_id), "Invalid nano session ID");
        let lock = WriterLock::acquire(private::file(&directory.join("writer.lock"), false)?)?;

        let (mut total_bytes, files) = measure(directory, &quotas)?;
        let mut file = private::file(&directory.join("events.jsonl"), false)?;
        let size = file.metadata()?.len();
        ensure!(size <= quotas.journal_bytes, "Journal exceeds quota");

        let max_line_bytes = quotas
            .record_bytes
            .checked_add(2)
            .context("Record quota overflow")? as u64;

        let mut reader = BufReader::new(file.try_clone()?);
        let mut records = Vec::new();
        let mut state = Replay::default();
        let mut digest = Sha256::new();
        let mut committed_bytes = 0;

        loop {
            let mut line = Vec::new();
            // read_until alone could allocate the whole file for a corrupt line.
            let count = reader
                .by_ref()
                .take(max_line_bytes)
                .read_until(b'\n', &mut line)?;

            if count == 0 {
                break;
            }

            ensure!(
                count <= quotas.record_bytes + 1,
                "Journal record exceeds quota"
            );

            if line.last() != Some(&b'\n') {
                break;
            }

            let record: Record = serde_json::from_slice(&line)?;
            ensure!(
                record.session_id == session_id,
                "Session directory/record mismatch"
            );

            state.apply(&record)?;
            digest.update(&line);
            committed_bytes += count as u64;
            records.push(record);
        }

        if committed_bytes < size {
            file.set_len(committed_bytes)?;
            file.sync_all()?;
            total_bytes -= size - committed_bytes;
        }

        file.seek(SeekFrom::End(0))?;

        let mut journal = Self {
            directory: directory.to_path_buf(),
            _lock: lock,
            file,
            quotas,
            state,
            session_id,
            journal_bytes: committed_bytes,
            total_bytes,
            files,
            digest,
            poisoned: false,
        };

        if journal.state.end.is_none()
            && let Some(Record {
                event: SessionEvent::Started { limits, .. },
                ..
            }) = records.first()
        {
            // The process-local clock cannot account for an uncommitted wait.
            // Exhaust the remaining allowance even at a complete checkpoint;
            // keep pending effects and token reservations for reconciliation.
            let mut budget = journal.state.budget.clone();
            budget.elapsed_ms = budget.elapsed_ms.max(limits.elapsed_ms);

            records.push(journal.append(
                SessionEvent::Ended {
                    reason: EndReason::Limit(LimitKind::Elapsed),
                },
                &budget,
            )?);
        }

        Ok((journal, records))
    }

    pub(crate) fn state(&self) -> &Replay {
        &self.state
    }

    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }

    pub(crate) fn artifact(&mut self, output_id: &str, bytes: &[u8]) -> Result<PathBuf> {
        ensure!(valid_id(output_id), "Invalid output ID");
        ensure!(
            bytes.len() <= self.quotas.artifact_bytes,
            "Artifact exceeds quota"
        );

        let path = self.directory.join("outputs").join(output_id);
        self.atomic_file(&path, bytes)?;

        Ok(path)
    }

    fn atomic_file(&mut self, path: &Path, bytes: &[u8]) -> Result<()> {
        ensure!(!self.poisoned, "Journal writer is poisoned");
        ensure!(
            self.files < self.quotas.files
                && bytes.len() as u64 <= self.quotas.total_bytes.saturating_sub(self.total_bytes),
            "Session storage quota exhausted"
        );
        ensure!(
            !path.try_exists()?,
            "Immutable session artifact already exists"
        );

        let temporary = path.with_extension("tmp");
        let result = (|| {
            let mut file = private::file(&temporary, true)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            private::rename(&temporary, path)?;
            private::sync_directory(path.parent().context("Missing artifact directory")?)?;
            Ok(())
        })();

        if result.is_err() {
            self.poisoned = true;
        } else {
            self.total_bytes += bytes.len() as u64;
            self.files += 1;
        }

        result
    }
}

impl Journal for FileJournal {
    fn append(&mut self, event: SessionEvent, budget: &Budget) -> Result<Record> {
        ensure!(!self.poisoned, "Journal writer is poisoned");
        let record = Record {
            version: JOURNAL_VERSION,
            session_id: self.session_id.clone(),
            sequence: self.state.sequence + 1,
            budget: budget.clone(),
            event,
        };

        let mut next = self.state.clone();
        next.apply(&record)?;

        let mut bytes = encode(&record, self.quotas.record_bytes)?;
        bytes.push(b'\n');

        ensure!(
            bytes.len() as u64 <= self.quotas.journal_bytes.saturating_sub(self.journal_bytes)
                && bytes.len() as u64 <= self.quotas.total_bytes.saturating_sub(self.total_bytes),
            "Journal quota exhausted"
        );

        if let Err(error) = self
            .file
            .write_all(&bytes)
            .and_then(|_| self.file.sync_all())
        {
            self.poisoned = true;
            return Err(error.into());
        }

        self.digest.update(&bytes);
        self.journal_bytes += bytes.len() as u64;
        self.total_bytes += bytes.len() as u64;
        self.state = next;

        Ok(record)
    }

    fn checkpoint(&mut self) -> Result<()> {
        ensure!(
            self.state.checkpoint_ready(),
            "Checkpoint would split a model/tool group"
        );

        let checkpoint = Checkpoint {
            version: JOURNAL_VERSION,
            session_id: self.session_id.clone(),
            sequence: self.state.sequence,
            budget: self.state.budget.clone(),
            prefix_sha256: hex_digest(self.digest.clone()),
        };

        let bytes = encode(&checkpoint, self.quotas.record_bytes)?;
        let path = self
            .directory
            .join("checkpoints")
            .join(format!("{}.json", self.state.sequence));

        self.atomic_file(&path, &bytes)
    }
}

pub(crate) fn verify_checkpoint(checkpoint: &Checkpoint, records: &[Record]) -> Result<Replay> {
    ensure!(
        checkpoint.version == JOURNAL_VERSION,
        "Unsupported checkpoint version"
    );

    let count = usize::try_from(checkpoint.sequence)?;
    ensure!(count <= records.len(), "Checkpoint exceeds journal prefix");

    let prefix = &records[..count];
    let state = Replay::from_records(prefix)?;
    ensure!(
        state.checkpoint_ready()
            && state.session_id == checkpoint.session_id
            && state.budget == checkpoint.budget,
        "Checkpoint does not match a complete journal group"
    );

    let mut digest = Sha256::new();
    for record in prefix {
        digest.update(serde_json::to_vec(record)?);
        digest.update(b"\n");
    }

    ensure!(
        hex_digest(digest) == checkpoint.prefix_sha256,
        "Checkpoint digest mismatch"
    );

    Ok(state)
}

fn measure(directory: &Path, quotas: &Quotas) -> Result<(u64, usize)> {
    let mut bytes = 0u64;
    let mut files = 0;

    for (dir, per_file_quota) in [
        (directory.to_path_buf(), None),
        (
            directory.join("outputs"),
            Some((quotas.artifact_bytes, "Artifact exceeds quota")),
        ),
        (
            directory.join("checkpoints"),
            Some((quotas.record_bytes, "Checkpoint exceeds quota")),
        ),
    ] {
        private::check(&dir, true)?;
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if dir == directory
                && matches!(
                    path.file_name().and_then(|n| n.to_str()),
                    Some("outputs" | "checkpoints")
                )
            {
                continue;
            }
            private::check(&path, false)?;

            let size = fs::metadata(path)?.len();
            if let Some((limit, message)) = per_file_quota {
                ensure!(size <= limit as u64, "{message}");
            }

            files += 1;
            bytes = bytes.saturating_add(size);

            ensure!(
                files <= quotas.files && bytes <= quotas.total_bytes,
                "Session storage exceeds quota"
            );
        }
    }

    Ok((bytes, files))
}

fn hex_digest(digest: Sha256) -> String {
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(all(test, unix))]
mod lock_tests {
    use super::*;

    #[test]
    fn dropping_a_writer_releases_the_lock_while_a_duplicate_remains_open() {
        let root = tempfile::TempDir::new().unwrap();
        let journal = FileJournal::create(root.path(), "s-1", Quotas::default()).unwrap();
        let directory = journal.directory().to_owned();
        // dup and fork share the same open file description. Keep the duplicate
        // alive to model a concurrent child between fork and close-on-exec.
        let duplicate = journal._lock.0.try_clone().unwrap();
        assert!(FileJournal::recover(&directory, Quotas::default()).is_err());
        drop(journal);

        let (recovered, _) = FileJournal::recover(&directory, Quotas::default()).unwrap();
        drop(duplicate);
        assert!(FileJournal::recover(&directory, Quotas::default()).is_err());
        drop(recovered);
        FileJournal::recover(&directory, Quotas::default()).unwrap();
    }
}
