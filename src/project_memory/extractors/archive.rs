use crate::project_memory::{
    documents::ArchiveSourceDocument,
    domain::{
        MemoryConfidence, MemoryEntity, MemoryEntityData, MemoryEvidenceLocator, MemoryFragment,
        MemoryIndexTimestamps, MemoryProvenance, MemoryRecordId, MemoryRelationship,
        MemoryRelationshipKind, MemoryRelationshipTarget, MemoryResolutionState,
        MemorySourceCategory, MemoryStatusToken,
    },
    ports::{MemoryExtractionFailure, MemoryExtractionInput, MemoryExtractor},
};

use super::{entity_id, extractor_identity, relationship_id};

#[derive(Debug, Clone, Copy)]
pub struct ArchiveManifestExtractor;

impl MemoryExtractor for ArchiveManifestExtractor {
    fn identity(&self) -> super::super::domain::MemoryExtractorIdentity {
        extractor_identity("ferrus.archive-manifest")
    }

    fn supports(&self, category: MemorySourceCategory) -> bool {
        category == MemorySourceCategory::ArchiveManifest
    }

    fn extract(
        &self,
        input: MemoryExtractionInput<'_>,
    ) -> Result<MemoryFragment, MemoryExtractionFailure> {
        let document: ArchiveSourceDocument =
            serde_json::from_slice(input.content).map_err(|_| failure("archive.invalid"))?;
        let archive_id =
            MemoryRecordId::new(&document.archive_id).map_err(|_| failure("archive.invalid_id"))?;
        let entity_id = entity_id(&input.context.project, &("archive", &document.archive_id));
        let provenance = provenance(input, &document.archive_id)?;
        let mut fragment = MemoryFragment::default();
        fragment.entities.push(MemoryEntity {
            project: input.context.project.clone(),
            memory_revision_id: input.context.revision_id.clone(),
            id: entity_id.clone(),
            data: MemoryEntityData::ArchiveReference {
                archive_id,
                spec_path: document.spec_path,
                archived_at: MemoryStatusToken::new(document.archived_at)
                    .map_err(|_| failure("archive.invalid_timestamp"))?,
                task_count: document.task_count,
                run_count: document.run_count,
            },
            provenance: provenance.clone(),
        });
        for task_id in document.task_ids {
            let task_id =
                MemoryRecordId::new(task_id).map_err(|_| failure("runtime.invalid_task_id"))?;
            fragment.relationships.push(MemoryRelationship {
                project: input.context.project.clone(),
                memory_revision_id: input.context.revision_id.clone(),
                id: relationship_id(
                    &input.context.project,
                    &("archive-task", &entity_id, &task_id),
                ),
                kind: MemoryRelationshipKind::Contains,
                source: entity_id.clone(),
                target: MemoryRelationshipTarget::Task { task_id },
                provenance: provenance.clone(),
            });
        }
        for milestone_id in document.milestone_ids {
            let milestone_id = MemoryRecordId::new(milestone_id)
                .map_err(|_| failure("runtime.invalid_milestone_id"))?;
            fragment.relationships.push(MemoryRelationship {
                project: input.context.project.clone(),
                memory_revision_id: input.context.revision_id.clone(),
                id: relationship_id(
                    &input.context.project,
                    &("archive-milestone", &entity_id, &milestone_id),
                ),
                kind: MemoryRelationshipKind::Concerns,
                source: entity_id.clone(),
                target: MemoryRelationshipTarget::Milestone { milestone_id },
                provenance: provenance.clone(),
            });
        }
        Ok(fragment)
    }
}

fn provenance(
    input: MemoryExtractionInput<'_>,
    archive_id: &str,
) -> Result<MemoryProvenance, MemoryExtractionFailure> {
    Ok(MemoryProvenance {
        source_category: input.source.category,
        source_locator: input.source.locator.clone(),
        source_fingerprint: input.source.fingerprint.clone(),
        extractor: extractor_identity("ferrus.archive-manifest"),
        evidence: MemoryEvidenceLocator::Record(
            MemoryRecordId::new(archive_id).map_err(|_| failure("archive.invalid_id"))?,
        ),
        resolution: MemoryResolutionState::Resolved,
        confidence: MemoryConfidence::Exact,
        timestamps: MemoryIndexTimestamps {
            source_observed_at: input.context.indexed_at,
            indexed_at: input.context.indexed_at,
        },
    })
}

fn failure(code: &str) -> MemoryExtractionFailure {
    MemoryExtractionFailure {
        extractor: extractor_identity("ferrus.archive-manifest"),
        code: super::super::diagnostics::MemoryDiagnosticCode::new(code)
            .expect("static memory diagnostic code is valid"),
    }
}
