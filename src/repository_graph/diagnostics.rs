//! Content-free lifecycle diagnostics and event adapters.

use std::convert::Infallible;

use serde::{Deserialize, Serialize};

use super::{
    domain::{BuildId, RepositoryRef, SnapshotId},
    ports::EventSink,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleEventKind {
    BuildStarted,
    BuildFailed,
    SnapshotCompleted,
    SnapshotPublished,
    BuildSuperseded,
    PublicationConflict,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleCounters {
    pub files: u64,
    pub nodes: u64,
    pub edges: u64,
    pub diagnostics: u64,
}

/// Safe event envelope: only identities, typed state and numeric counters.
/// It intentionally has no arbitrary message, path, content, or metadata map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GraphLifecycleEvent<'a> {
    pub kind: LifecycleEventKind,
    pub repository: &'a RepositoryRef,
    pub build_id: &'a BuildId,
    pub snapshot_id: Option<&'a SnapshotId>,
    pub counters: LifecycleCounters,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TracingEventSink;

impl EventSink for TracingEventSink {
    type Error = Infallible;

    fn emit(&self, event: GraphLifecycleEvent<'_>) -> Result<(), Self::Error> {
        tracing::info!(
            event = ?event.kind,
            repository_namespace = event.repository.namespace.as_str(),
            repository_id = event.repository.repository_id.as_str(),
            build_id = event.build_id.as_str(),
            snapshot_id = event.snapshot_id.map(|id| id.as_str()),
            files = event.counters.files,
            nodes = event.counters.nodes,
            edges = event.counters.edges,
            diagnostics = event.counters.diagnostics,
            duration_ms = event.duration_ms,
            "repository graph lifecycle"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository_graph::domain::{BuildId, RepositoryId, RepositoryNamespace, SnapshotId};

    fn repository() -> RepositoryRef {
        RepositoryRef {
            namespace: RepositoryNamespace::new("local:test").unwrap(),
            repository_id: RepositoryId::new("root").unwrap(),
        }
    }

    #[test]
    fn lifecycle_event_wire_shape_has_no_free_form_payload_channel() {
        let build_id = BuildId::new("build-1").unwrap();
        let snapshot_id = SnapshotId::new("snapshot-1").unwrap();
        let repository = repository();
        let event = GraphLifecycleEvent {
            kind: LifecycleEventKind::SnapshotPublished,
            repository: &repository,
            build_id: &build_id,
            snapshot_id: Some(&snapshot_id),
            counters: LifecycleCounters {
                files: 2,
                nodes: 3,
                edges: 4,
                diagnostics: 0,
            },
            duration_ms: Some(10),
        };
        let value = serde_json::to_value(event).unwrap();
        for forbidden in ["message", "content", "body", "path", "metadata", "secret"] {
            assert!(value.get(forbidden).is_none());
        }
    }
}
