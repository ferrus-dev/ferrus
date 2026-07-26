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
pub struct LocalSnapshotContent {
    root: SourceRoot,
    repository: RepositoryRef,
    snapshot_id: SnapshotId,
    files: BTreeMap<RepoPath, SourceFileDescriptor>,
    policy: SourcePolicy,
    hard_max_bytes: NonZeroU64,
}

/// Snapshot reader backed by an immutable Git tree captured at submission.
/// Git objects remain available after the managed worktree is removed, while
/// every returned blob is still checked against the graph file descriptor.
pub struct GitTreeSnapshotContent {
    root: PathBuf,
    repository: RepositoryRef,
    snapshot_id: SnapshotId,
    tree: Digest,
    files: BTreeMap<RepoPath, SourceFileDescriptor>,
    policy: SourcePolicy,
    hard_max_bytes: NonZeroU64,
}

impl GitTreeSnapshotContent {
    pub fn new(
        root: impl AsRef<Path>,
        repository: RepositoryRef,
        snapshot_id: SnapshotId,
        tree: Digest,
        config: &SourceConfig,
        files: Vec<SourceFileDescriptor>,
        hard_max_bytes: NonZeroU64,
    ) -> Result<Self, SourceError> {
        let root = fs::canonicalize(root.as_ref()).map_err(|source| SourceError::Io {
            operation: "canonicalize frozen content repository root",
            source,
        })?;
        Ok(Self {
            root,
            repository,
            snapshot_id,
            tree,
            files: files
                .into_iter()
                .map(|file| (file.path.clone(), file))
                .collect(),
            policy: SourcePolicy::new(config)?,
            hard_max_bytes,
        })
    }
}

impl SnapshotContent for GitTreeSnapshotContent {
    fn read_verified(&self, request: &ContentRequest) -> Result<ContentResponse, QueryError> {
        if request.wire_version != super::QUERY_WIRE_VERSION {
            return Err(content_error(
                QueryErrorCode::UnsupportedWireVersion,
                "unsupported repository content wire version",
                false,
                None,
            ));
        }
        if request.repository != self.repository || request.snapshot_id != self.snapshot_id {
            return Err(content_error(
                QueryErrorCode::InvalidRequest,
                "repository content request does not match the selected snapshot",
                false,
                None,
            ));
        }
        if self.policy.exclusion_for_file(&request.path).is_some() {
            return Err(content_error(
                QueryErrorCode::ContentUnavailable,
                "repository content is excluded by the source policy",
                false,
                None,
            ));
        }
        let Some(file) = self.files.get(&request.path) else {
            return Err(content_error(
                QueryErrorCode::ContentUnavailable,
                "repository content is unavailable for the selected snapshot",
                false,
                None,
            ));
        };
        if file.content_identity != request.expected_content_identity {
            return Err(content_error(
                QueryErrorCode::ContentChanged,
                "repository content identity does not match the selected snapshot",
                false,
                Some(RetrievalAction::RefreshIndex),
            ));
        }
        let content = worktree::read_tree_descriptor_verified(&self.root, &self.tree, file)
            .map_err(|error| match error {
                SourceError::ContentChanged => content_error(
                    QueryErrorCode::ContentChanged,
                    "frozen repository content does not match the selected snapshot",
                    false,
                    None,
                ),
                _ => content_error(
                    QueryErrorCode::ContentUnavailable,
                    "frozen repository content could not be read",
                    true,
                    None,
                ),
            })?;
        content_response_for_bytes(
            request,
            &self.repository,
            &self.snapshot_id,
            file,
            &content.bytes,
            self.hard_max_bytes,
        )
    }
}

impl LocalSnapshotContent {
    pub fn new(
        root: impl AsRef<Path>,
        repository: RepositoryRef,
        snapshot_id: SnapshotId,
        config: &SourceConfig,
        files: Vec<SourceFileDescriptor>,
        hard_max_bytes: NonZeroU64,
    ) -> Result<Self, SourceError> {
        let root = fs::canonicalize(root.as_ref()).map_err(|source| SourceError::Io {
            operation: "canonicalize snapshot content root",
            source,
        })?;
        Ok(Self {
            root: SourceRoot::new(root)?,
            repository,
            snapshot_id,
            files: files
                .into_iter()
                .map(|file| (file.path.clone(), file))
                .collect(),
            policy: SourcePolicy::new(config)?,
            hard_max_bytes,
        })
    }
}

impl SnapshotContent for LocalSnapshotContent {
    fn read_verified(&self, request: &ContentRequest) -> Result<ContentResponse, QueryError> {
        if request.wire_version != super::QUERY_WIRE_VERSION {
            return Err(content_error(
                QueryErrorCode::UnsupportedWireVersion,
                "unsupported repository content wire version",
                false,
                None,
            ));
        }
        if request.repository != self.repository || request.snapshot_id != self.snapshot_id {
            return Err(content_error(
                QueryErrorCode::InvalidRequest,
                "repository content request does not match the selected snapshot",
                false,
                None,
            ));
        }
        if self.policy.exclusion_for_file(&request.path).is_some() {
            return Err(content_error(
                QueryErrorCode::ContentUnavailable,
                "repository content is excluded by the source policy",
                false,
                None,
            ));
        }
        let Some(file) = self.files.get(&request.path) else {
            return Err(content_error(
                QueryErrorCode::ContentUnavailable,
                "repository content is unavailable for the selected snapshot",
                false,
                None,
            ));
        };
        if file.content_identity != request.expected_content_identity {
            return Err(content_error(
                QueryErrorCode::ContentChanged,
                "repository content identity does not match the selected snapshot",
                false,
                Some(RetrievalAction::RefreshIndex),
            ));
        }

        let content = read_descriptor_verified(&self.root, file).map_err(|error| match error {
            SourceError::ContentChanged => content_error(
                QueryErrorCode::ContentChanged,
                "repository content changed after the selected snapshot was published",
                false,
                Some(RetrievalAction::RefreshIndex),
            ),
            _ => content_error(
                QueryErrorCode::ContentUnavailable,
                "repository content could not be read through the confined source boundary",
                true,
                None,
            ),
        })?;

        content_response_for_bytes(
            request,
            &self.repository,
            &self.snapshot_id,
            file,
            &content.bytes,
            self.hard_max_bytes,
        )
    }
}

fn content_response_for_bytes(
    request: &ContentRequest,
    repository: &RepositoryRef,
    snapshot_id: &SnapshotId,
    file: &SourceFileDescriptor,
    bytes: &[u8],
    hard_max_bytes: NonZeroU64,
) -> Result<ContentResponse, QueryError> {
    let (start, end) = request
        .span
        .as_ref()
        .map_or((0_u64, bytes.len() as u64), |span| {
            (span.start.byte_offset, span.end.byte_offset)
        });
    let Ok(start) = usize::try_from(start) else {
        return Err(invalid_content_span());
    };
    let Ok(end) = usize::try_from(end) else {
        return Err(invalid_content_span());
    };
    if start > end || end > bytes.len() {
        return Err(invalid_content_span());
    }
    let limit = request.max_bytes.get().min(hard_max_bytes.get());
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    let selected = &bytes[start..end];
    let returned_len = clamp_utf8_truncation(selected, selected.len().min(limit));

    Ok(ContentResponse {
        wire_version: super::QUERY_WIRE_VERSION,
        repository: repository.clone(),
        snapshot_id: snapshot_id.clone(),
        path: request.path.clone(),
        verified_content_identity: file.content_identity.clone(),
        bytes: selected[..returned_len].to_vec(),
        truncated: returned_len < selected.len(),
    })
}

fn clamp_utf8_truncation(bytes: &[u8], requested_len: usize) -> usize {
    let requested_len = requested_len.min(bytes.len());
    let Ok(text) = std::str::from_utf8(bytes) else {
        // Preserve invalid source bytes so the snippet adapter can report the
        // existing content.non_utf8 diagnostic instead of hiding corruption.
        return requested_len;
    };
    let mut returned_len = requested_len;
    while !text.is_char_boundary(returned_len) {
        returned_len -= 1;
    }
    returned_len
}

fn invalid_content_span() -> QueryError {
    content_error(
        QueryErrorCode::InvalidRequest,
        "repository content span is outside the verified source bytes",
        false,
        None,
    )
}

fn content_error(
    code: QueryErrorCode,
    message: &str,
    retryable: bool,
    recommended_action: Option<RetrievalAction>,
) -> QueryError {
    QueryError {
        wire_version: super::QUERY_WIRE_VERSION,
        code,
        message: message.to_string(),
        retryable,
        recommended_action,
        details: BTreeMap::new(),
    }
}

fn same_manifest_identity(left: &SourceManifest, right: &SourceManifest) -> bool {
    left.revision == right.revision && left.extractor_set_digest == right.extractor_set_digest
}

pub(super) fn set_manifest_source_state(
    manifest: &mut SourceManifest,
    source_kind: SourceKind,
    base_revision: Option<Digest>,
    dirty: bool,
) {
    manifest.revision.id = revision_id(
        &manifest.revision.repository,
        source_kind,
        base_revision.as_ref(),
        &manifest.revision.manifest_digest,
        &manifest.revision.analysis_config_digest,
        dirty,
        manifest.revision.includes_untracked,
    );
    manifest.revision.source_kind = source_kind;
    manifest.revision.base_revision = base_revision;
    manifest.revision.dirty = dirty;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CandidateKind {
    File(Option<SourceFileMode>),
    Symlink,
    Gitlink,
    Special,
}

#[derive(Debug)]
pub(super) struct Candidate {
    pub path: RepoPath,
    pub kind: CandidateKind,
    pub untracked: bool,
}

pub(super) struct DiagnosticCollector {
    diagnostics: Vec<SourceDiagnostic>,
    max_diagnostics: u64,
    suppressed: u64,
}

impl DiagnosticCollector {
    pub fn new(max_diagnostics: u64) -> Self {
        Self {
            diagnostics: Vec::new(),
            max_diagnostics,
            suppressed: 0,
        }
    }

    pub fn push(&mut self, code: &'static str, path: Option<RepoPath>) {
        let diagnostic = SourceDiagnostic {
            code: DiagnosticCode::new(code)
                .expect("source diagnostic constants are valid bounded codes"),
            path,
        };
        let insertion = self
            .diagnostics
            .binary_search_by(|stored| {
                diagnostic_sort_key(stored).cmp(&diagnostic_sort_key(&diagnostic))
            })
            .unwrap_or_else(|index| index);
        if (self.diagnostics.len() as u64) < self.max_diagnostics {
            self.diagnostics.insert(insertion, diagnostic);
        } else if insertion < self.diagnostics.len() {
            self.diagnostics.insert(insertion, diagnostic);
            self.diagnostics.pop();
            self.suppressed = self.suppressed.saturating_add(1);
        } else {
            self.suppressed = self.suppressed.saturating_add(1);
        }
    }
}

fn diagnostic_sort_key(diagnostic: &SourceDiagnostic) -> (Option<&RepoPath>, &DiagnosticCode) {
    (diagnostic.path.as_ref(), &diagnostic.code)
}

pub(super) struct DiscoveryScan {
    pub source_kind: SourceKind,
    pub base_revision: Option<Digest>,
    pub dirty: bool,
    pub candidates: Vec<Candidate>,
    pub diagnostics: DiagnosticCollector,
    pub metrics: SourceDiscoveryMetrics,
}

pub(super) fn build_manifest(
    root: &SourceRoot,
    context: &SourceDiscoveryContext,
    scan: DiscoveryScan,
) -> Result<SourceManifest, SourceError> {
    let DiscoveryScan {
        source_kind,
        base_revision,
        dirty,
        mut candidates,
        mut diagnostics,
        mut metrics,
    } = scan;
    candidates.sort_by(|left, right| left.path.cmp(&right.path));
    if candidates
        .windows(2)
        .any(|pair| pair[0].path == pair[1].path)
    {
        return Err(SourceError::PathCollision);
    }
    #[cfg(windows)]
    {
        let mut keys = BTreeSet::new();
        for candidate in &candidates {
            let key = windows_path_key(&candidate.path).ok_or(SourceError::PathCollision)?;
            if !keys.insert(key) {
                return Err(SourceError::PathCollision);
            }
        }
    }

    let mut files = Vec::new();
    let mut includes_untracked = false;
    for candidate in candidates {
        let path = candidate.path;
        if let Some(code) = context.policy.exclusion_for_file(&path) {
            diagnostics.push(code, Some(path));
            metrics.skipped = metrics.skipped.saturating_add(1);
            continue;
        }
        if candidate.kind == CandidateKind::Special {
            diagnostics.push("special_file_skipped", Some(path));
            metrics.skipped = metrics.skipped.saturating_add(1);
            continue;
        }
        let file = match root.open_file(&path) {
            Ok(file) => file,
            Err(ConfinedOpenError::Symlink) => {
                diagnostics.push("symlink_skipped", Some(path));
                metrics.skipped = metrics.skipped.saturating_add(1);
                continue;
            }
            Err(ConfinedOpenError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                diagnostics.push("file_missing", Some(path));
                metrics.skipped = metrics.skipped.saturating_add(1);
                continue;
            }
            Err(ConfinedOpenError::Io(_)) => {
                diagnostics.push("file_unreadable", Some(path));
                metrics.skipped = metrics.skipped.saturating_add(1);
                continue;
            }
        };
        let metadata = match file.metadata() {
            Ok(metadata) => metadata,
            Err(_) => {
                diagnostics.push("file_unreadable", Some(path));
                metrics.skipped = metrics.skipped.saturating_add(1);
                continue;
            }
        };
        if !metadata.file_type().is_file() {
            let code = if candidate.kind == CandidateKind::Gitlink {
                "gitlink_skipped"
            } else {
                "special_file_skipped"
            };
            diagnostics.push(code, Some(path));
            metrics.skipped = metrics.skipped.saturating_add(1);
            continue;
        }
        if metadata.len() > context.limits.max_file_bytes {
            diagnostics.push("file_too_large", Some(path));
            metrics.skipped = metrics.skipped.saturating_add(1);
            continue;
        }
        let remaining_bytes = context
            .limits
            .max_total_bytes
            .checked_sub(metrics.total_bytes)
            .expect("inspected bytes never exceed the configured limit");
        if metadata.len() > remaining_bytes {
            return Err(SourceError::TotalBytesLimitExceeded {
                limit: context.limits.max_total_bytes,
            });
        }
        let read_limit = context.limits.max_file_bytes.min(remaining_bytes);
        let bytes = match read_bounded(file, read_limit) {
            Ok(BoundedRead::Complete(bytes)) => {
                account_inspected_bytes(
                    &mut metrics,
                    bytes.len() as u64,
                    context.limits.max_total_bytes,
                )?;
                bytes
            }
            Ok(BoundedRead::LimitExceeded { inspected }) => {
                account_inspected_bytes(&mut metrics, inspected, context.limits.max_total_bytes)?;
                if read_limit != remaining_bytes {
                    diagnostics.push("file_too_large", Some(path));
                    metrics.skipped = metrics.skipped.saturating_add(1);
                    continue;
                }
                return Err(SourceError::TotalBytesLimitExceeded {
                    limit: context.limits.max_total_bytes,
                });
            }
            Err(error) => {
                account_inspected_bytes(
                    &mut metrics,
                    error.inspected,
                    context.limits.max_total_bytes,
                )?;
                diagnostics.push("file_unreadable", Some(path));
                metrics.skipped = metrics.skipped.saturating_add(1);
                continue;
            }
        };
        let byte_len = u64::try_from(bytes.len()).expect("usize always fits into u64");
        if is_binary(&bytes) {
            diagnostics.push("binary_file_skipped", Some(path));
            metrics.skipped = metrics.skipped.saturating_add(1);
            continue;
        }
        let declared_mode = match candidate.kind {
            CandidateKind::File(mode) => mode,
            CandidateKind::Symlink | CandidateKind::Gitlink | CandidateKind::Special => None,
        };
        let mode = observed_file_mode(&metadata, declared_mode);
        if files.len() as u64 >= context.limits.max_files {
            return Err(SourceError::FileLimitExceeded {
                limit: context.limits.max_files,
            });
        }
        files.push(SourceFileDescriptor {
            path,
            content_identity: sha256_digest(&bytes),
            byte_len,
            file_mode: mode,
        });
        metrics.included = metrics.included.saturating_add(1);
        includes_untracked |= candidate.untracked;
    }

    metrics.suppressed_diagnostics = diagnostics.suppressed;
    let manifest_digest = manifest_digest(&files, &context.source_policy_digest);
    let revision = SourceRevision {
        id: revision_id(
            &context.repository,
            source_kind,
            base_revision.as_ref(),
            &manifest_digest,
            &context.analysis_config_digest,
            dirty,
            includes_untracked,
        ),
        repository: context.repository.clone(),
        source_kind,
        base_revision,
        manifest_digest,
        analysis_config_digest: context.analysis_config_digest.clone(),
        dirty,
        includes_untracked,
    };
    Ok(SourceManifest {
        revision,
        extractor_set_digest: context.extractor_set_digest.clone(),
        files,
        diagnostics: diagnostics.diagnostics,
        metrics,
    })
}

pub(super) fn read_verified(
    root: &SourceRoot,
    manifest: &SourceManifest,
    file: &SourceFileDescriptor,
) -> Result<SourceContent, SourceError> {
    let stored = manifest
        .files
        .binary_search_by(|candidate| candidate.path.cmp(&file.path))
        .ok()
        .and_then(|index| manifest.files.get(index))
        .filter(|stored| *stored == file)
        .ok_or(SourceError::FileNotInManifest)?;
    read_descriptor_verified(root, stored)
}

fn read_descriptor_verified(
    root: &SourceRoot,
    stored: &SourceFileDescriptor,
) -> Result<SourceContent, SourceError> {
    let file = match root.open_file(&stored.path) {
        Ok(file) => file,
        Err(ConfinedOpenError::Symlink) => return Err(SourceError::ContentChanged),
        Err(ConfinedOpenError::Io(source)) if source.kind() == io::ErrorKind::NotFound => {
            return Err(SourceError::ContentChanged);
        }
        Err(ConfinedOpenError::Io(source)) => {
            return Err(SourceError::Io {
                operation: "open verified content",
                source,
            });
        }
    };
    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(SourceError::ContentChanged);
        }
        Err(source) => {
            return Err(SourceError::Io {
                operation: "read verified metadata",
                source,
            });
        }
    };
    if !metadata.file_type().is_file() {
        return Err(SourceError::ContentChanged);
    }
    let bytes = match read_bounded(file, stored.byte_len) {
        Ok(BoundedRead::Complete(bytes)) => bytes,
        Ok(BoundedRead::LimitExceeded { .. }) => return Err(SourceError::ContentChanged),
        Err(error) => {
            return Err(SourceError::Io {
                operation: "read verified content",
                source: error.source,
            });
        }
    };
    if bytes.len() as u64 != stored.byte_len || sha256_digest(&bytes) != stored.content_identity {
        return Err(SourceError::ContentChanged);
    }
    #[cfg(unix)]
    if observed_file_mode(&metadata, None) != stored.file_mode {
        return Err(SourceError::ContentChanged);
    }
    Ok(SourceContent { bytes })
}

pub(super) fn normalize_discovered_path(path: &Path) -> Result<RepoPath, ()> {
    let mut normalized = String::new();
    for component in path.components() {
        let Component::Normal(component) = component else {
            return Err(());
        };
        let component = component.to_str().ok_or(())?;
        if component.contains('\\') || component.contains('\0') {
            return Err(());
        }
        if !normalized.is_empty() {
            normalized.push('/');
        }
        normalized.push_str(component);
    }
    RepoPath::new(normalized).map_err(|_| ())
}

#[cfg(any(windows, test))]
fn windows_path_key(path: &RepoPath) -> Option<String> {
    let mut key = String::new();
    for component in path.as_str().split('/') {
        if component.contains(':')
            || component.ends_with(['.', ' '])
            || windows_reserved_component(component)
        {
            return None;
        }
        if !key.is_empty() {
            key.push('/');
        }
        key.push_str(&component.to_ascii_lowercase());
    }
    Some(key)
}

#[cfg(any(windows, test))]
fn windows_reserved_component(component: &str) -> bool {
    let stem = component
        .split_once('.')
        .map_or(component, |(stem, _)| stem)
        .to_ascii_lowercase();
    matches!(
        stem.as_str(),
        "con" | "prn" | "aux" | "nul" | "conin$" | "conout$"
    ) || stem
        .strip_prefix("com")
        .or_else(|| stem.strip_prefix("lpt"))
        .is_some_and(|suffix| suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9'))
}

#[derive(Debug)]
enum BoundedRead {
    Complete(Vec<u8>),
    LimitExceeded { inspected: u64 },
}

#[derive(Debug)]
struct BoundedReadError {
    source: io::Error,
    inspected: u64,
}

fn read_bounded(reader: impl Read, max_bytes: u64) -> Result<BoundedRead, BoundedReadError> {
    let read_limit = max_bytes.saturating_add(1);
    let mut bytes = Vec::with_capacity(usize::try_from(max_bytes.min(64 * 1024)).unwrap_or(0));
    if let Err(source) = reader.take(read_limit).read_to_end(&mut bytes) {
        return Err(BoundedReadError {
            source,
            inspected: bytes.len() as u64,
        });
    }
    if bytes.len() as u64 > max_bytes {
        Ok(BoundedRead::LimitExceeded {
            inspected: bytes.len() as u64,
        })
    } else {
        Ok(BoundedRead::Complete(bytes))
    }
}

fn account_inspected_bytes(
    metrics: &mut SourceDiscoveryMetrics,
    inspected: u64,
    limit: u64,
) -> Result<(), SourceError> {
    let total = metrics
        .total_bytes
        .checked_add(inspected)
        .ok_or(SourceError::TotalBytesLimitExceeded { limit })?;
    if total > limit {
        return Err(SourceError::TotalBytesLimitExceeded { limit });
    }
    metrics.total_bytes = total;
    Ok(())
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes.contains(&0)
}

#[cfg(unix)]
fn observed_file_mode(
    metadata: &fs::Metadata,
    _declared: Option<SourceFileMode>,
) -> SourceFileMode {
    use std::os::unix::fs::PermissionsExt;

    if metadata.permissions().mode() & 0o111 == 0 {
        SourceFileMode::Regular
    } else {
        SourceFileMode::Executable
    }
}

#[cfg(not(unix))]
fn observed_file_mode(
    _metadata: &fs::Metadata,
    declared: Option<SourceFileMode>,
) -> SourceFileMode {
    declared.unwrap_or(SourceFileMode::Regular)
}

fn sha256_digest(bytes: &[u8]) -> Digest {
    Digest::new("sha256", hex_lower(&Sha256::digest(bytes)))
        .expect("sha256 output is always a canonical digest")
}

#[derive(Serialize, PartialEq, Eq, PartialOrd, Ord)]
struct CanonicalExtractorIdentity<'a> {
    id: &'a str,
    version: &'a str,
    contract_version: u32,
}

pub fn extractor_set_digest(extractors: &[ExtractorIdentity]) -> Digest {
    let canonical = extractors
        .iter()
        .map(|extractor| CanonicalExtractorIdentity {
            id: extractor.id.as_str(),
            version: &extractor.version,
            contract_version: extractor.contract_version,
        })
        .collect::<BTreeSet<_>>();
    let bytes = serde_json::to_vec(&(SOURCE_MANIFEST_VERSION, canonical))
        .expect("canonical extractor-set serialization cannot fail");
    sha256_digest(&bytes)
}

#[derive(Serialize)]
struct CanonicalManifest<'a> {
    version: u32,
    source_policy_version: u32,
    source_policy_digest: &'a Digest,
    files: &'a [SourceFileDescriptor],
}

pub(super) fn manifest_digest(
    files: &[SourceFileDescriptor],
    source_policy_digest: &Digest,
) -> Digest {
    let canonical = CanonicalManifest {
        version: SOURCE_MANIFEST_VERSION,
        source_policy_version: SOURCE_POLICY_VERSION,
        source_policy_digest,
        files,
    };
    let bytes = serde_json::to_vec(&canonical)
        .expect("canonical source manifest serialization cannot fail");
    sha256_digest(&bytes)
}

#[derive(Serialize)]
struct CanonicalRevision<'a> {
    version: u32,
    repository: &'a RepositoryRef,
    source_kind: SourceKind,
    base_revision: Option<&'a Digest>,
    manifest_digest: &'a Digest,
    analysis_config_digest: &'a Digest,
    dirty: bool,
    includes_untracked: bool,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn revision_id(
    repository: &RepositoryRef,
    source_kind: SourceKind,
    base_revision: Option<&Digest>,
    manifest_digest: &Digest,
    analysis_config_digest: &Digest,
    dirty: bool,
    includes_untracked: bool,
) -> SourceRevisionId {
    let canonical = CanonicalRevision {
        version: SOURCE_MANIFEST_VERSION,
        repository,
        source_kind,
        base_revision,
        manifest_digest,
        analysis_config_digest,
        dirty,
        includes_untracked,
    };
    let bytes = serde_json::to_vec(&canonical)
        .expect("canonical source revision serialization cannot fail");
    let digest = sha256_digest(&bytes);
    SourceRevisionId::new(format!("sha256:{}", digest.value()))
        .expect("derived source revision identity is never empty")
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

#[derive(Debug, Clone)]
struct SourcePolicy {
    include: Vec<String>,
    rules: Vec<(bool, String)>,
    sensitive: Vec<String>,
    include_untracked: bool,
    include_generated: bool,
    include_vendor: bool,
    has_negated_rules: bool,
}

impl SourcePolicy {
    fn new(config: &SourceConfig) -> Result<Self, SourceError> {
        let include = config
            .include
            .iter()
            .map(|pattern| canonical_pattern_body(pattern).map_err(SourceError::from))
            .collect::<Result<Vec<_>, _>>()?;
        let rules = config
            .rules
            .iter()
            .map(|pattern| {
                let (negated, body) = pattern
                    .trim()
                    .strip_prefix('!')
                    .map_or((false, pattern.as_str()), |body| (true, body));
                Ok((negated, canonical_pattern_body(body)?))
            })
            .collect::<Result<Vec<_>, SourceError>>()?;
        let sensitive = config
            .sensitive
            .iter()
            .map(|pattern| canonical_pattern_body(pattern).map_err(SourceError::from))
            .collect::<Result<Vec<_>, _>>()?;
        let has_negated_rules = rules.iter().any(|(negated, _)| *negated);
        Ok(Self {
            include,
            rules,
            sensitive,
            include_untracked: config.include_untracked,
            include_generated: config.include_generated,
            include_vendor: config.include_vendor,
            has_negated_rules,
        })
    }

    fn exclusion_for_file(&self, path: &RepoPath) -> Option<&'static str> {
        if hard_excluded(path) {
            return Some("runtime_path_excluded");
        }
        if self
            .sensitive
            .iter()
            .any(|pattern| sensitive_glob_matches(pattern, path.as_str()))
        {
            return Some("sensitive_path_excluded");
        }
        if !self.include_vendor && is_vendor(path) {
            return Some("vendor_path_excluded");
        }
        if !self.include_generated && is_generated(path) {
            return Some("generated_path_excluded");
        }
        if !self
            .include
            .iter()
            .any(|pattern| glob_matches(pattern, path.as_str()))
        {
            return Some("path_not_included");
        }
        let mut excluded = false;
        for (negated, pattern) in &self.rules {
            if glob_matches(pattern, path.as_str()) {
                excluded = !negated;
            }
        }
        excluded.then_some("source_rule_excluded")
    }

    fn exclusion_for_directory(&self, path: &RepoPath) -> Option<&'static str> {
        if hard_excluded(path) {
            return Some("runtime_path_excluded");
        }
        if self
            .sensitive
            .iter()
            .any(|pattern| sensitive_glob_matches(pattern, path.as_str()))
        {
            return Some("sensitive_path_excluded");
        }
        if !self.include_vendor && is_vendor(path) {
            return Some("vendor_path_excluded");
        }
        if !self.include_generated && is_generated(path) {
            return Some("generated_path_excluded");
        }
        if !self
            .include
            .iter()
            .any(|pattern| glob_may_match_descendant(pattern, path.as_str()))
        {
            return Some("path_not_included");
        }
        if self.has_negated_rules {
            return None;
        }
        self.rules
            .iter()
            .rev()
            .find(|(_, pattern)| glob_matches(pattern, path.as_str()))
            .and_then(|(negated, _)| (!negated).then_some("source_rule_excluded"))
    }
}

fn hard_excluded(path: &RepoPath) -> bool {
    path.as_str().split('/').any(|component| {
        component.eq_ignore_ascii_case(".git") || component.eq_ignore_ascii_case(".ferrus")
    })
}

fn is_vendor(path: &RepoPath) -> bool {
    path.as_str().split('/').any(|component| {
        ["vendor", "node_modules", "third_party"]
            .iter()
            .any(|name| component.eq_ignore_ascii_case(name))
    })
}

fn is_generated(path: &RepoPath) -> bool {
    path.as_str().split('/').any(|component| {
        [
            "target",
            "dist",
            "build",
            "out",
            "coverage",
            ".next",
            "generated",
        ]
        .iter()
        .any(|name| component.eq_ignore_ascii_case(name))
    })
}

fn glob_matches(pattern: &str, path: &str) -> bool {
    let pattern_components = pattern.split('/').collect::<Vec<_>>();
    let path_components = path.split('/').collect::<Vec<_>>();
    if pattern_components.len() == 1 {
        return path_components
            .iter()
            .any(|component| component_matches(pattern_components[0], component));
    }
    components_match(&pattern_components, &path_components)
}

fn glob_may_match_descendant(pattern: &str, directory: &str) -> bool {
    let pattern_components = pattern.split('/').collect::<Vec<_>>();
    if pattern_components.len() == 1 {
        return true;
    }
    let directory_components = directory.split('/').collect::<Vec<_>>();
    let mut reachable = BTreeSet::from([(0_usize, 0_usize)]);
    let mut visited = BTreeSet::new();
    while let Some(state) = reachable.pop_first() {
        if !visited.insert(state) {
            continue;
        }
        let (pattern_index, directory_index) = state;
        if directory_index == directory_components.len() && pattern_index < pattern_components.len()
        {
            return true;
        }
        let Some(component) = pattern_components.get(pattern_index) else {
            continue;
        };
        if *component == "**" {
            reachable.insert((pattern_index + 1, directory_index));
            if directory_index < directory_components.len() {
                reachable.insert((pattern_index, directory_index + 1));
            }
        } else if let Some(directory_component) = directory_components.get(directory_index)
            && component_matches(component, directory_component)
        {
            reachable.insert((pattern_index + 1, directory_index + 1));
        }
    }
    false
}

fn sensitive_glob_matches(pattern: &str, path: &str) -> bool {
    glob_matches(&pattern.to_ascii_lowercase(), &path.to_ascii_lowercase())
}

fn components_match(pattern: &[&str], path: &[&str]) -> bool {
    let mut reachable = BTreeSet::from([(0_usize, 0_usize)]);
    let mut visited = BTreeSet::new();
    while let Some(state) = reachable.pop_first() {
        if !visited.insert(state) {
            continue;
        }
        let (pattern_index, path_index) = state;
        if pattern_index == pattern.len() && path_index == path.len() {
            return true;
        }
        let Some(component) = pattern.get(pattern_index) else {
            continue;
        };
        if *component == "**" {
            reachable.insert((pattern_index + 1, path_index));
            if path_index < path.len() {
                reachable.insert((pattern_index, path_index + 1));
            }
        } else if let Some(path_component) = path.get(path_index)
            && component_matches(component, path_component)
        {
            reachable.insert((pattern_index + 1, path_index + 1));
        }
    }
    false
}

fn component_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.chars().collect::<Vec<_>>();
    let value = value.chars().collect::<Vec<_>>();
    let mut reachable = BTreeSet::from([(0_usize, 0_usize)]);
    let mut visited = BTreeSet::new();
    while let Some(state) = reachable.pop_first() {
        if !visited.insert(state) {
            continue;
        }
        let (pattern_index, value_index) = state;
        if pattern_index == pattern.len() && value_index == value.len() {
            return true;
        }
        match pattern.get(pattern_index) {
            Some('*') => {
                reachable.insert((pattern_index + 1, value_index));
                if value_index < value.len() {
                    reachable.insert((pattern_index, value_index + 1));
                }
            }
            Some('?') if value_index < value.len() => {
                reachable.insert((pattern_index + 1, value_index + 1));
            }
            Some(expected)
                if value
                    .get(value_index)
                    .is_some_and(|actual| actual == expected) =>
            {
                reachable.insert((pattern_index + 1, value_index + 1));
            }
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository_graph::domain::{
        RepositoryId, RepositoryNamespace, SourcePosition, SourceSpan,
    };

    fn test_repository() -> RepositoryRef {
        RepositoryRef {
            namespace: RepositoryNamespace::new("local:test").unwrap(),
            repository_id: RepositoryId::new("root").unwrap(),
        }
    }

    fn descriptor(path: &str, bytes: &[u8]) -> SourceFileDescriptor {
        SourceFileDescriptor {
            path: RepoPath::new(path).unwrap(),
            content_identity: sha256_digest(bytes),
            byte_len: bytes.len() as u64,
            file_mode: SourceFileMode::Regular,
        }
    }

    fn content_request(file: &SourceFileDescriptor) -> ContentRequest {
        ContentRequest {
            wire_version: super::super::QUERY_WIRE_VERSION,
            repository: test_repository(),
            snapshot_id: SnapshotId::new("snapshot-1").unwrap(),
            path: file.path.clone(),
            expected_content_identity: file.content_identity.clone(),
            span: None,
            max_bytes: NonZeroU64::new(1024).unwrap(),
        }
    }

    #[test]
    fn globstar_matches_root_and_nested_paths() {
        assert!(glob_matches("**/*", "Cargo.toml"));
        assert!(glob_matches("**/*", "src/main.rs"));
        assert!(glob_matches("**/.env", ".env"));
        assert!(glob_matches("**/.env", "nested/.env"));
        assert!(!glob_matches("src/**", "Cargo.toml"));
    }

    #[test]
    fn include_globs_identify_only_directories_with_possible_descendants() {
        assert!(glob_may_match_descendant("src/**", "src"));
        assert!(glob_may_match_descendant("src/**", "src/nested"));
        assert!(!glob_may_match_descendant("src/**", "docs"));
        assert!(glob_may_match_descendant(
            "crates/*/Cargo.toml",
            "crates/example"
        ));
        assert!(!glob_may_match_descendant(
            "crates/*/Cargo.toml",
            "crates/example/nested"
        ));
        assert!(glob_may_match_descendant("Cargo.toml", "any/nesting"));
        assert!(glob_may_match_descendant("**/src/**", "docs"));
    }

    #[test]
    fn ordered_rules_use_the_last_matching_rule() {
        let config = SourceConfig {
            rules: vec!["src/**".to_string(), "!src/keep.rs".to_string()],
            ..SourceConfig::default()
        };
        let policy = SourcePolicy::new(&config).unwrap();
        assert_eq!(
            policy.exclusion_for_file(&RepoPath::new("src/drop.rs").unwrap()),
            Some("source_rule_excluded")
        );
        assert_eq!(
            policy.exclusion_for_file(&RepoPath::new("src/keep.rs").unwrap()),
            None
        );
    }

    #[test]
    fn extractor_set_identity_is_order_independent() {
        let rust = ExtractorIdentity {
            id: crate::repository_graph::domain::ExtractorId::new("rust").unwrap(),
            version: "1.0.0".to_string(),
            contract_version: 1,
        };
        let cargo = ExtractorIdentity {
            id: crate::repository_graph::domain::ExtractorId::new("cargo").unwrap(),
            version: "2.0.0".to_string(),
            contract_version: 1,
        };
        assert_eq!(
            extractor_set_digest(&[rust.clone(), cargo.clone()]),
            extractor_set_digest(&[cargo, rust])
        );
    }

    #[test]
    fn windows_path_keys_reject_aliases_and_reserved_names() {
        let upper = RepoPath::new("src/Foo.rs").unwrap();
        let lower = RepoPath::new("src/foo.rs").unwrap();
        assert_eq!(windows_path_key(&upper), windows_path_key(&lower));
        for path in [
            "CON",
            "aux.txt",
            "src/name.",
            "src/name ",
            "src/file:stream",
        ] {
            assert!(windows_path_key(&RepoPath::new(path).unwrap()).is_none());
        }
    }

    #[test]
    fn windows_final_path_prefixes_compare_with_canonical_paths() {
        assert!(windows_paths_equal(Path::new(r"C:\repo"), r"\\?\C:\repo"));
        assert!(windows_paths_equal(
            Path::new(r"\\server\share\repo"),
            r"\\?\UNC\server\share\repo"
        ));
        assert!(windows_paths_equal(Path::new("C:/Repo/"), r"\\?\c:\repo"));
        assert!(!windows_paths_equal(Path::new(r"C:\repo"), r"\\?\C:\other"));
    }

    #[test]
    fn bounded_reads_report_bytes_consumed_before_an_io_error() {
        struct PartialThenError(bool);

        impl Read for PartialThenError {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                if self.0 {
                    return Err(io::Error::other("injected read failure"));
                }
                self.0 = true;
                buffer[..3].copy_from_slice(b"abc");
                Ok(3)
            }
        }

        let error = read_bounded(PartialThenError(false), 10).unwrap_err();
        assert_eq!(error.inspected, 3);
        let mut metrics = SourceDiscoveryMetrics {
            total_bytes: 4,
            ..SourceDiscoveryMetrics::default()
        };
        assert!(matches!(
            account_inspected_bytes(&mut metrics, error.inspected, 6),
            Err(SourceError::TotalBytesLimitExceeded { limit: 6 })
        ));
    }

    #[test]
    fn snapshot_content_confines_hash_verifies_and_bounds_spans() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join("src")).unwrap();
        let bytes = b"abcdef";
        std::fs::write(directory.path().join("src/lib.rs"), bytes).unwrap();
        let file = descriptor("src/lib.rs", bytes);
        let reader = LocalSnapshotContent::new(
            directory.path(),
            test_repository(),
            SnapshotId::new("snapshot-1").unwrap(),
            &SourceConfig::default(),
            vec![file.clone()],
            NonZeroU64::new(3).unwrap(),
        )
        .unwrap();
        let mut request = content_request(&file);
        request.span = Some(SourceSpan {
            start: SourcePosition {
                byte_offset: 1,
                line: Some(1),
                column: Some(2),
            },
            end: SourcePosition {
                byte_offset: 5,
                line: Some(1),
                column: Some(6),
            },
        });

        let response = reader.read_verified(&request).unwrap();
        assert_eq!(response.bytes, b"bcd");
        assert!(response.truncated);

        std::fs::write(directory.path().join("src/lib.rs"), b"changed").unwrap();
        assert_eq!(
            reader.read_verified(&request).unwrap_err().code,
            QueryErrorCode::ContentChanged
        );
    }

    #[test]
    fn snapshot_content_clamps_byte_limits_to_utf8_boundaries() {
        let directory = tempfile::tempdir().unwrap();
        let bytes = "aéz".as_bytes();
        std::fs::write(directory.path().join("unicode.rs"), bytes).unwrap();
        let file = descriptor("unicode.rs", bytes);

        for (hard_limit, request_limit) in [(2, 1024), (1024, 2)] {
            let reader = LocalSnapshotContent::new(
                directory.path(),
                test_repository(),
                SnapshotId::new("snapshot-1").unwrap(),
                &SourceConfig::default(),
                vec![file.clone()],
                NonZeroU64::new(hard_limit).unwrap(),
            )
            .unwrap();
            let mut request = content_request(&file);
            request.max_bytes = NonZeroU64::new(request_limit).unwrap();

            let response = reader.read_verified(&request).unwrap();

            assert_eq!(std::str::from_utf8(&response.bytes).unwrap(), "a");
            assert!(response.truncated);
        }
    }

    #[test]
    fn snapshot_content_denies_sensitive_paths_before_reading() {
        let directory = tempfile::tempdir().unwrap();
        let file = descriptor(".env", b"SECRET=value");
        std::fs::write(directory.path().join(".env"), b"SECRET=value").unwrap();
        let reader = LocalSnapshotContent::new(
            directory.path(),
            test_repository(),
            SnapshotId::new("snapshot-1").unwrap(),
            &SourceConfig::default(),
            vec![file.clone()],
            NonZeroU64::new(1024).unwrap(),
        )
        .unwrap();

        assert_eq!(
            reader
                .read_verified(&content_request(&file))
                .unwrap_err()
                .code,
            QueryErrorCode::ContentUnavailable
        );
    }

    #[test]
    fn frozen_git_tree_content_survives_worktree_changes() {
        let directory = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .arg("init")
            .arg("--quiet")
            .arg(directory.path())
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::create_dir_all(directory.path().join("src")).unwrap();
        let original = b"pub struct Submitted;\n";
        std::fs::write(directory.path().join("src/lib.rs"), original).unwrap();
        let tree = capture_worktree_tree(directory.path()).unwrap();
        let file = descriptor("src/lib.rs", original);
        let reader = GitTreeSnapshotContent::new(
            directory.path(),
            test_repository(),
            SnapshotId::new("snapshot-1").unwrap(),
            tree,
            &SourceConfig::default(),
            vec![file.clone()],
            NonZeroU64::new(1024).unwrap(),
        )
        .unwrap();

        std::fs::write(
            directory.path().join("src/lib.rs"),
            b"pub struct Addressing;\n",
        )
        .unwrap();

        let response = reader.read_verified(&content_request(&file)).unwrap();
        assert_eq!(response.bytes, original);
    }

    #[test]
    fn task_baseline_discovery_and_reads_ignore_worktree_changes() {
        let directory = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .arg("init")
            .arg("--quiet")
            .arg(directory.path())
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::create_dir_all(directory.path().join("src")).unwrap();
        let original = b"pub struct Baseline;\n";
        std::fs::write(directory.path().join("src/lib.rs"), original).unwrap();
        let tree = capture_worktree_tree(directory.path()).unwrap();
        let context = SourceDiscoveryContext::from_config(
            test_repository(),
            &RepositoryGraphConfig::default(),
            &[],
        )
        .unwrap();

        std::fs::write(
            directory.path().join("src/lib.rs"),
            b"pub struct ExecutorEdit;\n",
        )
        .unwrap();
        let source = TaskBaselineSource::discover(directory.path(), context, tree).unwrap();
        let file = source
            .manifest()
            .files
            .iter()
            .find(|file| file.path.as_str() == "src/lib.rs")
            .unwrap();

        assert_eq!(source.read_verified(file).unwrap().bytes, original);
        assert!(source.revalidate().unwrap());
        assert_eq!(
            source.manifest().revision.source_kind,
            SourceKind::TaskBaseline
        );
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_content_rejects_symlink_replacement() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), b"outside").unwrap();
        let file = descriptor("link.rs", b"outside");
        symlink(outside.path(), directory.path().join("link.rs")).unwrap();
        let reader = LocalSnapshotContent::new(
            directory.path(),
            test_repository(),
            SnapshotId::new("snapshot-1").unwrap(),
            &SourceConfig::default(),
            vec![file.clone()],
            NonZeroU64::new(1024).unwrap(),
        )
        .unwrap();

        assert_eq!(
            reader
                .read_verified(&content_request(&file))
                .unwrap_err()
                .code,
            QueryErrorCode::ContentChanged
        );
    }
}
