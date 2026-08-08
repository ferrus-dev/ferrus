use super::*;

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
    pub(super) diagnostics: Vec<SourceDiagnostic>,
    max_diagnostics: u64,
    pub(super) suppressed: u64,
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

pub(super) fn diagnostic_sort_key(
    diagnostic: &SourceDiagnostic,
) -> (Option<&RepoPath>, &DiagnosticCode) {
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

pub(super) fn read_descriptor_verified(
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
pub(super) fn windows_path_key(path: &RepoPath) -> Option<String> {
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
pub(super) fn windows_reserved_component(component: &str) -> bool {
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
pub(super) enum BoundedRead {
    Complete(Vec<u8>),
    LimitExceeded { inspected: u64 },
}

#[derive(Debug)]
pub(super) struct BoundedReadError {
    source: io::Error,
    pub(super) inspected: u64,
}

pub(super) fn read_bounded(
    reader: impl Read,
    max_bytes: u64,
) -> Result<BoundedRead, BoundedReadError> {
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

pub(super) fn account_inspected_bytes(
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

pub(super) fn is_binary(bytes: &[u8]) -> bool {
    bytes.contains(&0)
}

#[cfg(unix)]
pub(super) fn observed_file_mode(
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
pub(super) fn observed_file_mode(
    _metadata: &fs::Metadata,
    declared: Option<SourceFileMode>,
) -> SourceFileMode {
    declared.unwrap_or(SourceFileMode::Regular)
}

pub(super) fn sha256_digest(bytes: &[u8]) -> Digest {
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

pub(super) fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}
