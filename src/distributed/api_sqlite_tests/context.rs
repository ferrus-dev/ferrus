//! Tests for context.

use super::*;

#[test]
fn query_agent_is_read_only_and_denied_before_control_lookup() {
    let fixture = fixture();
    let query_agent = auth(
        CredentialClass::QueryAgent,
        AuthorizationScope::Project(fixture.project.clone()),
    );
    let unknown = InspectIndexJobRequest {
        protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
        request_id: RequestId::new("inspect-unknown").unwrap(),
        job: super::super::super::protocol::IndexJobRef {
            project: fixture.project.clone(),
            job_id: IndexJobId::new("unknown").unwrap(),
            kind: IndexJobKind::RepositoryGraph,
        },
    };
    let control = SqliteRemoteControlApi::new(coordinator(&fixture.control_path));
    let error = control.inspect_build(&query_agent, &unknown).unwrap_err();
    assert_eq!(error.code, RemoteErrorCode::Unauthorized);

    let foreign = auth(
        CredentialClass::Coordinator,
        AuthorizationScope::Project(RemoteProjectRef {
            tenant_id: TenantId::new("tenant-b").unwrap(),
            project_id: RemoteProjectId::new("project").unwrap(),
        }),
    );
    let error = control.inspect_build(&foreign, &unknown).unwrap_err();
    assert_eq!(error.code, RemoteErrorCode::Unauthorized);
}
