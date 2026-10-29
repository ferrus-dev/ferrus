//! Privacy-filtered immutable repository and memory source packaging.

use std::{collections::BTreeMap, num::NonZeroU64};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::{
    project_memory::{
        documents::{
            ArchiveSourceDocument, RuntimeSourceDocument, parse_spec_memory, sanitized_spec_source,
        },
        domain::{
            AuthorizedSourceDescriptor, AuthorizedSourceManifest, MemorySourceCategory,
            MemorySourceLocator, ProjectRef,
        },
        extractors::canonical_digest as memory_canonical_digest,
        policy::{MemoryContentAccess, MemoryPolicy, MemorySourceSensitivity},
        ports::MemorySource,
    },
    repository_graph::{
        domain::{DiagnosticCode, Digest, RepoPath, SourceKind, SourceRevision},
        index::{snapshot_identity, snapshot_identity_from_revision},
        ports::{
            RepositorySource, SourceFileDescriptor, SourceFileMode,
            canonical_source_manifest_digest,
        },
    },
};

pub const REMOTE_REPOSITORY_SOURCE_POLICY_VERSION: u32 = 1;

use super::{
    DISTRIBUTED_SOURCE_MANIFEST_VERSION,
    identity::{
        MemoryManifestId, MemoryManifestRef, ObjectId, RemoteProjectRef, RemoteRepositoryRef,
        RepositoryManifestId, RepositoryManifestRef, TenantObjectRef,
    },
    object_store::{ObjectStoreProtection, TenantObjectStore, VerifiedObject},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryFileRole {
    Source,
    Manifest,
    Documentation,
    Configuration,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositorySourceObject {
    pub path: RepoPath,
    pub content_identity: Digest,
    pub byte_len: u64,
    pub file_mode: SourceFileMode,
    pub file_role: RepositoryFileRole,
    pub object: TenantObjectRef,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackagingSummary {
    pub included_objects: u64,
    pub total_bytes: u64,
    /// Paths are deliberately omitted because excluded paths may be sensitive.
    pub source_diagnostic_codes: BTreeMap<DiagnosticCode, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositorySourceManifest {
    pub reference: RepositoryManifestRef,
    pub body: RepositorySourceManifestBody,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositorySourceManifestBody {
    pub protocol_version: u32,
    pub repository: RemoteRepositoryRef,
    pub source_policy_digest: Digest,
    pub source_revision: SourceRevision,
    pub extractor_set_digest: Digest,
    pub policy_schema_version: u32,
    pub files: Vec<RepositorySourceObject>,
    pub summary: PackagingSummary,
}

impl RepositorySourceManifest {
    pub fn validate<SourceError, StoreError>(
        &self,
    ) -> Result<(), PackagingError<SourceError, StoreError>> {
        let source_files = self
            .body
            .files
            .iter()
            .map(|file| SourceFileDescriptor {
                path: file.path.clone(),
                content_identity: file.content_identity.clone(),
                byte_len: file.byte_len,
                file_mode: file.file_mode,
            })
            .collect::<Vec<_>>();
        if self.body.protocol_version != DISTRIBUTED_SOURCE_MANIFEST_VERSION
            || self.body.policy_schema_version != REMOTE_REPOSITORY_SOURCE_POLICY_VERSION
            || !is_canonical_committed_revision(&self.body.source_revision)
            || self.body.repository != self.reference.repository
            || self.body.source_revision.repository != self.reference.repository_identity
            || self.body.source_policy_digest != self.reference.source_policy_digest
            || canonical_source_manifest_digest(&source_files, &self.body.source_policy_digest)
                .map_err(|_| PackagingError::InvalidManifest)?
                != self.body.source_revision.manifest_digest
            || snapshot_identity_from_revision(
                &self.body.source_revision,
                &self.body.extractor_set_digest,
            )
            .map_err(|_| PackagingError::InvalidManifest)?
                != self.reference.expected_snapshot_id
            || !self
                .body
                .files
                .windows(2)
                .all(|pair| pair[0].path < pair[1].path)
            || self.body.files.iter().any(|file| {
                file.object.project != self.reference.repository.project
                    || file.object.content_identity != file.content_identity
            })
            || self.body.summary.included_objects != self.body.files.len() as u64
            || self.body.summary.total_bytes
                != self
                    .body
                    .files
                    .iter()
                    .fold(0u64, |total, file| total.saturating_add(file.byte_len))
        {
            return Err(PackagingError::InvalidManifest);
        }

        let digest = hash_manifest(&self.body)?;
        if digest != self.reference.manifest_digest
            || self.reference.manifest_id.as_str() != digest.value()
            || self.reference.manifest_object.project != self.reference.repository.project
            || self.reference.manifest_object.content_identity != digest
            || self.reference.manifest_object.object_id.as_str() != digest.value()
        {
            return Err(PackagingError::InvalidManifest);
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemorySourceObject {
    pub category: MemorySourceCategory,
    pub locator: MemorySourceLocator,
    pub source_fingerprint: Digest,
    pub sanitized_byte_len: u64,
    pub sensitivity: MemorySourceSensitivity,
    pub content_access: MemoryContentAccess,
    pub object: TenantObjectRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemorySourceManifest {
    pub reference: MemoryManifestRef,
    pub body: MemorySourceManifestBody,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemorySourceManifestBody {
    pub protocol_version: u32,
    pub project: RemoteProjectRef,
    pub memory_policy_digest: Digest,
    pub project_identity: ProjectRef,
    pub source_set_digest: Digest,
    pub extractor_set_digest: Digest,
    pub policy_schema_version: u32,
    pub sources: Vec<MemorySourceObject>,
    pub summary: PackagingSummary,
}

impl MemorySourceManifest {
    pub fn validate<SourceError, StoreError>(
        &self,
    ) -> Result<(), PackagingError<SourceError, StoreError>> {
        let approved_policy = MemoryPolicy::default();
        let authorized_manifest = AuthorizedSourceManifest {
            project: self.body.project_identity.clone(),
            policy_digest: self.body.memory_policy_digest.clone(),
            source_set_digest: self.body.source_set_digest.clone(),
            extractor_set_digest: self.body.extractor_set_digest.clone(),
            sources: self
                .body
                .sources
                .iter()
                .map(|source| AuthorizedSourceDescriptor {
                    project: self.body.project_identity.clone(),
                    category: source.category,
                    locator: source.locator.clone(),
                    fingerprint: source.source_fingerprint.clone(),
                    byte_len: source.sanitized_byte_len,
                })
                .collect(),
        };
        let expected_revision_id = authorized_manifest
            .revision_id()
            .map_err(|_| PackagingError::InvalidManifest)?;
        if self.body.protocol_version != DISTRIBUTED_SOURCE_MANIFEST_VERSION
            || self.body.policy_schema_version
                != crate::project_memory::policy::MEMORY_POLICY_SCHEMA_VERSION
            || self.body.project != self.reference.project
            || self.body.project_identity != self.reference.project_identity
            || self.body.memory_policy_digest != self.reference.memory_policy_digest
            || expected_revision_id != self.reference.expected_revision_id
            || self.body.memory_policy_digest
                != approved_policy
                    .digest()
                    .map_err(|_| PackagingError::InvalidManifest)?
            || self.body.sources.iter().any(|source| {
                source.object.project != self.reference.project
                    || !is_remote_memory_category(source.category)
                    || !approved_policy
                        .category(source.category)
                        .is_some_and(|policy| {
                            policy.enabled
                                && source.sensitivity == policy.sensitivity
                                && source.content_access == policy.content_access
                        })
            })
            || !self
                .body
                .sources
                .windows(2)
                .map(|pair| {
                    Ok(memory_source_identity(&pair[0])? < memory_source_identity(&pair[1])?)
                })
                .collect::<Result<Vec<_>, serde_json::Error>>()
                .map_err(|_| PackagingError::InvalidManifest)?
                .into_iter()
                .all(|ordered| ordered)
            || self.body.summary.included_objects != self.body.sources.len() as u64
            || self.body.summary.total_bytes
                != self.body.sources.iter().fold(0u64, |total, source| {
                    total.saturating_add(source.sanitized_byte_len)
                })
        {
            return Err(PackagingError::InvalidManifest);
        }

        let digest = hash_manifest(&self.body)?;
        if digest != self.reference.manifest_digest
            || self.reference.manifest_id.as_str() != digest.value()
            || self.reference.manifest_object.project != self.reference.project
            || self.reference.manifest_object.content_identity != digest
            || self.reference.manifest_object.object_id.as_str() != digest.value()
        {
            return Err(PackagingError::InvalidManifest);
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackagingLimits {
    pub max_objects: NonZeroU64,
    pub max_total_bytes: NonZeroU64,
    pub max_diagnostics: NonZeroU64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryPackagingPolicy {
    pub schema_version: u32,
    pub source_policy_digest: Digest,
}

#[derive(Debug, Error)]
pub enum PackagingError<SourceError, StoreError> {
    #[error("source packaging requires authenticated transport and encryption at rest")]
    InsecureObjectStore,
    #[error("source manifest or policy is invalid")]
    InvalidManifest,
    #[error("source category is not authorized for remote project memory")]
    UnauthorizedMemoryCategory,
    #[error("remote prototype accepts only canonical repository snapshots")]
    NonCanonicalRepositorySource,
    #[error("source content changed or failed identity verification")]
    ContentIdentityMismatch,
    #[error("source object-count budget exceeded")]
    ObjectLimitExceeded,
    #[error("source byte budget exceeded")]
    ByteLimitExceeded,
    #[error("source diagnostic budget exceeded")]
    DiagnosticLimitExceeded,
    #[error("source adapter failed during verified packaging")]
    Source(#[source] SourceError),
    #[error("tenant object store rejected source content")]
    Store(#[source] StoreError),
}

pub fn package_repository_source<S, O>(
    source: &S,
    repository: RemoteRepositoryRef,
    policy: RepositoryPackagingPolicy,
    limits: PackagingLimits,
    store: &mut O,
) -> Result<RepositorySourceManifest, PackagingError<S::Error, O::Error>>
where
    S: RepositorySource,
    O: TenantObjectStore,
{
    require_protection(store.protection())?;
    let local = source.manifest();
    if policy.schema_version != REMOTE_REPOSITORY_SOURCE_POLICY_VERSION {
        return Err(PackagingError::InvalidManifest);
    }

    if !is_canonical_committed_revision(&local.revision) {
        return Err(PackagingError::NonCanonicalRepositorySource);
    }

    if (local.files.len() as u64).saturating_add(1) > limits.max_objects.get() {
        return Err(PackagingError::ObjectLimitExceeded);
    }

    if local.diagnostics.len() as u64 > limits.max_diagnostics.get() {
        return Err(PackagingError::DiagnosticLimitExceeded);
    }

    let declared_bytes = local
        .files
        .iter()
        .try_fold(0u64, |total, file| total.checked_add(file.byte_len))
        .ok_or(PackagingError::ByteLimitExceeded)?;
    if declared_bytes > limits.max_total_bytes.get() {
        return Err(PackagingError::ByteLimitExceeded);
    }

    if !local
        .files
        .windows(2)
        .all(|pair| pair[0].path < pair[1].path)
    {
        return Err(PackagingError::InvalidManifest);
    }

    let mut summary = diagnostic_summary(local);
    let mut files = Vec::with_capacity(local.files.len());
    let mut staged_objects = Vec::with_capacity(local.files.len().saturating_add(1));
    for file in &local.files {
        let content = source.read_verified(file).map_err(PackagingError::Source)?;
        verify_descriptor(file, &content.bytes)?;
        let object = tenant_object_ref(&repository.project, &file.content_identity)?;
        account_put(&mut summary, file.byte_len);
        files.push(RepositorySourceObject {
            path: file.path.clone(),
            content_identity: file.content_identity.clone(),
            byte_len: file.byte_len,
            file_mode: file.file_mode,
            file_role: repository_file_role(&file.path),
            object,
        });
        staged_objects.push((file.content_identity.clone(), content.bytes));
    }

    if !source.revalidate().map_err(PackagingError::Source)? {
        return Err(PackagingError::ContentIdentityMismatch);
    }

    let expected_snapshot_id =
        snapshot_identity(local).map_err(|_| PackagingError::InvalidManifest)?;
    let body = RepositorySourceManifestBody {
        protocol_version: DISTRIBUTED_SOURCE_MANIFEST_VERSION,
        repository: repository.clone(),
        source_policy_digest: policy.source_policy_digest.clone(),
        source_revision: local.revision.clone(),
        extractor_set_digest: local.extractor_set_digest.clone(),
        policy_schema_version: policy.schema_version,
        files,
        summary,
    };
    let manifest_bytes = serde_json::to_vec(&body).map_err(|_| PackagingError::InvalidManifest)?;
    if declared_bytes.saturating_add(manifest_bytes.len() as u64) > limits.max_total_bytes.get() {
        return Err(PackagingError::ByteLimitExceeded);
    }

    let manifest_digest = sha256(&manifest_bytes);
    let manifest_object = tenant_object_ref(&repository.project, &manifest_digest)?;
    let reference = RepositoryManifestRef {
        repository,
        repository_identity: local.revision.repository.clone(),
        manifest_id: RepositoryManifestId::new(manifest_digest.value())
            .map_err(|_| PackagingError::InvalidManifest)?,
        manifest_digest: manifest_digest.clone(),
        source_policy_digest: policy.source_policy_digest,
        expected_snapshot_id,
        manifest_object,
    };
    let manifest = RepositorySourceManifest { reference, body };
    manifest.validate()?;
    staged_objects.push((manifest_digest, manifest_bytes));
    let batch = staged_objects
        .iter()
        .map(|(content_identity, content)| VerifiedObject {
            content_identity,
            content,
        })
        .collect::<Vec<_>>();
    store
        .put_verified_batch(&manifest.reference.repository.project, &batch)
        .map_err(PackagingError::Store)?;
    Ok(manifest)
}

fn is_canonical_committed_revision(revision: &SourceRevision) -> bool {
    revision.source_kind == SourceKind::CommittedTree
        && !revision.dirty
        && !revision.includes_untracked
        && revision.base_revision.is_some()
}

pub fn package_memory_source<S, O>(
    source: &S,
    project: RemoteProjectRef,
    policy: &MemoryPolicy,
    limits: PackagingLimits,
    store: &mut O,
) -> Result<MemorySourceManifest, PackagingError<S::Error, O::Error>>
where
    S: MemorySource,
    O: TenantObjectStore,
{
    require_protection(store.protection())?;
    let local = source.manifest().map_err(PackagingError::Source)?;
    if policy != &MemoryPolicy::default() {
        return Err(PackagingError::InvalidManifest);
    }

    local
        .validate()
        .map_err(|_| PackagingError::InvalidManifest)?;
    if local.policy_digest
        != policy
            .digest()
            .map_err(|_| PackagingError::InvalidManifest)?
    {
        return Err(PackagingError::InvalidManifest);
    }

    if (local.sources.len() as u64).saturating_add(1) > limits.max_objects.get() {
        return Err(PackagingError::ObjectLimitExceeded);
    }

    let mut summary = PackagingSummary::default();
    let mut sources = Vec::with_capacity(local.sources.len());
    let mut staged_objects = Vec::with_capacity(local.sources.len().saturating_add(1));
    for descriptor in &local.sources {
        let source_policy = policy
            .category(descriptor.category)
            .filter(|entry| entry.enabled)
            .ok_or(PackagingError::UnauthorizedMemoryCategory)?;
        if !is_remote_memory_category(descriptor.category) {
            return Err(PackagingError::UnauthorizedMemoryCategory);
        }

        let content = source
            .read_verified(descriptor)
            .map_err(PackagingError::Source)?;
        let sanitized = sanitize_memory_source(descriptor, &content.bytes)?;
        let sanitized_len = u64::try_from(sanitized.len()).unwrap_or(u64::MAX);
        if summary.total_bytes.saturating_add(sanitized_len) > limits.max_total_bytes.get() {
            return Err(PackagingError::ByteLimitExceeded);
        }

        let content_identity = sha256(&sanitized);
        let object = tenant_object_ref(&project, &content_identity)?;
        account_put(&mut summary, sanitized_len);
        sources.push(MemorySourceObject {
            category: descriptor.category,
            locator: descriptor.locator.clone(),
            source_fingerprint: descriptor.fingerprint.clone(),
            sanitized_byte_len: sanitized_len,
            sensitivity: source_policy.sensitivity,
            content_access: source_policy.content_access,
            object,
        });
        staged_objects.push((content_identity, sanitized));
    }

    sources = crate::json_size::sort_by_json_key(sources, memory_source_identity)
        .map_err(|_| PackagingError::InvalidManifest)?;
    source.revalidate(&local).map_err(PackagingError::Source)?;

    let expected_revision_id = local
        .revision_id()
        .map_err(|_| PackagingError::InvalidManifest)?;
    let body = MemorySourceManifestBody {
        protocol_version: DISTRIBUTED_SOURCE_MANIFEST_VERSION,
        project: project.clone(),
        memory_policy_digest: local.policy_digest.clone(),
        project_identity: local.project,
        source_set_digest: local.source_set_digest,
        extractor_set_digest: local.extractor_set_digest,
        policy_schema_version: policy.schema_version,
        sources,
        summary,
    };
    let manifest_bytes = serde_json::to_vec(&body).map_err(|_| PackagingError::InvalidManifest)?;
    if body
        .summary
        .total_bytes
        .saturating_add(manifest_bytes.len() as u64)
        > limits.max_total_bytes.get()
    {
        return Err(PackagingError::ByteLimitExceeded);
    }

    let manifest_digest = sha256(&manifest_bytes);
    let manifest_object = tenant_object_ref(&project, &manifest_digest)?;
    let reference = MemoryManifestRef {
        project,
        project_identity: body.project_identity.clone(),
        manifest_id: MemoryManifestId::new(manifest_digest.value())
            .map_err(|_| PackagingError::InvalidManifest)?,
        manifest_digest: manifest_digest.clone(),
        memory_policy_digest: local.policy_digest,
        expected_revision_id,
        manifest_object,
        repository_snapshot: None,
        repository_origin_snapshots: Vec::new(),
    };
    let manifest = MemorySourceManifest { reference, body };
    manifest.validate()?;
    staged_objects.push((manifest_digest, manifest_bytes));
    let batch = staged_objects
        .iter()
        .map(|(content_identity, content)| VerifiedObject {
            content_identity,
            content,
        })
        .collect::<Vec<_>>();
    store
        .put_verified_batch(&manifest.reference.project, &batch)
        .map_err(PackagingError::Store)?;
    Ok(manifest)
}

fn memory_source_identity(source: &MemorySourceObject) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(&(source.category, &source.locator, &source.source_fingerprint))
}

fn tenant_object_ref<S, E>(
    project: &RemoteProjectRef,
    content_identity: &Digest,
) -> Result<TenantObjectRef, PackagingError<S, E>> {
    Ok(TenantObjectRef {
        project: project.clone(),
        object_id: ObjectId::new(content_identity.value())
            .map_err(|_| PackagingError::InvalidManifest)?,
        content_identity: content_identity.clone(),
    })
}

fn require_protection<S, E>(protection: ObjectStoreProtection) -> Result<(), PackagingError<S, E>> {
    if !protection.authenticated_transport || !protection.encrypted_at_rest {
        return Err(PackagingError::InsecureObjectStore);
    }

    Ok(())
}

fn diagnostic_summary(
    manifest: &crate::repository_graph::ports::SourceManifest,
) -> PackagingSummary {
    let mut source_diagnostic_codes = BTreeMap::new();
    for diagnostic in &manifest.diagnostics {
        *source_diagnostic_codes
            .entry(diagnostic.code.clone())
            .or_insert(0) += 1;
    }

    PackagingSummary {
        source_diagnostic_codes,
        ..PackagingSummary::default()
    }
}

fn account_put(summary: &mut PackagingSummary, byte_len: u64) {
    summary.included_objects = summary.included_objects.saturating_add(1);
    summary.total_bytes = summary.total_bytes.saturating_add(byte_len);
}

fn verify_descriptor<S, E>(
    descriptor: &SourceFileDescriptor,
    content: &[u8],
) -> Result<(), PackagingError<S, E>> {
    if u64::try_from(content.len()).unwrap_or(u64::MAX) != descriptor.byte_len
        || sha256(content) != descriptor.content_identity
    {
        return Err(PackagingError::ContentIdentityMismatch);
    }

    Ok(())
}

fn sanitize_memory_source<S, E>(
    descriptor: &AuthorizedSourceDescriptor,
    content: &[u8],
) -> Result<Vec<u8>, PackagingError<S, E>> {
    let (canonical, fingerprint) = canonical_memory_source(descriptor.category, content)
        .ok_or(PackagingError::ContentIdentityMismatch)?;
    if fingerprint != descriptor.fingerprint {
        return Err(PackagingError::ContentIdentityMismatch);
    }

    Ok(canonical)
}

pub(crate) fn verify_sanitized_memory_source(source: &MemorySourceObject, content: &[u8]) -> bool {
    canonical_memory_source(source.category, content).is_some_and(|(canonical, fingerprint)| {
        canonical == content && fingerprint == source.source_fingerprint
    })
}

fn canonical_memory_source(
    category: MemorySourceCategory,
    content: &[u8],
) -> Option<(Vec<u8>, Digest)> {
    match category {
        MemorySourceCategory::SpecificationStructure | MemorySourceCategory::ApprovedOutcome => {
            let text = std::str::from_utf8(content).ok()?;
            let sanitized = sanitized_spec_source(category, text)?;
            let parsed = parse_spec_memory(std::str::from_utf8(&sanitized).ok()?);
            let fingerprint = match category {
                MemorySourceCategory::SpecificationStructure => {
                    memory_canonical_digest(&parsed.structure).ok()?
                }
                MemorySourceCategory::ApprovedOutcome => {
                    memory_canonical_digest(&parsed.outcome?).ok()?
                }
                _ => unreachable!(),
            };
            Some((sanitized, fingerprint))
        }
        MemorySourceCategory::ArchiveManifest | MemorySourceCategory::RuntimeProvenance => {
            let canonical = match category {
                MemorySourceCategory::ArchiveManifest => serde_json::to_vec(
                    &serde_json::from_slice::<ArchiveSourceDocument>(content).ok()?,
                ),
                MemorySourceCategory::RuntimeProvenance => serde_json::to_vec(
                    &serde_json::from_slice::<RuntimeSourceDocument>(content).ok()?,
                ),
                _ => unreachable!(),
            }
            .ok()?;
            let fingerprint = memory_canonical_digest(&canonical).ok()?;
            Some((canonical, fingerprint))
        }
        _ => None,
    }
}

fn is_remote_memory_category(category: MemorySourceCategory) -> bool {
    matches!(
        category,
        MemorySourceCategory::SpecificationStructure
            | MemorySourceCategory::ApprovedOutcome
            | MemorySourceCategory::ArchiveManifest
            | MemorySourceCategory::RuntimeProvenance
    )
}

fn repository_file_role(path: &RepoPath) -> RepositoryFileRole {
    let value = path.as_str().to_ascii_lowercase();
    let name = value.rsplit('/').next().unwrap_or(&value);
    if matches!(
        name,
        "cargo.toml" | "package.json" | "pyproject.toml" | "go.mod" | "ferrus.toml" | "dockerfile"
    ) {
        RepositoryFileRole::Manifest
    } else if value.ends_with(".md") || value.ends_with(".rst") || value.ends_with(".adoc") {
        RepositoryFileRole::Documentation
    } else if value.ends_with(".toml")
        || value.ends_with(".json")
        || value.ends_with(".yaml")
        || value.ends_with(".yml")
    {
        RepositoryFileRole::Configuration
    } else if [
        ".rs", ".c", ".cc", ".cpp", ".h", ".hpp", ".go", ".java", ".js", ".jsx", ".ts", ".tsx",
        ".py", ".rb", ".swift", ".kt", ".kts", ".cs",
    ]
    .iter()
    .any(|extension| value.ends_with(extension))
    {
        RepositoryFileRole::Source
    } else {
        RepositoryFileRole::Other
    }
}

fn hash_manifest<S, E>(value: &impl Serialize) -> Result<Digest, PackagingError<S, E>> {
    let encoded = serde_json::to_vec(value).map_err(|_| PackagingError::InvalidManifest)?;
    Ok(sha256(&encoded))
}

fn sha256(content: &[u8]) -> Digest {
    Digest::from_sha256(Sha256::digest(content).into())
}

#[cfg(test)]
#[path = "source_tests.rs"]
mod tests;
