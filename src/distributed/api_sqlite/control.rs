//! Authorize remote build operations before validating requests or accessing coordinator state.

use super::*;

pub struct SqliteRemoteControlApi {
    coordinator: SqliteIndexJobCoordinator,
}

impl SqliteRemoteControlApi {
    pub fn new(coordinator: SqliteIndexJobCoordinator) -> Self {
        Self { coordinator }
    }

    pub fn into_inner(self) -> SqliteIndexJobCoordinator {
        self.coordinator
    }
}

impl RemoteControlApi for SqliteRemoteControlApi {
    fn submit_build(
        &mut self,
        authorization: &AuthorizationContext,
        request: &SubmitIndexJobRequest,
        now: DateTime<Utc>,
    ) -> Result<RemoteControlResponse<IndexJobRecord>, RemoteError> {
        authorize(
            authorization,
            RemotePermission::SubmitBuild,
            AuthorizationScope::Project(request.project.clone()),
            &request.request_id,
            DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
        )?;
        request.validate().map_err(|error| {
            protocol_error(
                &request.request_id,
                DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                error,
            )
        })?;
        let record = self
            .coordinator
            .submit(request, now)
            .map_err(|error| control_backend_error(&request.request_id, error))?;
        Ok(control_response(
            request.request_id.clone(),
            request.project.clone(),
            record,
        ))
    }

    fn inspect_build(
        &self,
        authorization: &AuthorizationContext,
        request: &InspectIndexJobRequest,
    ) -> Result<RemoteControlResponse<IndexJobRecord>, RemoteError> {
        authorize(
            authorization,
            RemotePermission::InspectJob,
            AuthorizationScope::Project(request.job.project.clone()),
            &request.request_id,
            DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
        )?;
        request.validate().map_err(|error| {
            protocol_error(
                &request.request_id,
                DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                error,
            )
        })?;
        let record = self
            .coordinator
            .inspect(request)
            .map_err(|error| control_backend_error(&request.request_id, error))?
            .ok_or_else(|| remote_error(&request.request_id, RemoteErrorCode::NotFound, false))?;
        Ok(control_response(
            request.request_id.clone(),
            request.job.project.clone(),
            record,
        ))
    }

    fn cancel_build(
        &mut self,
        authorization: &AuthorizationContext,
        request: &CancelIndexJobRequest,
        now: DateTime<Utc>,
    ) -> Result<RemoteControlResponse<IndexJobRecord>, RemoteError> {
        authorize(
            authorization,
            RemotePermission::CancelJob,
            AuthorizationScope::Project(request.job.project.clone()),
            &request.request_id,
            DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
        )?;
        request.validate().map_err(|error| {
            protocol_error(
                &request.request_id,
                DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
                error,
            )
        })?;
        let record = self
            .coordinator
            .cancel(request, now)
            .map_err(|error| control_backend_error(&request.request_id, error))?;
        Ok(control_response(
            request.request_id.clone(),
            request.job.project.clone(),
            record,
        ))
    }
}

fn control_response<T>(
    request_id: RequestId,
    project: RemoteProjectRef,
    body: T,
) -> RemoteControlResponse<T> {
    RemoteControlResponse {
        protocol_version: DISTRIBUTED_CONTROL_PROTOCOL_VERSION,
        request_id,
        project,
        body,
    }
}
