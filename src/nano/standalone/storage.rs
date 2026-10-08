use super::super::{
    effect_recovery,
    journal::{self, FileJournal, Quotas},
    private,
    replay::Replay,
    session::*,
};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub(super) fn home() -> Result<PathBuf> {
    crate::user_paths::ferrus_home()
}

pub(super) struct Storage {
    pub path: PathBuf,
    pub workspace_id: String,
    lock: File,
    workspace_lock: File,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Binding {
    version: u32,
    workspace: PathBuf,
    workspace_id: String,
}

impl Storage {
    pub fn open(workspace: &Path, path: Option<PathBuf>) -> Result<Self> {
        let identity = super::super::workspace::directory_identity(workspace)?;
        let workspace_id: String = Sha256::digest(serde_json::to_vec(&identity)?)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        // Coordinate the same workspace even when callers choose different
        // journal directories. This is a local advisory lock, not a task lease.
        let root = home()?;
        std::fs::create_dir_all(&root)?;
        let standalone = root.join("standalone");
        private::directory(&standalone, false)?;
        let locks = standalone.join("locks");
        private::directory(&locks, false)?;
        let workspace_lock = lock_file(&locks.join(format!("{workspace_id}.lock")))?;
        workspace_lock
            .try_lock_exclusive()
            .context("Standalone workspace already has a writer")?;
        let path = match path {
            Some(path) => path,
            None => standalone.join(&workspace_id),
        };
        ensure!(path.is_absolute(), "Storage path must be absolute");
        let parent = path
            .parent()
            .context("Storage requires a parent directory")?
            .canonicalize()?;
        let path = parent.join(
            path.file_name()
                .context("Storage requires a directory name")?,
        );
        // Spelling comparisons do not identify case/normalization aliases on
        // all supported filesystems. Check opened directory identities instead.
        for ancestor in parent.ancestors() {
            ensure!(
                super::super::workspace::directory_identity(ancestor)? != identity,
                "Keep private session storage outside the workspace"
            );
        }
        if path.try_exists()? {
            ensure!(
                super::super::workspace::directory_identity(&path)? != identity,
                "Keep private session storage outside the workspace"
            );
        }
        private::directory(&path, false)?;
        let lock_path = path.join("workspace.lock");
        let lock = lock_file(&lock_path)?;
        lock.try_lock_exclusive()
            .context("Standalone workspace storage already has a writer")?;
        let binding = Binding {
            version: 1,
            workspace: workspace.to_owned(),
            workspace_id: workspace_id.clone(),
        };
        let marker = path.join("workspace.json");
        if marker.try_exists()? {
            let mut bytes = Vec::new();
            private::read_only_file(&marker)?
                .take(16 * 1024 + 1)
                .read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= 16 * 1024, "Storage binding exceeds limit");
            let previous: Binding = serde_json::from_slice(&bytes)?;
            ensure!(
                previous.version == binding.version
                    && previous.workspace_id == workspace_id
                    && super::super::workspace::directory_identity(&previous.workspace)?
                        == super::super::workspace::directory_identity(workspace)?,
                "Storage belongs to another workspace"
            );
        } else {
            let mut file = private::file(&marker, true)?;
            if let Err(error) = (|| -> Result<()> {
                file.write_all(&journal::encode(&binding, 16 * 1024)?)?;
                file.sync_all()?;
                private::sync_directory(&path)?;
                Ok(())
            })() {
                drop(file);
                let _ = std::fs::remove_file(&marker);
                return Err(error);
            }
        }
        Ok(Self {
            path,
            workspace_id,
            lock,
            workspace_lock,
        })
    }
}
impl Drop for Storage {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.lock);
        let _ = FileExt::unlock(&self.workspace_lock);
    }
}

pub(super) struct Recovery {
    pub transcript: String,
    pub budget: Budget,
}

pub(super) fn resume(store: &Storage, id: &str) -> Result<Recovery> {
    ensure!(journal::valid_id(id), "Invalid resume session ID");
    let directory = store.path.join("nano/sessions").join(id);
    let (journal, records) = FileJournal::recover(&directory, Quotas::default())?;
    ensure!(
        matches!(records.first().map(|r| &r.event), Some(SessionEvent::Started { identity, system_prompt: Some(prompt), .. }) if identity.project_id == store.workspace_id && identity.task_id.is_none() && identity.run_id.is_none() && prompt == super::POLICY),
        "Resume journal does not belong to this standalone workspace"
    );
    let replay = Replay::from_records(&records)?;
    ensure!(
        replay.pending_effect.is_none() && replay.unknown_effects.is_empty(),
        "Resume requires manual reconciliation of unknown effects; no operation was replayed"
    );
    let expected =
        effect_recovery::recorded_commands(&records).context("Unverifiable command journal")?;
    ensure!(
        !effect_recovery::unresolved_commands(journal.directory(), id, &expected)?,
        "Resume requires complete terminal command spools; no process was replayed"
    );
    let transcript = String::from_utf8(journal::encode(&replay.messages, 160 * 1024)?)?;
    let mut budget = replay.budget;
    budget.estimated_input_tokens = budget
        .estimated_input_tokens
        .saturating_add(budget.reserved_input_tokens);
    budget.estimated_output_tokens = budget
        .estimated_output_tokens
        .saturating_add(budget.reserved_output_tokens);
    budget.reserved_input_tokens = 0;
    budget.reserved_output_tokens = 0;
    // Explicit continuation is a new attempt with a fresh wall-clock deadline.
    budget.elapsed_ms = 0;
    budget.no_progress = 0;
    Ok(Recovery { transcript, budget })
}

fn lock_file(path: &Path) -> Result<File> {
    match private::file(path, true) {
        Ok(file) => Ok(file),
        Err(_) if path.try_exists()? => private::file(path, false),
        Err(error) => Err(error),
    }
}

pub(super) fn fresh_id() -> Result<String> {
    use ring::rand::SecureRandom;
    let mut bytes = [0; 16];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("Cannot generate session identity"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
