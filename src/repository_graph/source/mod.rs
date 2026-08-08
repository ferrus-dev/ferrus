//! Deterministic local repository discovery and immutable source manifests.
//!
//! Source adapters never execute repository code and v1 deliberately skips all
//! symbolic links, directory links, and Git links. They are reported with
//! bounded diagnostics and are never dereferenced as indexable content.
//!
//! Git discovery includes tracked and repository-ignored-aware untracked files
//! without consulting machine-global excludes. Both adapters hard-exclude any
//! `.git` or `.ferrus` component, apply sensitive patterns case-insensitively,
//! and exclude generated/vendor paths unless explicitly enabled. A NUL anywhere
//! in a bounded file marks it binary. Per-file violations are skipped; file,
//! directory, and aggregate inspected-byte counts are hard limits, and
//! diagnostics retain a deterministic bounded prefix plus a suppressed count.
//! On Unix and Windows, traversal and reads stay rooted at held directory
//! handles so concurrent path or symlink replacement cannot redirect discovery.

mod filesystem;
mod git;
mod worktree;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, Read},
    num::NonZeroU64,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use serde::Serialize;
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use super::{
    config::{
        IndexLimitsConfig, RepositoryGraphConfig, RepositoryGraphConfigError, SourceConfig,
        canonical_pattern_body,
    },
    domain::{
        DiagnosticCode, Digest, ExtractorIdentity, RepoPath, RepositoryRef, SnapshotId, SourceKind,
        SourceRevision, SourceRevisionId,
    },
    ports::{RepositorySource, SnapshotContent, SourceFileDescriptor, SourceFileMode},
    query::{ContentRequest, ContentResponse, QueryError, QueryErrorCode, RetrievalAction},
};

pub use super::ports::{SourceContent, SourceDiagnostic, SourceDiscoveryMetrics, SourceManifest};
pub use filesystem::FilesystemRepositorySource;
pub use git::GitRepositorySource;
pub use worktree::{
    GitWorktreeChange, GitWorktreeInventory, TaskOverlaySource, TaskWorktreeOverlay,
    capture_worktree_tree, parse_git_tree_digest, pin_submitted_tree, release_submitted_tree_pin,
};

const SOURCE_MANIFEST_VERSION: u32 = 1;
const SOURCE_POLICY_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum SourceError {
    #[error("repository source root must be an existing directory")]
    InvalidRoot,
    #[error("repository source configuration is invalid")]
    Config(#[from] RepositoryGraphConfigError),
    #[error("repository source I/O failed during {operation}")]
    Io {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("Git repository discovery failed during {operation}")]
    GitCommand { operation: &'static str },
    #[error("repository source root is not the Git worktree root")]
    NotGitRoot,
    #[error("repository source does not match the requested repository identity")]
    RepositoryMismatch,
    #[error("pinned Git tree identity is invalid or unsupported")]
    InvalidGitTree,
    #[error("repository source contains colliding portable paths")]
    PathCollision,
    #[error("repository source file count exceeds configured limit {limit}")]
    FileLimitExceeded { limit: u64 },
    #[error("repository source directory count exceeds configured limit {limit}")]
    DirectoryLimitExceeded { limit: u64 },
    #[error("repository source bytes exceed configured limit {limit}")]
    TotalBytesLimitExceeded { limit: u64 },
    #[error("repository source content changed after discovery")]
    ContentChanged,
    #[error("repository source file is not part of the discovered manifest")]
    FileNotInManifest,
}

#[derive(Debug, Clone)]
pub struct SourceDiscoveryContext {
    repository: RepositoryRef,
    analysis_config_digest: Digest,
    source_policy_digest: Digest,
    extractor_set_digest: Digest,
    policy: SourcePolicy,
    limits: IndexLimitsConfig,
}

impl SourceDiscoveryContext {
    pub fn from_config(
        repository: RepositoryRef,
        config: &RepositoryGraphConfig,
        extractors: &[ExtractorIdentity],
    ) -> Result<Self, SourceError> {
        let analysis_config_digest = config.analysis_config_digest()?;
        let source_policy_digest = config.source_policy_digest()?;
        Ok(Self {
            repository,
            analysis_config_digest,
            source_policy_digest,
            extractor_set_digest: extractor_set_digest(extractors),
            policy: SourcePolicy::new(&config.source)?,
            limits: config.index_limits.clone(),
        })
    }

    pub fn repository(&self) -> &RepositoryRef {
        &self.repository
    }

    pub fn analysis_config_digest(&self) -> &Digest {
        &self.analysis_config_digest
    }
}

#[derive(Debug, Clone)]
pub(super) struct SourceRoot {
    path: PathBuf,
    #[cfg(any(unix, windows))]
    directory: Arc<File>,
}

#[derive(Debug)]
pub(super) enum ConfinedOpenError {
    Symlink,
    Io(io::Error),
}

impl SourceRoot {
    pub fn new(path: PathBuf) -> Result<Self, SourceError> {
        #[cfg(unix)]
        let directory = {
            use std::{
                ffi::CString,
                fs::OpenOptions,
                os::{
                    fd::{AsRawFd, FromRawFd},
                    unix::{ffi::OsStrExt, fs::OpenOptionsExt},
                },
            };

            let mut directory = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(Path::new("/"))
                .map_err(|source| SourceError::Io {
                    operation: "open filesystem root",
                    source,
                })?;
            for component in path.components() {
                let Component::Normal(component) = component else {
                    if component == Component::RootDir {
                        continue;
                    }
                    return Err(SourceError::InvalidRoot);
                };
                let component =
                    CString::new(component.as_bytes()).map_err(|_| SourceError::InvalidRoot)?;
                // SAFETY: `directory` is a live directory descriptor and the
                // canonical path component is NUL-terminated.
                let descriptor = unsafe {
                    libc::openat(
                        directory.as_raw_fd(),
                        component.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if descriptor < 0 {
                    return Err(SourceError::Io {
                        operation: "open source root component",
                        source: io::Error::last_os_error(),
                    });
                }
                // SAFETY: `openat` returned a new owned descriptor.
                directory = unsafe { File::from_raw_fd(descriptor) };
            }
            Arc::new(directory)
        };
        #[cfg(windows)]
        let directory = {
            use std::{
                fs::OpenOptions,
                os::windows::fs::{MetadataExt, OpenOptionsExt},
            };
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
                FILE_FLAG_OPEN_REPARSE_POINT,
            };

            let directory = OpenOptions::new()
                .read(true)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(&path)
                .map_err(|source| SourceError::Io {
                    operation: "open source root",
                    source,
                })?;
            let metadata = directory.metadata().map_err(|source| SourceError::Io {
                operation: "inspect source root handle",
                source,
            })?;
            if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            {
                return Err(SourceError::InvalidRoot);
            }
            let final_path = windows_final_path(&directory).map_err(|source| SourceError::Io {
                operation: "resolve source root handle",
                source,
            })?;
            if !windows_paths_equal(&path, &final_path) {
                return Err(SourceError::InvalidRoot);
            }
            Arc::new(directory)
        };
        Ok(Self {
            path,
            #[cfg(any(unix, windows))]
            directory,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn root_directory(&self) -> Result<File, SourceError> {
        #[cfg(any(unix, windows))]
        {
            self.directory
                .try_clone()
                .map_err(|source| SourceError::Io {
                    operation: "clone source root handle",
                    source,
                })
        }

        #[cfg(not(any(unix, windows)))]
        File::open(&self.path).map_err(|source| SourceError::Io {
            operation: "open source root handle",
            source,
        })
    }

    #[cfg(unix)]
    fn open_unix_path(
        &self,
        path: &RepoPath,
        final_component_is_directory: bool,
    ) -> Result<File, ConfinedOpenError> {
        use std::{ffi::CString, os::fd::AsRawFd, os::fd::FromRawFd};

        let mut directory = self.directory.try_clone().map_err(ConfinedOpenError::Io)?;
        let mut components = path.as_str().split('/').peekable();
        while let Some(component) = components.next() {
            let component = CString::new(component).expect("RepoPath rejects NUL bytes");
            let is_final = components.peek().is_none();
            let directory_flag = !is_final || final_component_is_directory;
            let mut flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
            if directory_flag {
                flags |= libc::O_DIRECTORY;
            } else {
                flags |= libc::O_NONBLOCK;
            }
            // SAFETY: `directory` is a live directory descriptor, `component`
            // is NUL-terminated, and the returned descriptor is owned below.
            let descriptor =
                unsafe { libc::openat(directory.as_raw_fd(), component.as_ptr(), flags) };
            if descriptor < 0 {
                let error = io::Error::last_os_error();
                return if matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) {
                    Err(ConfinedOpenError::Symlink)
                } else {
                    Err(ConfinedOpenError::Io(error))
                };
            }
            // SAFETY: `openat` returned a new owned descriptor.
            directory = unsafe { File::from_raw_fd(descriptor) };
        }
        Ok(directory)
    }

    #[cfg(windows)]
    fn open_windows_path(
        &self,
        path: &RepoPath,
        final_component_is_directory: bool,
    ) -> Result<File, ConfinedOpenError> {
        use std::{
            mem::size_of,
            os::windows::{
                fs::MetadataExt,
                io::{AsRawHandle, FromRawHandle},
            },
        };
        use windows_sys::Wdk::{
            Foundation::OBJECT_ATTRIBUTES,
            Storage::FileSystem::{
                FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
                FILE_OPEN_FOR_BACKUP_INTENT, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT,
                NtCreateFile,
            },
        };
        use windows_sys::Win32::{
            Foundation::{
                HANDLE, OBJ_CASE_INSENSITIVE, OBJ_DONT_REPARSE, RtlNtStatusToDosError,
                STATUS_NOT_A_DIRECTORY, STATUS_REPARSE_POINT_ENCOUNTERED, UNICODE_STRING,
            },
            Storage::FileSystem::{
                FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_GENERIC_READ,
                FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            },
            System::IO::IO_STATUS_BLOCK,
        };

        let mut directory = self.directory.try_clone().map_err(ConfinedOpenError::Io)?;
        let mut components = path.as_str().split('/').peekable();
        while let Some(component) = components.next() {
            let mut name = component.encode_utf16().collect::<Vec<_>>();
            let name_bytes = name
                .len()
                .checked_mul(size_of::<u16>())
                .and_then(|length| u16::try_from(length).ok())
                .ok_or_else(|| {
                    ConfinedOpenError::Io(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "repository path component is too long",
                    ))
                })?;
            let object_name = UNICODE_STRING {
                Length: name_bytes,
                MaximumLength: name_bytes,
                Buffer: name.as_mut_ptr(),
            };
            let attributes = OBJECT_ATTRIBUTES {
                Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
                RootDirectory: directory.as_raw_handle() as HANDLE,
                ObjectName: &object_name,
                Attributes: OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE,
                SecurityDescriptor: std::ptr::null(),
                SecurityQualityOfService: std::ptr::null(),
            };
            let is_directory = components.peek().is_some() || final_component_is_directory;
            let create_options = FILE_OPEN_REPARSE_POINT
                | FILE_SYNCHRONOUS_IO_NONALERT
                | FILE_OPEN_FOR_BACKUP_INTENT
                | if is_directory {
                    FILE_DIRECTORY_FILE
                } else {
                    FILE_NON_DIRECTORY_FILE
                };
            let mut handle: HANDLE = std::ptr::null_mut();
            let mut io_status = IO_STATUS_BLOCK::default();
            // SAFETY: all structures and the UTF-16 component remain live for
            // the synchronous call. The returned handle is owned below.
            let status = unsafe {
                NtCreateFile(
                    &mut handle,
                    FILE_GENERIC_READ,
                    &attributes,
                    &mut io_status,
                    std::ptr::null(),
                    FILE_ATTRIBUTE_NORMAL,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    FILE_OPEN,
                    create_options,
                    std::ptr::null(),
                    0,
                )
            };
            if status < 0 {
                return if matches!(
                    status,
                    STATUS_REPARSE_POINT_ENCOUNTERED | STATUS_NOT_A_DIRECTORY
                ) {
                    Err(ConfinedOpenError::Symlink)
                } else {
                    // SAFETY: conversion is pure for the returned NTSTATUS.
                    let error = unsafe { RtlNtStatusToDosError(status) };
                    Err(ConfinedOpenError::Io(io::Error::from_raw_os_error(
                        error as i32,
                    )))
                };
            }
            if handle.is_null() {
                return Err(ConfinedOpenError::Io(io::Error::other(
                    "Windows returned an empty file handle",
                )));
            }
            // SAFETY: successful `NtCreateFile` returned a new owned handle.
            let opened = unsafe { File::from_raw_handle(handle.cast()) };
            let metadata = opened.metadata().map_err(ConfinedOpenError::Io)?;
            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(ConfinedOpenError::Symlink);
            }
            directory = opened;
        }
        Ok(directory)
    }

    fn open_file(&self, path: &RepoPath) -> Result<File, ConfinedOpenError> {
        #[cfg(unix)]
        {
            self.open_unix_path(path, false)
        }

        #[cfg(windows)]
        {
            self.open_windows_path(path, false)
        }

        #[cfg(not(any(unix, windows)))]
        {
            let absolute = self.path.join(path.as_str());
            if fs::symlink_metadata(&absolute)
                .map_err(ConfinedOpenError::Io)?
                .file_type()
                .is_symlink()
            {
                return Err(ConfinedOpenError::Symlink);
            }
            let resolved = fs::canonicalize(&absolute).map_err(ConfinedOpenError::Io)?;
            if !resolved.starts_with(&self.path) {
                return Err(ConfinedOpenError::Symlink);
            }
            File::open(resolved).map_err(ConfinedOpenError::Io)
        }
    }

    pub(super) fn open_directory(&self, path: &RepoPath) -> Result<File, ConfinedOpenError> {
        #[cfg(unix)]
        {
            self.open_unix_path(path, true)
        }

        #[cfg(windows)]
        {
            self.open_windows_path(path, true)
        }

        #[cfg(not(any(unix, windows)))]
        {
            let directory = self.open_file(path)?;
            let metadata = directory.metadata().map_err(ConfinedOpenError::Io)?;
            if !metadata.file_type().is_dir() {
                return Err(ConfinedOpenError::Symlink);
            }
            Ok(directory)
        }
    }
}

#[cfg(windows)]
fn windows_final_path(file: &File) -> io::Result<String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;

    let handle = file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
    // SAFETY: the handle remains live for both calls and the second buffer has
    // the capacity reported by the first call.
    let required = unsafe { GetFinalPathNameByHandleW(handle, std::ptr::null_mut(), 0, 0) };
    if required == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buffer = vec![0_u16; required as usize + 1];
    let written =
        unsafe { GetFinalPathNameByHandleW(handle, buffer.as_mut_ptr(), buffer.len() as u32, 0) };
    if written == 0 || written as usize >= buffer.len() {
        return Err(io::Error::last_os_error());
    }
    String::from_utf16(&buffer[..written as usize])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-UTF-16 final path"))
}

#[cfg(any(windows, test))]
fn windows_paths_equal(expected: &Path, actual: &str) -> bool {
    fn strip_prefix_ignore_ascii_case<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
        let candidate = path.get(..prefix.len())?;
        if candidate.eq_ignore_ascii_case(prefix) {
            path.get(prefix.len()..)
        } else {
            None
        }
    }

    fn normalize(path: &str) -> String {
        let path = path.replace('/', "\\");
        let path = if let Some(path) = strip_prefix_ignore_ascii_case(&path, r"\\?\UNC\") {
            format!(r"\\{path}")
        } else if let Some(path) = strip_prefix_ignore_ascii_case(&path, r"\\?\") {
            path.to_string()
        } else {
            path
        };
        path.trim_end_matches('\\').to_ascii_lowercase()
    }

    normalize(&expected.to_string_lossy()) == normalize(actual)
}

#[derive(Debug, Clone)]
pub enum LocalRepositorySource {
    Git(GitRepositorySource),
    Filesystem(FilesystemRepositorySource),
}

impl LocalRepositorySource {
    pub fn discover(
        root: impl AsRef<Path>,
        context: SourceDiscoveryContext,
    ) -> Result<Self, SourceError> {
        let root = root.as_ref();
        match fs::symlink_metadata(root.join(".git")) {
            Ok(_) => GitRepositorySource::discover(root, context).map(Self::Git),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                FilesystemRepositorySource::discover(root, context).map(Self::Filesystem)
            }
            Err(source) => Err(SourceError::Io {
                operation: "inspect Git worktree metadata",
                source,
            }),
        }
    }

    pub fn manifest(&self) -> &SourceManifest {
        match self {
            Self::Git(source) => source.manifest(),
            Self::Filesystem(source) => source.manifest(),
        }
    }

    pub fn read_verified(&self, file: &SourceFileDescriptor) -> Result<SourceContent, SourceError> {
        match self {
            Self::Git(source) => source.read_verified(file),
            Self::Filesystem(source) => source.read_verified(file),
        }
    }

    pub fn revalidate(&self) -> Result<bool, SourceError> {
        match self {
            Self::Git(source) => source.revalidate(),
            Self::Filesystem(source) => source.revalidate(),
        }
    }
}

impl RepositorySource for LocalRepositorySource {
    type Error = SourceError;

    fn repository(&self) -> &RepositoryRef {
        &self.manifest().revision.repository
    }

    fn manifest(&self) -> &SourceManifest {
        self.manifest()
    }

    fn read_verified(&self, file: &SourceFileDescriptor) -> Result<SourceContent, Self::Error> {
        self.read_verified(file)
    }

    fn revalidate(&self) -> Result<bool, Self::Error> {
        self.revalidate()
    }
}

/// A task's immutable baseline read directly from its pinned Git tree.
///
/// The managed worktree is used only to locate the repository object database;
/// discovery, extractor reads, and revalidation never observe checkout bytes.
#[derive(Debug, Clone)]
pub struct TaskBaselineSource {
    root: PathBuf,
    baseline_tree: Digest,
    manifest: SourceManifest,
}

impl TaskBaselineSource {
    pub fn discover(
        root: impl AsRef<Path>,
        context: SourceDiscoveryContext,
        baseline_tree: Digest,
    ) -> Result<Self, SourceError> {
        let root = git::canonical_root(root.as_ref())?;
        git::ensure_worktree_root(&root)?;
        let manifest = worktree::discover_tree_manifest(&root, &context, baseline_tree.clone())?;
        Ok(Self {
            root,
            baseline_tree,
            manifest,
        })
    }
}

impl RepositorySource for TaskBaselineSource {
    type Error = SourceError;

    fn repository(&self) -> &RepositoryRef {
        &self.manifest.revision.repository
    }

    fn manifest(&self) -> &SourceManifest {
        &self.manifest
    }

    fn read_verified(&self, file: &SourceFileDescriptor) -> Result<SourceContent, Self::Error> {
        if !self.manifest.files.iter().any(|stored| stored == file) {
            return Err(SourceError::FileNotInManifest);
        }
        worktree::read_tree_descriptor_verified(&self.root, &self.baseline_tree, file)
    }

    fn revalidate(&self) -> Result<bool, Self::Error> {
        worktree::verify_tree_available(&self.root, &self.baseline_tree).map(|()| true)
    }
}

/// A repository-root-confined reader for file identities persisted in one
/// immutable graph snapshot. It never discovers source identity from the
/// process working directory and never follows symbolic links.
mod content;
use content::same_manifest_identity;
pub use content::*;

mod discovery;
pub use discovery::extractor_set_digest;
use discovery::*;

mod policy;
use policy::*;

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
