//! Tenant-scoped identities for remote repository graph and project memory.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::{
    project_memory::domain::{MemoryRevisionId, ProjectRef},
    repository_graph::domain::{Digest, SnapshotId},
};

const MAX_REMOTE_ID_BYTES: usize = 128;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RemoteIdentityError {
    #[error("{kind} must be 1..={MAX_REMOTE_ID_BYTES} lowercase ASCII token bytes")]
    Invalid { kind: &'static str },
    #[error("federated graph and memory references must belong to the same tenant and project")]
    FederatedScopeMismatch,
    #[error("manifest object scope or identity does not match its manifest reference")]
    ManifestObjectMismatch,
}

fn validate_remote_id(value: String, kind: &'static str) -> Result<String, RemoteIdentityError> {
    if value.is_empty()
        || value.len() > MAX_REMOTE_ID_BYTES
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        })
        || !value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
    {
        return Err(RemoteIdentityError::Invalid { kind });
    }
    Ok(value)
}

macro_rules! remote_id {
    ($name:ident, $kind:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, RemoteIdentityError> {
                validate_remote_id(value.into(), $kind).map(Self)
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }
    };
}

remote_id!(TenantId, "tenant id");
remote_id!(RemoteProjectId, "remote project id");
remote_id!(RemoteRepositoryId, "remote repository id");
remote_id!(RepositoryManifestId, "repository manifest id");
remote_id!(MemoryManifestId, "memory manifest id");
remote_id!(IndexJobId, "index job id");
remote_id!(IndexJobFailureCode, "index job failure code");
remote_id!(FactBatchId, "fact batch id");
remote_id!(FactShardId, "fact shard id");
remote_id!(WorkerId, "worker id");
remote_id!(PrincipalId, "principal id");
remote_id!(CredentialId, "credential id");
remote_id!(RequestId, "request id");
remote_id!(RemotePageCursor, "remote page cursor");
remote_id!(DeletionId, "deletion id");
remote_id!(AuditEventId, "audit event id");
remote_id!(ObjectId, "tenant object id");

/// Cloud scope is intentionally distinct from Ferrus's machine-local project identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteProjectRef {
    pub tenant_id: TenantId,
    pub project_id: RemoteProjectId,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteRepositoryRef {
    pub project: RemoteProjectRef,
    pub repository_id: RemoteRepositoryId,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteGraphSnapshotRef {
    pub repository: RemoteRepositoryRef,
    pub snapshot_id: SnapshotId,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteMemoryRevisionRef {
    pub project: RemoteProjectRef,
    pub revision_id: MemoryRevisionId,
}

/// A digest does not grant access. Object lookup also requires this explicit scope.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantObjectRef {
    pub project: RemoteProjectRef,
    pub object_id: ObjectId,
    pub content_identity: Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryManifestRef {
    pub repository: RemoteRepositoryRef,
    pub repository_identity: crate::repository_graph::domain::RepositoryRef,
    pub manifest_id: RepositoryManifestId,
    pub manifest_digest: Digest,
    pub source_policy_digest: Digest,
    pub expected_snapshot_id: SnapshotId,
    pub manifest_object: TenantObjectRef,
}

impl RepositoryManifestRef {
    pub fn validate(&self) -> Result<(), RemoteIdentityError> {
        if self.manifest_object.project != self.repository.project
            || self.manifest_object.content_identity != self.manifest_digest
            || self.manifest_object.object_id.as_str() != self.manifest_digest.value()
        {
            return Err(RemoteIdentityError::ManifestObjectMismatch);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryManifestRef {
    pub project: RemoteProjectRef,
    pub project_identity: ProjectRef,
    pub manifest_id: MemoryManifestId,
    pub manifest_digest: Digest,
    pub memory_policy_digest: Digest,
    pub expected_revision_id: MemoryRevisionId,
    pub manifest_object: TenantObjectRef,
    /// Optional immutable repository snapshot used only for cross-link
    /// resolution. It is intentionally excluded from the semantic memory
    /// revision identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_snapshot: Option<RemoteGraphSnapshotRef>,
}

impl MemoryManifestRef {
    pub fn validate(&self) -> Result<(), RemoteIdentityError> {
        if self.manifest_object.project != self.project
            || self.manifest_object.content_identity != self.manifest_digest
            || self.manifest_object.object_id.as_str() != self.manifest_digest.value()
            || self
                .repository_snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.repository.project != self.project)
        {
            return Err(RemoteIdentityError::ManifestObjectMismatch);
        }
        Ok(())
    }
}

/// Immutable pair used only for federated query selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FederatedViewRef {
    pub graph: RemoteGraphSnapshotRef,
    pub memory: RemoteMemoryRevisionRef,
}

impl FederatedViewRef {
    pub fn new(
        graph: RemoteGraphSnapshotRef,
        memory: RemoteMemoryRevisionRef,
    ) -> Result<Self, RemoteIdentityError> {
        let value = Self { graph, memory };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), RemoteIdentityError> {
        if self.graph.repository.project != self.memory.project {
            return Err(RemoteIdentityError::FederatedScopeMismatch);
        }
        Ok(())
    }

    pub fn project(&self) -> &RemoteProjectRef {
        &self.memory.project
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(value: &str) -> Digest {
        Digest::new("sha256", value).unwrap()
    }

    fn project(tenant: &str) -> RemoteProjectRef {
        RemoteProjectRef {
            tenant_id: TenantId::new(tenant).unwrap(),
            project_id: RemoteProjectId::new("project").unwrap(),
        }
    }

    #[test]
    fn remote_ids_are_bounded_canonical_tokens() {
        assert!(TenantId::new("tenant-a").is_ok());
        assert!(TenantId::new("").is_err());
        assert!(TenantId::new("Tenant-A").is_err());
        assert!(TenantId::new("../tenant").is_err());
        assert!(TenantId::new("x".repeat(MAX_REMOTE_ID_BYTES + 1)).is_err());
    }

    #[test]
    fn content_identity_never_erases_tenant_scope() {
        let left = TenantObjectRef {
            project: project("tenant-a"),
            object_id: ObjectId::new("object").unwrap(),
            content_identity: digest("00"),
        };
        let right = TenantObjectRef {
            project: project("tenant-b"),
            ..left.clone()
        };
        assert_ne!(left, right);
        assert_eq!(left.content_identity, right.content_identity);
    }

    #[test]
    fn federated_views_cannot_cross_tenants_or_projects() {
        let graph = RemoteGraphSnapshotRef {
            repository: RemoteRepositoryRef {
                project: project("tenant-a"),
                repository_id: RemoteRepositoryId::new("repo").unwrap(),
            },
            snapshot_id: SnapshotId::new("snapshot").unwrap(),
        };
        let matching = RemoteMemoryRevisionRef {
            project: project("tenant-a"),
            revision_id: MemoryRevisionId::new("revision").unwrap(),
        };
        assert!(FederatedViewRef::new(graph.clone(), matching).is_ok());
        let foreign = RemoteMemoryRevisionRef {
            project: project("tenant-b"),
            revision_id: MemoryRevisionId::new("revision").unwrap(),
        };
        assert_eq!(
            FederatedViewRef::new(graph, foreign),
            Err(RemoteIdentityError::FederatedScopeMismatch)
        );
    }
}
