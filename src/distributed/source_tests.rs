//! Privacy-filtered packaging and immutable manifest validation.

use std::num::NonZeroU64;

use super::*;
use crate::{
    distributed::{
        identity::{ObjectId, RemoteProjectId, RemoteRepositoryId, RepositoryManifestId, TenantId},
        object_store::{EncryptedFilesystemObjectStore, ObjectStoreError, ObjectStoreQuota},
    },
    project_memory::{
        domain::{
            AuthorizedSourceManifest, MemoryRecordId, MemorySourceCategory, MemorySourceLocator,
            ProjectId, ProjectNamespace,
        },
        policy::MEMORY_POLICY_SCHEMA_VERSION,
        ports::{MemorySource, MemorySourceContent},
    },
    repository_graph::{
        config::RepositoryGraphConfig,
        domain::{RepositoryId, RepositoryNamespace, RepositoryRef},
        source::{LocalRepositorySource, SourceDiscoveryContext},
    },
};

fn remote_project(tenant: &str) -> RemoteProjectRef {
    RemoteProjectRef {
        tenant_id: TenantId::new(tenant).unwrap(),
        project_id: RemoteProjectId::new("project").unwrap(),
    }
}

fn remote_repository(tenant: &str) -> RemoteRepositoryRef {
    RemoteRepositoryRef {
        project: remote_project(tenant),
        repository_id: RemoteRepositoryId::new("repository").unwrap(),
    }
}

fn local_repository() -> RepositoryRef {
    RepositoryRef {
        namespace: RepositoryNamespace::new("local:test").unwrap(),
        repository_id: RepositoryId::new("repository").unwrap(),
    }
}

fn local_project() -> ProjectRef {
    ProjectRef {
        namespace: ProjectNamespace::new("local:test").unwrap(),
        project_id: ProjectId::new("project").unwrap(),
    }
}

fn limits() -> PackagingLimits {
    PackagingLimits {
        max_objects: NonZeroU64::new(100).unwrap(),
        max_total_bytes: NonZeroU64::new(1024 * 1024).unwrap(),
        max_diagnostics: NonZeroU64::new(100).unwrap(),
    }
}

fn store(path: &std::path::Path) -> EncryptedFilesystemObjectStore {
    EncryptedFilesystemObjectStore::open(
        path,
        [17; 32],
        ObjectStoreQuota {
            max_objects_per_project: NonZeroU64::new(100).unwrap(),
            max_bytes_per_project: NonZeroU64::new(1024 * 1024).unwrap(),
            max_object_bytes: NonZeroU64::new(1024 * 1024).unwrap(),
        },
        true,
    )
    .unwrap()
}

fn git(root: &std::path::Path, args: &[&str]) {
    assert!(
        std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .unwrap()
            .success()
    );
}

fn commit_repository(root: &std::path::Path) {
    git(root, &["init"]);
    git(root, &["config", "user.email", "tests@example.com"]);
    git(root, &["config", "user.name", "Ferrus Tests"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    git(root, &["add", "--all"]);
    git(root, &["commit", "-m", "initial"]);
}

#[test]
fn repository_packaging_uploads_only_locally_filtered_verified_files() {
    let repository_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repository_dir.path().join("src")).unwrap();
    std::fs::write(
        repository_dir.path().join("src/lib.rs"),
        b"pub fn run() {}\n",
    )
    .unwrap();
    std::fs::write(repository_dir.path().join(".env"), b"TOKEN=secret\n").unwrap();
    std::fs::write(repository_dir.path().join("binary.bin"), b"binary\0secret").unwrap();
    commit_repository(repository_dir.path());
    let config = RepositoryGraphConfig::default();
    let context = SourceDiscoveryContext::from_config(local_repository(), &config, &[]).unwrap();
    let source = LocalRepositorySource::discover(repository_dir.path(), context).unwrap();
    let object_dir = tempfile::tempdir().unwrap();
    let mut object_store = store(object_dir.path());
    let packaged = package_repository_source(
        &source,
        remote_repository("tenant-a"),
        RepositoryPackagingPolicy {
            schema_version: 1,
            source_policy_digest: config.source_policy_digest().unwrap(),
        },
        limits(),
        &mut object_store,
    )
    .unwrap();

    assert_eq!(packaged.body.files.len(), 1);
    assert_eq!(packaged.body.files[0].path.as_str(), "src/lib.rs");
    assert_eq!(packaged.body.files[0].file_role, RepositoryFileRole::Source);
    assert_eq!(
        object_store
            .read_verified(&packaged.body.files[0].object)
            .unwrap(),
        b"pub fn run() {}\n"
    );
    assert_eq!(
        object_store
            .read_verified(&packaged.reference.manifest_object)
            .unwrap(),
        serde_json::to_vec(&packaged.body).unwrap()
    );
    let serialized = serde_json::to_string(&packaged).unwrap();
    assert!(!serialized.contains(".env"));
    assert!(!serialized.contains("binary.bin"));
    assert!(!serialized.contains("TOKEN"));
    assert!(
        packaged
            .body
            .summary
            .source_diagnostic_codes
            .keys()
            .any(|code| code.as_str() == "sensitive_path_excluded")
    );

    let assert_invalid = |mut forged: RepositorySourceManifest| {
        let manifest_digest = hash_manifest::<
            anyhow::Error,
            crate::distributed::object_store::ObjectStoreError,
        >(&forged.body)
        .unwrap();
        forged.reference.manifest_id = RepositoryManifestId::new(manifest_digest.value()).unwrap();
        forged.reference.manifest_digest = manifest_digest.clone();
        forged.reference.manifest_object.object_id =
            ObjectId::new(manifest_digest.value()).unwrap();
        forged.reference.manifest_object.content_identity = manifest_digest;
        assert!(matches!(
            forged.validate::<anyhow::Error, crate::distributed::object_store::ObjectStoreError>(),
            Err(PackagingError::InvalidManifest)
        ));
    };

    let mut wrong_kind = packaged.clone();
    wrong_kind.body.source_revision.source_kind = SourceKind::NonGitManifest;
    assert_invalid(wrong_kind);

    let mut dirty = packaged.clone();
    dirty.body.source_revision.dirty = true;
    assert_invalid(dirty);

    let mut includes_untracked = packaged.clone();
    includes_untracked.body.source_revision.includes_untracked = true;
    assert_invalid(includes_untracked);

    let mut mismatched_manifest = packaged.clone();
    mismatched_manifest.body.source_revision.manifest_digest = Digest::new("sha256", "00").unwrap();
    mismatched_manifest.reference.expected_snapshot_id = snapshot_identity_from_revision(
        &mismatched_manifest.body.source_revision,
        &mismatched_manifest.body.extractor_set_digest,
    )
    .unwrap();
    assert_invalid(mismatched_manifest);

    let mut missing_base = packaged;
    missing_base.body.source_revision.base_revision = None;
    assert_invalid(missing_base);
}

#[test]
fn repository_packaging_rejects_non_git_manifests() {
    let repository_dir = tempfile::tempdir().unwrap();
    std::fs::write(repository_dir.path().join("lib.rs"), b"pub fn run() {}\n").unwrap();
    let config = RepositoryGraphConfig::default();
    let context = SourceDiscoveryContext::from_config(local_repository(), &config, &[]).unwrap();
    let source = LocalRepositorySource::discover(repository_dir.path(), context).unwrap();
    let object_dir = tempfile::tempdir().unwrap();
    let mut object_store = store(object_dir.path());

    let result = package_repository_source(
        &source,
        remote_repository("tenant-a"),
        RepositoryPackagingPolicy {
            schema_version: 1,
            source_policy_digest: config.source_policy_digest().unwrap(),
        },
        limits(),
        &mut object_store,
    );

    assert!(matches!(
        result,
        Err(PackagingError::NonCanonicalRepositorySource)
    ));
}

struct FakeMemorySource {
    manifest: AuthorizedSourceManifest,
    content: Vec<u8>,
}

impl MemorySource for FakeMemorySource {
    type Error = anyhow::Error;

    fn manifest(&self) -> Result<AuthorizedSourceManifest, Self::Error> {
        Ok(self.manifest.clone())
    }

    fn read_verified(
        &self,
        source: &AuthorizedSourceDescriptor,
    ) -> Result<MemorySourceContent, Self::Error> {
        anyhow::ensure!(self.manifest.sources.contains(source));
        Ok(MemorySourceContent {
            bytes: self.content.clone(),
        })
    }

    fn revalidate(&self, manifest: &AuthorizedSourceManifest) -> Result<(), Self::Error> {
        anyhow::ensure!(manifest == &self.manifest);
        Ok(())
    }
}

#[test]
fn memory_packaging_redacts_non_authorized_spec_text_before_storage() {
    let content = b"# Example\n\nPrivate task body.\n\n- [x] #5.0 Done\n\nID: rg5.0\n\n## Outcome\n\nApproved result.\n\n## Private\n\nNever upload this.\n".to_vec();
    let parsed = parse_spec_memory(std::str::from_utf8(&content).unwrap());
    let policy = MemoryPolicy::default();
    let descriptor = AuthorizedSourceDescriptor {
        project: local_project(),
        category: MemorySourceCategory::SpecificationStructure,
        locator: MemorySourceLocator::TrackedFile {
            path: RepoPath::new("docs/specs/example.md").unwrap(),
        },
        fingerprint: memory_canonical_digest(&parsed.structure).unwrap(),
        byte_len: content.len() as u64,
    };
    let mut manifest = AuthorizedSourceManifest {
        project: local_project(),
        policy_digest: policy.digest().unwrap(),
        source_set_digest: sha256(b"placeholder"),
        extractor_set_digest: sha256(b"extractors"),
        sources: vec![descriptor],
    };
    manifest.source_set_digest = manifest.computed_source_set_digest().unwrap();
    let source = FakeMemorySource { manifest, content };
    let object_dir = tempfile::tempdir().unwrap();
    let mut object_store = store(object_dir.path());
    let first = package_memory_source(
        &source,
        remote_project("tenant-a"),
        &policy,
        limits(),
        &mut object_store,
    )
    .unwrap();
    let repeated = package_memory_source(
        &source,
        remote_project("tenant-a"),
        &policy,
        limits(),
        &mut object_store,
    )
    .unwrap();

    assert_eq!(first, repeated);
    assert_eq!(
        first.body.policy_schema_version,
        MEMORY_POLICY_SCHEMA_VERSION
    );
    let stored = object_store
        .read_verified(&first.body.sources[0].object)
        .unwrap();
    assert_eq!(
        object_store
            .read_verified(&first.reference.manifest_object)
            .unwrap(),
        serde_json::to_vec(&first.body).unwrap()
    );
    let stored = String::from_utf8(stored).unwrap();
    assert!(stored.contains("# Example"));
    assert!(stored.contains("ID: rg5.0"));
    assert!(!stored.contains("Private task body"));
    assert!(!stored.contains("Approved result"));
    assert!(!stored.contains("Never upload"));

    let mut forged = first;
    forged.body.sources[0].sensitivity = MemorySourceSensitivity::Sensitive;
    forged.body.sources[0].content_access = MemoryContentAccess::RawBody;
    let manifest_digest = hash_manifest::<
        anyhow::Error,
        crate::distributed::object_store::ObjectStoreError,
    >(&forged.body)
    .unwrap();
    forged.reference.manifest_id = MemoryManifestId::new(manifest_digest.value()).unwrap();
    forged.reference.manifest_digest = manifest_digest.clone();
    forged.reference.manifest_object.object_id = ObjectId::new(manifest_digest.value()).unwrap();
    forged.reference.manifest_object.content_identity = manifest_digest;
    assert!(matches!(
        forged.validate::<anyhow::Error, crate::distributed::object_store::ObjectStoreError>(),
        Err(PackagingError::InvalidManifest)
    ));
}

#[test]
fn memory_packaging_canonicalizes_source_discovery_order() {
    let content =
        b"# Example\n\n- [x] #5.0 Done\n\nID: rg5.0\n\n## Outcome\n\nApproved.\n".to_vec();
    let parsed = parse_spec_memory(std::str::from_utf8(&content).unwrap());
    let policy = MemoryPolicy::default();
    let descriptor = |category, fingerprint| AuthorizedSourceDescriptor {
        project: local_project(),
        category,
        locator: MemorySourceLocator::TrackedFile {
            path: RepoPath::new("docs/specs/example.md").unwrap(),
        },
        fingerprint,
        byte_len: content.len() as u64,
    };
    let structure = descriptor(
        MemorySourceCategory::SpecificationStructure,
        memory_canonical_digest(&parsed.structure).unwrap(),
    );
    let outcome = descriptor(
        MemorySourceCategory::ApprovedOutcome,
        memory_canonical_digest(parsed.outcome.as_ref().unwrap()).unwrap(),
    );
    let source = |sources| {
        let mut manifest = AuthorizedSourceManifest {
            project: local_project(),
            policy_digest: policy.digest().unwrap(),
            source_set_digest: sha256(b"placeholder"),
            extractor_set_digest: sha256(b"extractors"),
            sources,
        };
        manifest.source_set_digest = manifest.computed_source_set_digest().unwrap();
        FakeMemorySource {
            manifest,
            content: content.clone(),
        }
    };
    let object_dir = tempfile::tempdir().unwrap();
    let mut object_store = store(object_dir.path());

    let first = package_memory_source(
        &source(vec![structure.clone(), outcome.clone()]),
        remote_project("tenant-a"),
        &policy,
        limits(),
        &mut object_store,
    )
    .unwrap();
    let reordered = package_memory_source(
        &source(vec![outcome, structure]),
        remote_project("tenant-a"),
        &policy,
        limits(),
        &mut object_store,
    )
    .unwrap();

    assert_eq!(first, reordered);
    assert!(first.body.sources.windows(2).all(|pair| {
        memory_source_identity(&pair[0]).unwrap() < memory_source_identity(&pair[1]).unwrap()
    }));
}

#[test]
fn failed_memory_packaging_does_not_persist_staged_source_objects() {
    let content = b"# Example\n\n- [x] #5.0 Done\n\nID: rg5.0\n".to_vec();
    let parsed = parse_spec_memory(std::str::from_utf8(&content).unwrap());
    let policy = MemoryPolicy::default();
    let descriptor = AuthorizedSourceDescriptor {
        project: local_project(),
        category: MemorySourceCategory::SpecificationStructure,
        locator: MemorySourceLocator::TrackedFile {
            path: RepoPath::new("docs/specs/example.md").unwrap(),
        },
        fingerprint: memory_canonical_digest(&parsed.structure).unwrap(),
        byte_len: content.len() as u64,
    };
    let sanitized =
        sanitize_memory_source::<anyhow::Error, ObjectStoreError>(&descriptor, &content).unwrap();
    let content_identity = sha256(&sanitized);
    let mut manifest = AuthorizedSourceManifest {
        project: local_project(),
        policy_digest: policy.digest().unwrap(),
        source_set_digest: sha256(b"placeholder"),
        extractor_set_digest: sha256(b"extractors"),
        sources: vec![descriptor],
    };
    manifest.source_set_digest = manifest.computed_source_set_digest().unwrap();
    let source = FakeMemorySource { manifest, content };
    let object_dir = tempfile::tempdir().unwrap();
    let mut object_store = store(object_dir.path());
    let project = remote_project("tenant-a");
    let result = package_memory_source(
        &source,
        project.clone(),
        &policy,
        PackagingLimits {
            max_objects: NonZeroU64::new(100).unwrap(),
            max_total_bytes: NonZeroU64::new(sanitized.len() as u64).unwrap(),
            max_diagnostics: NonZeroU64::new(100).unwrap(),
        },
        &mut object_store,
    );

    assert!(matches!(result, Err(PackagingError::ByteLimitExceeded)));
    let object = TenantObjectRef {
        project,
        object_id: ObjectId::new(content_identity.value()).unwrap(),
        content_identity,
    };
    assert!(matches!(
        object_store.read_verified(&object),
        Err(ObjectStoreError::ObjectUnavailable)
    ));
}

#[test]
fn sanitized_memory_json_rejects_unknown_payload_fields() {
    let document = ArchiveSourceDocument {
        archive_id: "archive".to_string(),
        spec_path: RepoPath::new("docs/specs/example.md").unwrap(),
        archived_at: "2026-08-08T00:00:00Z".to_string(),
        task_count: 1,
        run_count: 1,
        task_ids: vec!["t-001".to_string()],
        milestone_ids: vec!["rg5.1".to_string()],
    };
    let mut value = serde_json::to_value(document).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .insert("raw_submission".to_string(), serde_json::json!("secret"));
    let content = serde_json::to_vec(&value).unwrap();
    let descriptor = AuthorizedSourceDescriptor {
        project: local_project(),
        category: MemorySourceCategory::ArchiveManifest,
        locator: MemorySourceLocator::ArchiveManifest {
            archive_id: MemoryRecordId::new("archive").unwrap(),
        },
        fingerprint: memory_canonical_digest(&content).unwrap(),
        byte_len: content.len() as u64,
    };
    assert!(matches!(
        sanitize_memory_source::<anyhow::Error, crate::distributed::object_store::ObjectStoreError>(
            &descriptor,
            &content,
        ),
        Err(PackagingError::ContentIdentityMismatch)
    ));
}
