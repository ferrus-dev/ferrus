//! Deterministic built-in project-memory extractors.

mod archive;
mod runtime;
mod specification;

use serde::Serialize;
use sha2::{Digest as ShaDigest, Sha256};

use crate::repository_graph::domain::Digest;

use super::{
    domain::{
        MemoryEntityId, MemoryExtractorId, MemoryExtractorIdentity, MemoryRelationshipId,
        MemoryStatusToken, ProjectRef,
    },
    ports::MemoryExtractor,
};

pub use archive::ArchiveManifestExtractor;
pub use runtime::RuntimeProvenanceExtractor;
pub use specification::SpecificationExtractor;

pub fn built_in_extractors() -> Vec<Box<dyn MemoryExtractor>> {
    vec![
        Box::new(SpecificationExtractor),
        Box::new(ArchiveManifestExtractor),
        Box::new(RuntimeProvenanceExtractor),
    ]
}

pub fn built_in_extractor_set_digest() -> Result<Digest, serde_json::Error> {
    let identities = built_in_extractors()
        .iter()
        .map(|extractor| extractor.identity())
        .collect::<Vec<_>>();
    canonical_digest(&identities)
}

pub(crate) fn extractor_identity(id: MemoryExtractorId) -> MemoryExtractorIdentity {
    MemoryExtractorIdentity::current(
        id,
        const { MemoryStatusToken::from_static(env!("CARGO_PKG_VERSION")) },
    )
}

pub(crate) fn entity_id(
    project: &ProjectRef,
    key: &impl Serialize,
) -> Result<MemoryEntityId, serde_json::Error> {
    let digest = canonical_digest(&("entity", project, key))?;
    MemoryEntityId::new(format!("memory-entity:{}", digest.value()))
        .map_err(serde::ser::Error::custom)
}

pub(crate) fn relationship_id(
    project: &ProjectRef,
    key: &impl Serialize,
) -> Result<MemoryRelationshipId, serde_json::Error> {
    let digest = canonical_digest(&("relationship", project, key))?;
    MemoryRelationshipId::new(format!("memory-relationship:{}", digest.value()))
        .map_err(serde::ser::Error::custom)
}

pub(crate) fn canonical_digest(value: &impl Serialize) -> Result<Digest, serde_json::Error> {
    let encoded = serde_json::to_vec(value)?;
    Ok(Digest::from_sha256(Sha256::digest(encoded).into()))
}

#[cfg(test)]
mod tests {
    //! Stable and unique built-in memory extractor registration.

    use super::*;

    #[test]
    fn built_in_extractor_set_is_stable_and_unique() {
        let extractors = built_in_extractors();
        let ids = extractors
            .iter()
            .map(|extractor| extractor.identity().id)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(ids.len(), extractors.len());
        assert_eq!(
            built_in_extractor_set_digest().unwrap(),
            built_in_extractor_set_digest().unwrap()
        );
    }
    #[test]
    fn malformed_sources_return_valid_diagnostics_instead_of_panicking() {
        use crate::project_memory::{
            domain::{
                AuthorizedSourceDescriptor, MemoryBuildId, MemoryRevisionId, MemorySourceCategory,
                MemorySourceLocator, ProjectId, ProjectNamespace,
            },
            ports::{MemoryExtractionContext, MemoryExtractionInput},
        };
        use crate::repository_graph::domain::RepoPath;

        let project = ProjectRef {
            namespace: ProjectNamespace::new("local:test").unwrap(),
            project_id: ProjectId::new("test-project").unwrap(),
        };
        let context = MemoryExtractionContext {
            project: project.clone(),
            revision_id: MemoryRevisionId::new("revision-1").unwrap(),
            build_id: MemoryBuildId::new("build-1").unwrap(),
            indexed_at: chrono::Utc::now(),
            max_entities_per_source: 100,
            max_relationships_per_source: 100,
            max_parser_duration_ms: 1000,
            max_diagnostics: 100,
        };
        let cases: Vec<(Box<dyn MemoryExtractor>, _, _, _)> = vec![
            (
                Box::new(SpecificationExtractor),
                MemorySourceCategory::SpecificationStructure,
                vec![0xff],
                "source.non.utf8",
            ),
            (
                Box::new(ArchiveManifestExtractor),
                MemorySourceCategory::ArchiveManifest,
                serde_json::to_vec(&serde_json::json!({
                    "archive_id":"", "spec_path":"docs/spec.md", "archived_at":"2026-10-25",
                    "task_count":0, "run_count":0, "task_ids":[], "milestone_ids":[]
                }))
                .unwrap(),
                "archive.invalid.id",
            ),
            (
                Box::new(RuntimeProvenanceExtractor),
                MemorySourceCategory::RuntimeProvenance,
                serde_json::to_vec(&serde_json::json!({
                    "tasks":[{"id":"", "status":"complete"}], "runs":[], "checks":[]
                }))
                .unwrap(),
                "runtime.invalid.task.id",
            ),
        ];
        for (extractor, category, content, expected) in cases {
            let source = AuthorizedSourceDescriptor {
                project: project.clone(),
                category,
                locator: MemorySourceLocator::TrackedFile {
                    path: RepoPath::new("docs/spec.md").unwrap(),
                },
                fingerprint: canonical_digest(&content).unwrap(),
                byte_len: content.len() as u64,
            };
            let error = extractor
                .extract(MemoryExtractionInput {
                    context: &context,
                    source: &source,
                    content: &content,
                })
                .unwrap_err();
            assert_eq!(error.code.as_str(), expected);
            assert_eq!(
                super::super::diagnostics::MemoryDiagnosticCode::new(expected).unwrap(),
                error.code,
            );
        }
    }
}
