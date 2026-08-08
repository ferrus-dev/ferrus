//! Vendor-neutral deletion, recovery, and audit service contracts.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{
    DISTRIBUTED_MAINTENANCE_PROTOCOL_VERSION,
    identity::{AuditEventId, DeletionId, RequestId},
    protocol::{DistributedProtocolError, RemoteError},
    security::{
        AuditCounter, AuthorizationContext, DeleteDataRequest, DeletionState, DeletionTarget,
    },
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteDeleteRequest {
    pub protocol_version: u32,
    pub request_id: RequestId,
    pub deletion: DeleteDataRequest,
}

impl RemoteDeleteRequest {
    pub fn validate(&self) -> Result<(), DistributedProtocolError> {
        if self.protocol_version != DISTRIBUTED_MAINTENANCE_PROTOCOL_VERSION {
            return Err(DistributedProtocolError::UnsupportedVersion);
        }
        self.deletion
            .validate()
            .map_err(|_| DistributedProtocolError::DeletionMismatch)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectRemoteDeletionRequest {
    pub protocol_version: u32,
    pub request_id: RequestId,
    pub deletion_id: DeletionId,
    pub target: DeletionTarget,
}

impl InspectRemoteDeletionRequest {
    pub fn validate(&self) -> Result<(), DistributedProtocolError> {
        if self.protocol_version != DISTRIBUTED_MAINTENANCE_PROTOCOL_VERSION {
            return Err(DistributedProtocolError::UnsupportedVersion);
        }
        Ok(())
    }
}

/// Privacy-safe deletion progress. Source names, object keys, query text, and
/// backend errors are deliberately absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteDeletionResult {
    pub protocol_version: u32,
    pub request_id: RequestId,
    pub deletion_id: DeletionId,
    pub target: DeletionTarget,
    pub state: DeletionState,
    pub counters: BTreeMap<AuditCounter, u64>,
    pub audit_event_id: Option<AuditEventId>,
    pub updated_at: DateTime<Utc>,
}

pub trait RemoteMaintenanceApi {
    fn delete(
        &mut self,
        authorization: &AuthorizationContext,
        request: &RemoteDeleteRequest,
        now: DateTime<Utc>,
    ) -> Result<RemoteDeletionResult, RemoteError>;

    fn inspect_deletion(
        &self,
        authorization: &AuthorizationContext,
        request: &InspectRemoteDeletionRequest,
    ) -> Result<Option<RemoteDeletionResult>, RemoteError>;
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::distributed::{
        DISTRIBUTED_POLICY_VERSION,
        identity::{RemoteProjectId, RemoteProjectRef, TenantId},
        security::RetentionClass,
    };

    #[test]
    fn deletion_envelopes_fail_closed_on_version_or_identity_drift() {
        let deletion = DeleteDataRequest::new(
            DeletionId::new("delete-project").unwrap(),
            DeletionTarget::Project(RemoteProjectRef {
                tenant_id: TenantId::new("tenant").unwrap(),
                project_id: RemoteProjectId::new("project").unwrap(),
            }),
            RetentionClass::ALL.into_iter().collect::<BTreeSet<_>>(),
            Utc::now(),
        )
        .unwrap();
        let mut request = RemoteDeleteRequest {
            protocol_version: DISTRIBUTED_MAINTENANCE_PROTOCOL_VERSION,
            request_id: RequestId::new("delete-request").unwrap(),
            deletion,
        };
        assert!(request.validate().is_ok());
        request.protocol_version += 1;
        assert_eq!(
            request.validate(),
            Err(DistributedProtocolError::UnsupportedVersion)
        );
        request.protocol_version = DISTRIBUTED_MAINTENANCE_PROTOCOL_VERSION;
        request.deletion.policy_version = DISTRIBUTED_POLICY_VERSION + 1;
        assert_eq!(
            request.validate(),
            Err(DistributedProtocolError::DeletionMismatch)
        );
    }
}
