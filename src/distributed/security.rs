//! Authorization, data protection, retention, deletion, and worker isolation.

use std::{collections::BTreeSet, num::NonZeroU64};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::repository_graph::domain::Digest;

use super::{
    DISTRIBUTED_POLICY_VERSION,
    identity::{
        AuditEventId, CredentialId, DeletionId, IndexJobId, PrincipalId, RemoteProjectRef,
        RemoteRepositoryRef, TenantId,
    },
    protocol::{IndexJobKind, RemoteErrorCode},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemotePermission {
    UploadSource,
    SubmitBuild,
    InspectJob,
    CancelJob,
    ClaimJob,
    ReadSourceObject,
    WriteFactBatch,
    PublishGraph,
    PublishMemory,
    QueryGraph,
    QueryMemory,
    ReadVerifiedContent,
    DeleteProject,
    DeleteRepository,
    ReadAdministrativeDiagnostics,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialClass {
    QueryAgent,
    SnapshotUploader,
    IndexWorker,
    Coordinator,
    ProjectOperator,
    TenantAdministrator,
}

impl CredentialClass {
    pub fn permissions(self) -> BTreeSet<RemotePermission> {
        use RemotePermission as Permission;
        match self {
            Self::QueryAgent => BTreeSet::from([
                Permission::QueryGraph,
                Permission::QueryMemory,
                Permission::ReadVerifiedContent,
            ]),
            Self::SnapshotUploader => BTreeSet::from([Permission::UploadSource]),
            Self::IndexWorker => BTreeSet::from([
                Permission::ClaimJob,
                Permission::ReadSourceObject,
                Permission::WriteFactBatch,
            ]),
            Self::Coordinator => BTreeSet::from([
                Permission::InspectJob,
                Permission::CancelJob,
                Permission::PublishGraph,
                Permission::PublishMemory,
            ]),
            Self::ProjectOperator => BTreeSet::from([
                Permission::UploadSource,
                Permission::SubmitBuild,
                Permission::InspectJob,
                Permission::CancelJob,
                Permission::QueryGraph,
                Permission::QueryMemory,
                Permission::ReadVerifiedContent,
            ]),
            Self::TenantAdministrator => RemotePermission::ALL.into_iter().collect(),
        }
    }
}

impl RemotePermission {
    pub const ALL: [Self; 15] = [
        Self::UploadSource,
        Self::SubmitBuild,
        Self::InspectJob,
        Self::CancelJob,
        Self::ClaimJob,
        Self::ReadSourceObject,
        Self::WriteFactBatch,
        Self::PublishGraph,
        Self::PublishMemory,
        Self::QueryGraph,
        Self::QueryMemory,
        Self::ReadVerifiedContent,
        Self::DeleteProject,
        Self::DeleteRepository,
        Self::ReadAdministrativeDiagnostics,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum AuthorizationScope {
    Tenant(TenantId),
    Project(RemoteProjectRef),
    Repository(RemoteRepositoryRef),
}

impl AuthorizationScope {
    fn covers(&self, resource: &Self) -> bool {
        match (self, resource) {
            (Self::Tenant(grant), Self::Tenant(resource)) => grant == resource,
            (Self::Tenant(grant), Self::Project(resource)) => grant == &resource.tenant_id,
            (Self::Tenant(grant), Self::Repository(resource)) => {
                grant == &resource.project.tenant_id
            }
            (Self::Project(grant), Self::Project(resource)) => grant == resource,
            (Self::Project(grant), Self::Repository(resource)) => grant == &resource.project,
            (Self::Repository(grant), Self::Repository(resource)) => grant == resource,
            _ => false,
        }
    }
}

/// Identifies a credential without carrying reusable credential material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizationContext {
    policy_version: u32,
    principal_id: PrincipalId,
    credential_id: CredentialId,
    credential_class: CredentialClass,
    scope: AuthorizationScope,
}

impl AuthorizationContext {
    pub fn for_class(
        principal_id: PrincipalId,
        credential_id: CredentialId,
        credential_class: CredentialClass,
        scope: AuthorizationScope,
    ) -> Self {
        Self {
            policy_version: DISTRIBUTED_POLICY_VERSION,
            principal_id,
            credential_id,
            credential_class,
            scope,
        }
    }

    pub fn principal_id(&self) -> &PrincipalId {
        &self.principal_id
    }

    pub fn credential_id(&self) -> &CredentialId {
        &self.credential_id
    }

    pub fn credential_class(&self) -> CredentialClass {
        self.credential_class
    }

    pub fn scope(&self) -> &AuthorizationScope {
        &self.scope
    }

    /// Must run before any object, job, snapshot, or project lookup.
    pub fn authorize(
        &self,
        permission: RemotePermission,
        resource: &AuthorizationScope,
    ) -> Result<(), AuthorizationError> {
        if self.policy_version != DISTRIBUTED_POLICY_VERSION
            || !self.credential_class.permissions().contains(&permission)
            || !self.scope.covers(resource)
        {
            return Err(AuthorizationError::Denied);
        }
        Ok(())
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AuthorizationError {
    #[error("remote operation is not authorized")]
    Denied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteDataClass {
    RepositorySource,
    CuratedMemorySource,
    OperationalMetadata,
    DerivedFact,
    QueryInput,
    VerifiedSnippet,
    ReusableCredential,
    AuditMetadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataSensitivity {
    Confidential,
    Sensitive,
    Operational,
}

impl RemoteDataClass {
    pub fn sensitivity(self) -> DataSensitivity {
        match self {
            Self::OperationalMetadata | Self::AuditMetadata => DataSensitivity::Operational,
            Self::ReusableCredential => DataSensitivity::Sensitive,
            Self::RepositorySource
            | Self::CuratedMemorySource
            | Self::DerivedFact
            | Self::QueryInput
            | Self::VerifiedSnippet => DataSensitivity::Confidential,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportProtection {
    AuthenticatedEncryptionRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AtRestProtection {
    EncryptionRequired,
    PrototypeLimitationMustBeDeclared,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataProtectionPolicy {
    pub policy_version: u32,
    pub transport: TransportProtection,
    pub at_rest: AtRestProtection,
    pub cross_tenant_deduplication: bool,
}

impl Default for DataProtectionPolicy {
    fn default() -> Self {
        Self {
            policy_version: DISTRIBUTED_POLICY_VERSION,
            transport: TransportProtection::AuthenticatedEncryptionRequired,
            at_rest: AtRestProtection::EncryptionRequired,
            cross_tenant_deduplication: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionClass {
    UploadedSource,
    UnpublishedFact,
    PublishedGraphSnapshot,
    PublishedMemoryRevision,
    QueryCache,
    AuditRecord,
}

impl RetentionClass {
    pub const ALL: [Self; 6] = [
        Self::UploadedSource,
        Self::UnpublishedFact,
        Self::PublishedGraphSnapshot,
        Self::PublishedMemoryRevision,
        Self::QueryCache,
        Self::AuditRecord,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionRule {
    pub class: RetentionClass,
    /// None means retained until explicit project deletion under this policy.
    pub max_age_seconds: Option<NonZeroU64>,
    pub delete_on_project_deletion: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteRetentionPolicy {
    pub policy_version: u32,
    pub rules: Vec<RetentionRule>,
}

impl RemoteRetentionPolicy {
    pub fn validate(&self) -> Result<(), RetentionPolicyError> {
        if self.policy_version != DISTRIBUTED_POLICY_VERSION {
            return Err(RetentionPolicyError::UnsupportedVersion);
        }
        let classes = self
            .rules
            .iter()
            .map(|rule| rule.class)
            .collect::<BTreeSet<_>>();
        if self.rules.len() != RetentionClass::ALL.len()
            || classes.len() != RetentionClass::ALL.len()
        {
            return Err(RetentionPolicyError::Incomplete);
        }
        Ok(())
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RetentionPolicyError {
    #[error("unsupported retention policy version")]
    UnsupportedVersion,
    #[error("retention policy must define every class exactly once")]
    Incomplete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum DeletionTarget {
    Project(RemoteProjectRef),
    Repository(RemoteRepositoryRef),
}

impl DeletionTarget {
    pub fn project(&self) -> &RemoteProjectRef {
        match self {
            Self::Project(project) => project,
            Self::Repository(repository) => &repository.project,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteDataRequest {
    pub policy_version: u32,
    pub deletion_id: DeletionId,
    pub target: DeletionTarget,
    pub coverage: BTreeSet<RetentionClass>,
    pub idempotency_key: Digest,
    pub requested_at: DateTime<Utc>,
}

#[derive(Serialize)]
struct DeletionIdentityMaterial<'a> {
    policy_version: u32,
    target: &'a DeletionTarget,
    coverage: &'a BTreeSet<RetentionClass>,
}

impl DeleteDataRequest {
    pub fn new(
        deletion_id: DeletionId,
        target: DeletionTarget,
        coverage: BTreeSet<RetentionClass>,
        requested_at: DateTime<Utc>,
    ) -> Result<Self, DeletionPolicyError> {
        let idempotency_key = deletion_key(&target, &coverage)?;
        let request = Self {
            policy_version: DISTRIBUTED_POLICY_VERSION,
            deletion_id,
            target,
            coverage,
            idempotency_key,
            requested_at,
        };
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&self) -> Result<(), DeletionPolicyError> {
        if self.policy_version != DISTRIBUTED_POLICY_VERSION {
            return Err(DeletionPolicyError::UnsupportedVersion);
        }
        if self.coverage.is_empty() {
            return Err(DeletionPolicyError::EmptyCoverage);
        }
        if self.idempotency_key != deletion_key(&self.target, &self.coverage)? {
            return Err(DeletionPolicyError::IdempotencyMismatch);
        }
        Ok(())
    }
}

fn deletion_key(
    target: &DeletionTarget,
    coverage: &BTreeSet<RetentionClass>,
) -> Result<Digest, DeletionPolicyError> {
    let encoded = serde_json::to_vec(&DeletionIdentityMaterial {
        policy_version: DISTRIBUTED_POLICY_VERSION,
        target,
        coverage,
    })
    .map_err(|_| DeletionPolicyError::Serialization)?;
    let mut hasher = Sha256::new();
    hasher.update(b"ferrus.distributed.project-deletion.v1\0");
    hasher.update(encoded);
    let value = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Digest::new("sha256", value).map_err(|_| DeletionPolicyError::Serialization)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeletionState {
    Requested,
    Running,
    Complete,
    Failed,
}

impl DeletionState {
    pub fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Requested, Self::Running | Self::Failed)
                | (Self::Running, Self::Complete | Self::Failed)
                | (Self::Failed, Self::Running)
        )
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum DeletionPolicyError {
    #[error("unsupported deletion policy version")]
    UnsupportedVersion,
    #[error("project deletion coverage must not be empty")]
    EmptyCoverage,
    #[error("project deletion idempotency key mismatch")]
    IdempotencyMismatch,
    #[error("project deletion contract serialization failed")]
    Serialization,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryExecutionPolicy {
    Denied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerEgressPolicy {
    AllowlistedControlAndObjectStoresOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerResourceLimits {
    pub max_snapshot_bytes: NonZeroU64,
    pub max_file_bytes: NonZeroU64,
    pub max_memory_bytes: NonZeroU64,
    pub max_parser_duration_ms: NonZeroU64,
    pub max_job_duration_ms: NonZeroU64,
    pub max_concurrency: NonZeroU64,
    pub max_facts: NonZeroU64,
    pub max_diagnostics: NonZeroU64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerSandboxPolicy {
    pub policy_version: u32,
    pub repository_execution: RepositoryExecutionPolicy,
    pub egress: WorkerEgressPolicy,
    pub read_only_source_objects: bool,
    pub ephemeral_workspace: bool,
    pub limits: WorkerResourceLimits,
}

impl WorkerSandboxPolicy {
    pub fn validate(&self) -> Result<(), WorkerPolicyError> {
        if self.policy_version != DISTRIBUTED_POLICY_VERSION {
            return Err(WorkerPolicyError::UnsupportedVersion);
        }
        if !self.read_only_source_objects || !self.ephemeral_workspace {
            return Err(WorkerPolicyError::UnsafeStorageBoundary);
        }
        Ok(())
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum WorkerPolicyError {
    #[error("unsupported worker policy version")]
    UnsupportedVersion,
    #[error("worker source objects must be read-only and workspace must be ephemeral")]
    UnsafeStorageBoundary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditCounter {
    Objects,
    Jobs,
    FactBatches,
    Snapshots,
    Revisions,
    CacheEntries,
    AuditRecords,
    DurationMs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditOutcome {
    Allowed,
    Denied,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum AuditedResource {
    Project(RemoteProjectRef),
    Repository(RemoteRepositoryRef),
    Job {
        project: RemoteProjectRef,
        job_id: IndexJobId,
        kind: IndexJobKind,
    },
    Deletion {
        target: DeletionTarget,
        deletion_id: DeletionId,
    },
}

/// Bounded audit record with no source, query, credential, or error text channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditRecord {
    pub policy_version: u32,
    pub event_id: AuditEventId,
    pub principal_id: PrincipalId,
    pub credential_id: CredentialId,
    pub action: RemotePermission,
    pub outcome: AuditOutcome,
    pub resource: AuditedResource,
    pub error_code: Option<RemoteErrorCode>,
    pub counters: std::collections::BTreeMap<AuditCounter, u64>,
    pub observed_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::distributed::identity::{RemoteProjectId, RemoteRepositoryId};

    fn project(tenant: &str) -> RemoteProjectRef {
        RemoteProjectRef {
            tenant_id: TenantId::new(tenant).unwrap(),
            project_id: RemoteProjectId::new("project").unwrap(),
        }
    }

    #[test]
    fn query_agent_matrix_is_query_only_and_scope_is_checked_before_lookup() {
        let context = AuthorizationContext::for_class(
            PrincipalId::new("agent").unwrap(),
            CredentialId::new("credential").unwrap(),
            CredentialClass::QueryAgent,
            AuthorizationScope::Project(project("tenant-a")),
        );
        assert!(
            context
                .authorize(
                    RemotePermission::QueryGraph,
                    &AuthorizationScope::Repository(RemoteRepositoryRef {
                        project: project("tenant-a"),
                        repository_id: RemoteRepositoryId::new("repo").unwrap(),
                    }),
                )
                .is_ok()
        );
        assert_eq!(
            context.authorize(
                RemotePermission::SubmitBuild,
                &AuthorizationScope::Project(project("tenant-a")),
            ),
            Err(AuthorizationError::Denied)
        );
        assert_eq!(
            context.authorize(
                RemotePermission::QueryGraph,
                &AuthorizationScope::Project(project("tenant-b")),
            ),
            Err(AuthorizationError::Denied)
        );
    }

    #[test]
    fn retention_requires_one_rule_per_independent_data_class() {
        let policy = RemoteRetentionPolicy {
            policy_version: DISTRIBUTED_POLICY_VERSION,
            rules: RetentionClass::ALL
                .into_iter()
                .map(|class| RetentionRule {
                    class,
                    max_age_seconds: NonZeroU64::new(3600),
                    delete_on_project_deletion: true,
                })
                .collect(),
        };
        assert!(policy.validate().is_ok());
        let mut incomplete = policy;
        incomplete.rules.pop();
        assert_eq!(incomplete.validate(), Err(RetentionPolicyError::Incomplete));
    }

    #[test]
    fn deletion_identity_is_deterministic_and_tenant_scoped() {
        let coverage = RetentionClass::ALL.into_iter().collect::<BTreeSet<_>>();
        let first = DeleteDataRequest::new(
            DeletionId::new("delete-a").unwrap(),
            DeletionTarget::Project(project("tenant-a")),
            coverage.clone(),
            Utc::now(),
        )
        .unwrap();
        let repeated = DeleteDataRequest::new(
            DeletionId::new("delete-b").unwrap(),
            DeletionTarget::Project(project("tenant-a")),
            coverage.clone(),
            Utc::now(),
        )
        .unwrap();
        let foreign = DeleteDataRequest::new(
            DeletionId::new("delete-c").unwrap(),
            DeletionTarget::Project(project("tenant-b")),
            coverage,
            Utc::now(),
        )
        .unwrap();
        assert_eq!(first.idempotency_key, repeated.idempotency_key);
        assert_ne!(first.idempotency_key, foreign.idempotency_key);
        assert!(first.validate().is_ok());
    }

    #[test]
    fn protection_and_worker_contracts_have_no_unrestricted_modes() {
        let protection = DataProtectionPolicy::default();
        assert!(!protection.cross_tenant_deduplication);
        assert_eq!(
            protection.transport,
            TransportProtection::AuthenticatedEncryptionRequired
        );
        let serialized = serde_json::to_string(&protection).unwrap();
        assert!(!serialized.contains("disabled"));
        assert!(!serialized.contains("plaintext"));
    }
}
