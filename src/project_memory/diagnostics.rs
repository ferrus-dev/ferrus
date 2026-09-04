//! Privacy-safe diagnostics and lifecycle events.

use std::{collections::BTreeMap, convert::Infallible, fmt};

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use super::{
    domain::{
        MemoryBuildId, MemoryEntityId, MemoryRelationshipId, MemoryRevisionId,
        MemorySourceCategory, ProjectRef,
    },
    ports::MemoryEventSink,
};

const MAX_DIAGNOSTIC_CODE_BYTES: usize = 64;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("diagnostic code must be a lowercase dotted token of at most 64 bytes")]
pub struct InvalidMemoryDiagnosticCode;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct MemoryDiagnosticCode(String);

impl MemoryDiagnosticCode {
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidMemoryDiagnosticCode> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= MAX_DIAGNOSTIC_CODE_BYTES
            && value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'.');
        valid
            .then_some(Self(value))
            .ok_or(InvalidMemoryDiagnosticCode)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MemoryDiagnosticCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for MemoryDiagnosticCode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryDiagnosticSeverity {
    Info,
    Warning,
    Error,
}

/// A bounded diagnostic with no free-form message or source body channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryDiagnostic {
    pub build_id: MemoryBuildId,
    pub revision_id: MemoryRevisionId,
    pub severity: MemoryDiagnosticSeverity,
    pub code: MemoryDiagnosticCode,
    pub source_category: Option<MemorySourceCategory>,
    pub entity_id: Option<MemoryEntityId>,
    pub relationship_id: Option<MemoryRelationshipId>,
    #[serde(default)]
    pub metrics: BTreeMap<MemoryDiagnosticCode, i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryLifecycleEventKind {
    BuildStarted,
    BuildFailed,
    RevisionCompleted,
    RevisionPublished,
    BuildSuperseded,
    PublicationConflict,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryLifecycleCounters {
    pub sources: u64,
    pub entities: u64,
    pub relationships: u64,
    pub stale_links: u64,
    pub diagnostics: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryLifecycleEvent<'a> {
    pub kind: MemoryLifecycleEventKind,
    pub project: &'a ProjectRef,
    pub build_id: &'a MemoryBuildId,
    pub revision_id: Option<&'a MemoryRevisionId>,
    pub counters: MemoryLifecycleCounters,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TracingMemoryEventSink;

impl MemoryEventSink for TracingMemoryEventSink {
    type Error = Infallible;

    fn emit(&self, event: MemoryLifecycleEvent<'_>) -> Result<(), Self::Error> {
        tracing::info!(
            event = ?event.kind,
            project_namespace = event.project.namespace.as_str(),
            project_id = event.project.project_id.as_str(),
            build_id = event.build_id.as_str(),
            revision_id = event.revision_id.map(|id| id.as_str()),
            sources = event.counters.sources,
            entities = event.counters.entities,
            relationships = event.counters.relationships,
            stale_links = event.counters.stale_links,
            diagnostics = event.counters.diagnostics,
            duration_ms = event.duration_ms,
            "project memory lifecycle"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! Diagnostic and lifecycle payloads exclude free-form text.

    use super::*;
    use crate::project_memory::domain::{ProjectId, ProjectNamespace};

    #[test]
    fn diagnostic_code_rejects_free_form_text() {
        assert!(MemoryDiagnosticCode::new("source.timeout").is_ok());
        assert!(MemoryDiagnosticCode::new("Source failed: /private/path").is_err());
    }

    #[test]
    fn lifecycle_event_has_no_free_form_payload_channel() {
        let project = ProjectRef {
            namespace: ProjectNamespace::new("local:test").unwrap(),
            project_id: ProjectId::new("project-1").unwrap(),
        };
        let build_id = MemoryBuildId::new("build-1").unwrap();
        let value = serde_json::to_value(MemoryLifecycleEvent {
            kind: MemoryLifecycleEventKind::BuildStarted,
            project: &project,
            build_id: &build_id,
            revision_id: None,
            counters: MemoryLifecycleCounters::default(),
            duration_ms: None,
        })
        .unwrap();
        for forbidden in ["message", "content", "body", "path", "metadata", "secret"] {
            assert!(value.get(forbidden).is_none());
        }
    }
}
