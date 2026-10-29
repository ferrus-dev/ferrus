//! Canonical workspace approval locking and stale-owner recovery.

use super::*;

pub(crate) struct CanonicalApprovalLock {
    pub(super) path: PathBuf,
    pub(super) owner_path: PathBuf,
    pub(super) owner_file: Option<std::fs::File>,
    // Keep creation, stale recovery, and owner cleanup serialized for the full operation.
    pub(super) _guard: Option<CanonicalApprovalLockGuard>,
}

pub(super) struct CanonicalApprovalLockGuard {
    pub(super) _file: std::fs::File,
}

impl Drop for CanonicalApprovalLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        unregister_canonical_approval_owner(&self.owner_path);
        drop(self.owner_file.take());
        let _ = std::fs::remove_file(&self.owner_path);
    }
}

pub(super) static CANONICAL_APPROVAL_OWNERS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

pub(super) fn canonical_approval_owners() -> &'static Mutex<HashSet<PathBuf>> {
    CANONICAL_APPROVAL_OWNERS.get_or_init(|| Mutex::new(HashSet::new()))
}

pub(super) fn register_canonical_approval_owner(path: &Path) {
    canonical_approval_owners()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(path.to_path_buf());
}

pub(super) fn unregister_canonical_approval_owner(path: &Path) {
    canonical_approval_owners()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(path);
}

pub(super) fn process_holds_canonical_approval_owner(path: &Path) -> bool {
    canonical_approval_owners()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains(path)
}

pub(super) async fn acquire_canonical_approval_lock(
    context: &RuntimeTaskContext,
) -> Result<CanonicalApprovalLock> {
    let local_ref_content =
        tokio::fs::read_to_string(store::resolve_project_path(".ferrus/project.toml")).await?;
    let local_ref: project::LocalProjectRef = toml::from_str(&local_ref_content)?;
    let lock_path = PathBuf::from(local_ref.data_dir).join("canonical-approval.lock");
    acquire_canonical_approval_lock_at(&lock_path, &context.task_id).await
}

pub(crate) async fn acquire_canonical_approval_lock_at(
    lock_path: &Path,
    task_id: &str,
) -> Result<CanonicalApprovalLock> {
    if let Some(parent) = lock_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }

    // Serialize creation and stale-lock removal on a stable guard file so
    // competing recoveries cannot unlink a newly acquired approval lock.
    loop {
        let guard = acquire_canonical_approval_lock_guard(lock_path).await?;
        match try_create_canonical_approval_lock(lock_path, task_id).await {
            Ok(Some(mut lock)) => {
                lock._guard = Some(guard);
                return Ok(lock);
            }
            Ok(None) => {
                if remove_stale_canonical_approval_lock(lock_path).await? {
                    continue;
                }
            }
            Err(err) => return Err(err),
        }

        drop(guard);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub(super) async fn acquire_canonical_approval_lock_guard(
    lock_path: &Path,
) -> Result<CanonicalApprovalLockGuard> {
    let guard_path = canonical_approval_lock_guard_path(lock_path);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&guard_path)
        .with_context(|| {
            format!(
                "Failed to open canonical approval lock guard {}",
                guard_path.display()
            )
        })?;
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => break,
            // Windows reports ERROR_LOCK_VIOLATION rather than WouldBlock.
            Err(err) if err.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(err) => return Err(err).context("Failed to acquire canonical approval lock guard"),
        }
    }

    Ok(CanonicalApprovalLockGuard { _file: file })
}

pub(super) fn canonical_approval_lock_guard_path(lock_path: &Path) -> PathBuf {
    lock_path.with_file_name(".canonical-approval.lock.guard")
}

pub(super) fn canonical_approval_lock_owner_path(lock_path: &Path) -> PathBuf {
    lock_path.with_file_name(".canonical-approval.lock.owner")
}

pub(super) async fn try_create_canonical_approval_lock(
    lock_path: &Path,
    task_id: &str,
) -> Result<Option<CanonicalApprovalLock>> {
    let temp_path = canonical_approval_lock_temp_path(lock_path, task_id);
    let content = format!("pid={}\ntask_id={task_id}\n", std::process::id());
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .with_context(|| {
            format!(
                "Failed to create temporary canonical approval lock {}",
                temp_path.display()
            )
        })?;
    if let Err(err) = file.write_all(content.as_bytes()) {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(err).with_context(|| {
            format!(
                "Failed to write temporary canonical approval lock {}",
                temp_path.display()
            )
        });
    }

    drop(file);

    // Publish a complete marker without replacing an existing owner's path.
    match std::fs::hard_link(&temp_path, lock_path) {
        Ok(()) => {
            let owner_path = canonical_approval_lock_owner_path(lock_path);
            let owner_file = match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&owner_path)
                .and_then(|file| {
                    file.lock_exclusive()?;
                    Ok(file)
                }) {
                Ok(file) => file,
                Err(err) => {
                    let _ = tokio::fs::remove_file(&temp_path).await;
                    let _ = tokio::fs::remove_file(lock_path).await;
                    let _ = tokio::fs::remove_file(&owner_path).await;
                    return Err(err).with_context(|| {
                        format!(
                            "Failed to hold canonical approval owner marker {}",
                            owner_path.display()
                        )
                    });
                }
            };
            register_canonical_approval_owner(&owner_path);
            // No cancellation point between publishing ownership and its RAII guard.
            let _ = std::fs::remove_file(&temp_path);
            Ok(Some(CanonicalApprovalLock {
                path: lock_path.to_path_buf(),
                owner_path,
                owner_file: Some(owner_file),
                _guard: None,
            }))
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = tokio::fs::remove_file(&temp_path).await;
            Ok(None)
        }
        Err(err) => {
            let _ = tokio::fs::remove_file(&temp_path).await;
            Err(err).with_context(|| {
                format!(
                    "Failed to publish canonical approval lock {}",
                    lock_path.display()
                )
            })
        }
    }
}

pub(super) fn canonical_approval_lock_temp_path(lock_path: &Path, task_id: &str) -> PathBuf {
    let task_id = task_id.replace(std::path::MAIN_SEPARATOR, "_");
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    lock_path.with_file_name(format!(
        ".canonical-approval.lock.{}.{}.{}.tmp",
        std::process::id(),
        task_id,
        nonce
    ))
}

pub(super) async fn remove_stale_canonical_approval_lock(lock_path: &Path) -> Result<bool> {
    let owner_path = canonical_approval_lock_owner_path(lock_path);
    if process_holds_canonical_approval_owner(&owner_path) {
        return Ok(false);
    }

    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&owner_path)
    {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return remove_canonical_approval_lock_files(lock_path, &owner_path).await;
        }
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "Failed to open canonical approval owner marker {}",
                    owner_path.display()
                )
            });
        }
    };
    match file.try_lock_exclusive() {
        Ok(()) => {}
        Err(err) if err.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
            return Ok(false);
        }
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "Failed to inspect canonical approval owner marker {}",
                    owner_path.display()
                )
            });
        }
    }

    drop(file);

    remove_canonical_approval_lock_files(lock_path, &owner_path).await
}

pub(super) async fn remove_canonical_approval_lock_files(
    lock_path: &Path,
    owner_path: &Path,
) -> Result<bool> {
    match tokio::fs::remove_file(lock_path).await {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => Err(err).with_context(|| {
            format!(
                "Failed to remove stale canonical approval lock {}",
                lock_path.display()
            )
        })?,
    }

    match tokio::fs::remove_file(owner_path).await {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => Err(err).with_context(|| {
            format!(
                "Failed to remove stale canonical approval owner marker {}",
                owner_path.display()
            )
        })?,
    }

    Ok(true)
}

#[cfg(test)]
pub(super) fn canonical_approval_lock_pid(contents: &str) -> Option<u32> {
    contents.lines().find_map(|line| {
        line.strip_prefix("pid=")
            .and_then(|value| value.parse().ok())
    })
}
