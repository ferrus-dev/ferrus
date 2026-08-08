use std::{
    ffi::{OsStr, OsString},
    fs::{self, File},
    path::{Path, PathBuf},
};

use super::{
    Candidate, CandidateKind, ConfinedOpenError, DiagnosticCollector, DiscoveryScan, SourceContent,
    SourceDiscoveryContext, SourceDiscoveryMetrics, SourceError, SourceManifest, SourceRoot,
    build_manifest, normalize_discovered_path, read_verified, same_manifest_identity,
};
use crate::repository_graph::{
    domain::{RepoPath, SourceKind},
    ports::RepositorySource,
};

const MAX_DIRECTORY_DEPTH: usize = 256;

#[derive(Debug, Clone)]
pub struct FilesystemRepositorySource {
    root: SourceRoot,
    context: SourceDiscoveryContext,
    manifest: SourceManifest,
}

impl FilesystemRepositorySource {
    pub fn discover(
        root: impl AsRef<Path>,
        context: SourceDiscoveryContext,
    ) -> Result<Self, SourceError> {
        let root = canonical_root(root.as_ref())?;
        let source_root = SourceRoot::new(root.clone())?;
        let mut candidates = Vec::new();
        let mut diagnostics = DiagnosticCollector::new(context.limits.max_diagnostics);
        let mut metrics = SourceDiscoveryMetrics::default();
        let root_directory = source_root.root_directory()?;
        collect_directory(
            &source_root,
            root_directory,
            None,
            &context,
            &mut candidates,
            &mut diagnostics,
            &mut metrics,
        )?;
        let manifest = build_manifest(
            &source_root,
            &context,
            DiscoveryScan {
                source_kind: SourceKind::NonGitManifest,
                base_revision: None,
                dirty: false,
                candidates,
                diagnostics,
                metrics,
            },
        )?;
        Ok(Self {
            root: source_root,
            context,
            manifest,
        })
    }

    pub fn root(&self) -> &Path {
        self.root.path()
    }

    pub fn manifest(&self) -> &SourceManifest {
        &self.manifest
    }

    pub fn read_verified(
        &self,
        file: &crate::repository_graph::ports::SourceFileDescriptor,
    ) -> Result<SourceContent, SourceError> {
        read_verified(&self.root, &self.manifest, file)
    }

    pub fn revalidate(&self) -> Result<bool, SourceError> {
        let current_root = match fs::canonicalize(self.root.path()) {
            Ok(root) => root,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(source) => {
                return Err(SourceError::Io {
                    operation: "revalidate source root",
                    source,
                });
            }
        };
        if current_root != self.root.path() {
            return Ok(false);
        }
        let current = Self::discover(current_root, self.context.clone())?;
        Ok(same_manifest_identity(&self.manifest, &current.manifest))
    }
}

impl RepositorySource for FilesystemRepositorySource {
    type Error = SourceError;

    fn repository(&self) -> &crate::repository_graph::domain::RepositoryRef {
        &self.manifest.revision.repository
    }

    fn manifest(&self) -> &SourceManifest {
        &self.manifest
    }

    fn read_verified(
        &self,
        file: &crate::repository_graph::ports::SourceFileDescriptor,
    ) -> Result<SourceContent, Self::Error> {
        self.read_verified(file)
    }

    fn revalidate(&self) -> Result<bool, Self::Error> {
        self.revalidate()
    }
}

fn canonical_root(root: &Path) -> Result<std::path::PathBuf, SourceError> {
    let metadata = fs::metadata(root).map_err(|_| SourceError::InvalidRoot)?;
    if !metadata.is_dir() {
        return Err(SourceError::InvalidRoot);
    }
    fs::canonicalize(root).map_err(|_| SourceError::InvalidRoot)
}

#[allow(clippy::too_many_arguments)]
fn collect_directory(
    root: &SourceRoot,
    directory: File,
    relative_directory: Option<&RepoPath>,
    context: &SourceDiscoveryContext,
    candidates: &mut Vec<Candidate>,
    diagnostics: &mut DiagnosticCollector,
    metrics: &mut SourceDiscoveryMetrics,
) -> Result<(), SourceError> {
    if metrics.directories >= context.limits.max_directories {
        return Err(SourceError::DirectoryLimitExceeded {
            limit: context.limits.max_directories,
        });
    }
    metrics.directories = metrics.directories.saturating_add(1);
    let result = visit_directory_entries(&directory, |name, kind| {
        let Ok(path) = child_path(relative_directory, &name) else {
            diagnostics.push("path_encoding_unsupported", None);
            metrics.skipped = metrics.skipped.saturating_add(1);
            return Ok(());
        };

        if kind == RawEntryKind::Unreadable {
            diagnostics.push("file_type_unreadable", Some(path));
            metrics.skipped = metrics.skipped.saturating_add(1);
            return Ok(());
        }
        if kind == RawEntryKind::Directory {
            if let Some(code) = context.policy.exclusion_for_directory(&path) {
                diagnostics.push(code, Some(path));
                metrics.skipped = metrics.skipped.saturating_add(1);
                return Ok(());
            }
            if path.as_str().split('/').count() >= MAX_DIRECTORY_DEPTH {
                diagnostics.push("directory_depth_exceeded", Some(path));
                metrics.skipped = metrics.skipped.saturating_add(1);
                return Ok(());
            }
            let child = match root.open_directory(&path) {
                Ok(directory) => directory,
                Err(ConfinedOpenError::Symlink) => {
                    diagnostics.push("symlink_skipped", Some(path));
                    metrics.skipped = metrics.skipped.saturating_add(1);
                    return Ok(());
                }
                Err(ConfinedOpenError::Io(error))
                    if error.kind() == std::io::ErrorKind::NotFound =>
                {
                    diagnostics.push("directory_missing", Some(path));
                    metrics.skipped = metrics.skipped.saturating_add(1);
                    return Ok(());
                }
                Err(ConfinedOpenError::Io(_)) => {
                    diagnostics.push("directory_unreadable", Some(path));
                    metrics.skipped = metrics.skipped.saturating_add(1);
                    return Ok(());
                }
            };
            collect_directory(
                root,
                child,
                Some(&path),
                context,
                candidates,
                diagnostics,
                metrics,
            )?;
            return Ok(());
        }

        if metrics.candidates >= context.limits.max_files {
            return Err(SourceError::FileLimitExceeded {
                limit: context.limits.max_files,
            });
        }
        metrics.candidates = metrics.candidates.saturating_add(1);
        let candidate_kind = match kind {
            RawEntryKind::File => CandidateKind::File(None),
            RawEntryKind::Symlink => CandidateKind::Symlink,
            RawEntryKind::Special | RawEntryKind::Unreadable => CandidateKind::Special,
            RawEntryKind::Directory => unreachable!("directories recurse above"),
        };
        candidates.push(Candidate {
            path,
            kind: candidate_kind,
            untracked: false,
        });
        Ok(())
    });
    match result {
        Ok(()) => Ok(()),
        Err(DirectoryVisitError::Source(error)) => Err(error),
        Err(DirectoryVisitError::Io(source)) if relative_directory.is_none() => {
            Err(SourceError::Io {
                operation: "read source root",
                source,
            })
        }
        Err(DirectoryVisitError::Io(_)) => {
            diagnostics.push("directory_unreadable", relative_directory.cloned());
            metrics.skipped = metrics.skipped.saturating_add(1);
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RawEntryKind {
    Directory,
    File,
    Symlink,
    Special,
    Unreadable,
}

#[derive(Debug)]
enum DirectoryVisitError {
    Source(SourceError),
    Io(std::io::Error),
}

impl From<SourceError> for DirectoryVisitError {
    fn from(error: SourceError) -> Self {
        Self::Source(error)
    }
}

fn child_path(parent: Option<&RepoPath>, name: &OsStr) -> Result<RepoPath, ()> {
    let mut relative = PathBuf::new();
    if let Some(parent) = parent {
        relative.push(parent.as_str());
    }
    relative.push(name);
    normalize_discovered_path(&relative)
}

#[cfg(not(any(unix, windows)))]
fn visit_directory_entries(
    directory: &File,
    mut visit: impl FnMut(OsString, RawEntryKind) -> Result<(), SourceError>,
) -> Result<(), DirectoryVisitError> {
    let _ = directory;
    Err(DirectoryVisitError::Io(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "handle-relative directory enumeration is unsupported",
    )))
}

/*
 * Platform visitors below enumerate from the already-open directory handle.
 * This is the traversal-side counterpart to handle-relative file opens: a
 * concurrent directory-to-symlink replacement cannot redirect discovery.
 */

#[cfg(unix)]
fn visit_directory_entries(
    directory: &File,
    mut visit: impl FnMut(OsString, RawEntryKind) -> Result<(), SourceError>,
) -> Result<(), DirectoryVisitError> {
    use std::{
        ffi::CStr,
        os::{
            fd::{FromRawFd, IntoRawFd},
            unix::ffi::OsStringExt,
        },
    };

    struct DirectoryStream(*mut libc::DIR);

    impl Drop for DirectoryStream {
        fn drop(&mut self) {
            // SAFETY: the stream is created by `fdopendir` and owned here.
            unsafe {
                libc::closedir(self.0);
            }
        }
    }

    let descriptor = directory
        .try_clone()
        .map_err(DirectoryVisitError::Io)?
        .into_raw_fd();
    // SAFETY: ownership of the cloned descriptor transfers to `fdopendir`.
    let stream = unsafe { libc::fdopendir(descriptor) };
    if stream.is_null() {
        let source = std::io::Error::last_os_error();
        // SAFETY: `fdopendir` failed and did not take ownership.
        drop(unsafe { File::from_raw_fd(descriptor) });
        return Err(DirectoryVisitError::Io(source));
    }
    let stream = DirectoryStream(stream);
    loop {
        errno::set_errno(errno::Errno(0));
        // SAFETY: `stream` remains live and is only accessed on this thread.
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let error = errno::errno();
            if error.0 == 0 {
                break;
            }
            return Err(DirectoryVisitError::Io(std::io::Error::from_raw_os_error(
                error.0,
            )));
        }
        // SAFETY: `readdir` returned a live dirent with a NUL-terminated name.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        // SAFETY: zero is a valid initial representation for `stat`.
        let mut status: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: the directory stream and NUL-terminated child name are live;
        // `AT_SYMLINK_NOFOLLOW` inspects the entry itself.
        let result = unsafe {
            libc::fstatat(
                libc::dirfd(stream.0),
                name.as_ptr(),
                &mut status,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        let kind = if result != 0 {
            RawEntryKind::Unreadable
        } else {
            match status.st_mode & libc::S_IFMT {
                libc::S_IFDIR => RawEntryKind::Directory,
                libc::S_IFREG => RawEntryKind::File,
                libc::S_IFLNK => RawEntryKind::Symlink,
                _ => RawEntryKind::Special,
            }
        };
        visit(OsString::from_vec(name.to_bytes().to_vec()), kind)?;
    }
    Ok(())
}

#[cfg(windows)]
fn visit_directory_entries(
    directory: &File,
    mut visit: impl FnMut(OsString, RawEntryKind) -> Result<(), SourceError>,
) -> Result<(), DirectoryVisitError> {
    use std::{
        mem::{offset_of, size_of},
        os::windows::{ffi::OsStringExt, io::AsRawHandle},
    };
    use windows_sys::Win32::{
        Foundation::{ERROR_NO_MORE_FILES, HANDLE},
        Storage::FileSystem::{
            FILE_ATTRIBUTE_DEVICE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
            FILE_ID_BOTH_DIR_INFO, FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo,
            GetFileInformationByHandleEx,
        },
    };

    const BUFFER_BYTES: usize = 64 * 1024;
    let mut buffer = vec![0_u64; BUFFER_BYTES / size_of::<u64>()];
    let mut restart = true;
    loop {
        let class = if restart {
            FileIdBothDirectoryRestartInfo
        } else {
            FileIdBothDirectoryInfo
        };
        // SAFETY: the directory handle remains live, and `buffer` is aligned,
        // writable, and has the advertised capacity for the duration of the call.
        let result = unsafe {
            GetFileInformationByHandleEx(
                directory.as_raw_handle() as HANDLE,
                class,
                buffer.as_mut_ptr().cast(),
                BUFFER_BYTES as u32,
            )
        };
        if result == 0 {
            let source = std::io::Error::last_os_error();
            if source.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                return Ok(());
            }
            return Err(DirectoryVisitError::Io(source));
        }
        restart = false;

        let bytes = buffer.as_ptr().cast::<u8>();
        let mut cursor = 0_usize;
        loop {
            let fixed_size = offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
            if cursor
                .checked_add(fixed_size)
                .is_none_or(|end| end > BUFFER_BYTES)
            {
                return Err(invalid_directory_buffer());
            }
            // SAFETY: the fixed fields fit in the checked buffer region. Some
            // filesystem drivers return entries without their documented
            // alignment, so every field is read unaligned.
            let info = unsafe { bytes.add(cursor).cast::<FILE_ID_BOTH_DIR_INFO>() };
            // SAFETY: the fixed portion of `info` was bounds-checked above.
            let (next, name_bytes, attributes) = unsafe {
                (
                    std::ptr::addr_of!((*info).NextEntryOffset).read_unaligned() as usize,
                    std::ptr::addr_of!((*info).FileNameLength).read_unaligned() as usize,
                    std::ptr::addr_of!((*info).FileAttributes).read_unaligned(),
                )
            };
            if name_bytes == 0 || name_bytes % size_of::<u16>() != 0 {
                return Err(invalid_directory_buffer());
            }
            let name_start = cursor
                .checked_add(fixed_size)
                .ok_or_else(invalid_directory_buffer)?;
            let name_end = name_start
                .checked_add(name_bytes)
                .filter(|end| *end <= BUFFER_BYTES)
                .ok_or_else(invalid_directory_buffer)?;
            let wide = (name_start..name_end)
                .step_by(size_of::<u16>())
                .map(|offset| {
                    // SAFETY: each two-byte code unit lies in the checked name region.
                    unsafe { bytes.add(offset).cast::<u16>().read_unaligned() }
                })
                .collect::<Vec<_>>();
            if !matches!(wide.as_slice(), [46] | [46, 46]) {
                let kind = if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    RawEntryKind::Symlink
                } else if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                    RawEntryKind::Directory
                } else if attributes & FILE_ATTRIBUTE_DEVICE != 0 {
                    RawEntryKind::Special
                } else {
                    RawEntryKind::File
                };
                visit(OsString::from_wide(&wide), kind)?;
            }

            if next == 0 {
                break;
            }
            cursor = cursor
                .checked_add(next)
                .filter(|next_cursor| {
                    *next_cursor > cursor && *next_cursor >= name_end && *next_cursor < BUFFER_BYTES
                })
                .ok_or_else(invalid_directory_buffer)?;
        }
    }
}

#[cfg(windows)]
fn invalid_directory_buffer() -> DirectoryVisitError {
    DirectoryVisitError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "invalid directory enumeration buffer",
    ))
}

#[cfg(test)]
#[path = "filesystem_tests.rs"]
mod tests;
