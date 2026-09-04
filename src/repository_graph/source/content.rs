//! Serve source snippets only after verifying content against the requested snapshot manifest.

use super::*;

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
        if request.wire_version != super::super::QUERY_WIRE_VERSION {
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
        if request.wire_version != super::super::QUERY_WIRE_VERSION {
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

// Callers verify the entire blob first; slicing before verification would leave
// a snippet detached from the snapshot's whole-file content identity.
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
        wire_version: super::super::QUERY_WIRE_VERSION,
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
        wire_version: super::super::QUERY_WIRE_VERSION,
        code,
        message: message.to_string(),
        retryable,
        recommended_action,
        details: BTreeMap::new(),
    }
}

pub(super) fn same_manifest_identity(left: &SourceManifest, right: &SourceManifest) -> bool {
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
