use std::collections::BTreeMap;

use crate::project_memory::{
    documents::RuntimeSourceDocument,
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
pub struct RuntimeProvenanceExtractor;

impl MemoryExtractor for RuntimeProvenanceExtractor {
    fn identity(&self) -> super::super::domain::MemoryExtractorIdentity {
        extractor_identity("ferrus.runtime-provenance")
    }

    fn supports(&self, category: MemorySourceCategory) -> bool {
        category == MemorySourceCategory::RuntimeProvenance
    }

    fn extract(
        &self,
        input: MemoryExtractionInput<'_>,
    ) -> Result<MemoryFragment, MemoryExtractionFailure> {
        let document: RuntimeSourceDocument =
            serde_json::from_slice(input.content).map_err(|_| failure("runtime.invalid"))?;
        let provenance = |record: &str| provenance(input, record);
        let mut fragment = MemoryFragment::default();
        let mut task_entities = BTreeMap::new();
        let mut run_entities = BTreeMap::new();

        for task in document.tasks {
            let task_record =
                MemoryRecordId::new(&task.id).map_err(|_| failure("runtime.invalid_task_id"))?;
            let task_entity = entity_id(&input.context.project, &("task", &task.id));
            task_entities.insert(task.id.clone(), task_entity.clone());
            let milestone_id = task
                .milestone_id
                .map(MemoryRecordId::new)
                .transpose()
                .map_err(|_| failure("runtime.invalid_milestone_id"))?;
            fragment.entities.push(MemoryEntity {
                project: input.context.project.clone(),
                memory_revision_id: input.context.revision_id.clone(),
                id: task_entity.clone(),
                data: MemoryEntityData::TaskReference {
                    task_id: task_record,
                    milestone_id: milestone_id.clone(),
                    status: MemoryStatusToken::new(task.status)
                        .map_err(|_| failure("runtime.invalid_status"))?,
                },
                provenance: provenance(&task.id)?,
            });
            if let Some(milestone_id) = milestone_id {
                fragment.relationships.push(MemoryRelationship {
                    project: input.context.project.clone(),
                    memory_revision_id: input.context.revision_id.clone(),
                    id: relationship_id(
                        &input.context.project,
                        &("implements", &task_entity, &milestone_id),
                    ),
                    kind: MemoryRelationshipKind::Implements,
                    source: task_entity,
                    target: MemoryRelationshipTarget::Milestone { milestone_id },
                    provenance: provenance(&task.id)?,
                });
            }
        }

        for run in document.runs {
            let run_record =
                MemoryRecordId::new(&run.id).map_err(|_| failure("runtime.invalid_run_id"))?;
            let task_record = MemoryRecordId::new(&run.task_id)
                .map_err(|_| failure("runtime.invalid_task_id"))?;
            let run_entity = entity_id(&input.context.project, &("run", &run.id));
            run_entities.insert(run.id.clone(), run_entity.clone());
            let check_ids = run
                .check_ids
                .into_iter()
                .map(MemoryRecordId::new)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| failure("runtime.invalid_check_id"))?;
            fragment.entities.push(MemoryEntity {
                project: input.context.project.clone(),
                memory_revision_id: input.context.revision_id.clone(),
                id: run_entity.clone(),
                data: MemoryEntityData::RunReference {
                    run_id: run_record,
                    task_id: task_record,
                    status: MemoryStatusToken::new(run.status)
                        .map_err(|_| failure("runtime.invalid_status"))?,
                    check_ids,
                },
                provenance: provenance(&run.id)?,
            });
            if let Some(task_entity) = task_entities.get(&run.task_id) {
                fragment.relationships.push(MemoryRelationship {
                    project: input.context.project.clone(),
                    memory_revision_id: input.context.revision_id.clone(),
                    id: relationship_id(
                        &input.context.project,
                        &("task-run", task_entity, &run_entity),
                    ),
                    kind: MemoryRelationshipKind::Contains,
                    source: task_entity.clone(),
                    target: MemoryRelationshipTarget::MemoryEntity {
                        entity_id: run_entity,
                    },
                    provenance: provenance(&run.id)?,
                });
            }
        }

        for check in document.checks {
            let check_record =
                MemoryRecordId::new(&check.id).map_err(|_| failure("runtime.invalid_check_id"))?;
            let check_entity = entity_id(&input.context.project, &("check", &check.id));
            fragment.entities.push(MemoryEntity {
                project: input.context.project.clone(),
                memory_revision_id: input.context.revision_id.clone(),
                id: check_entity.clone(),
                data: MemoryEntityData::ValidationEvidence {
                    text: None,
                    check_id: Some(check_record),
                    status: Some(
                        MemoryStatusToken::new(check.status)
                            .map_err(|_| failure("runtime.invalid_status"))?,
                    ),
                },
                provenance: provenance(&check.id)?,
            });
            let target = check
                .run_id
                .as_ref()
                .and_then(|run_id| run_entities.get(run_id))
                .or_else(|| task_entities.get(&check.task_id));
            if let Some(target) = target {
                fragment.relationships.push(MemoryRelationship {
                    project: input.context.project.clone(),
                    memory_revision_id: input.context.revision_id.clone(),
                    id: relationship_id(
                        &input.context.project,
                        &("validates", &check_entity, target),
                    ),
                    kind: MemoryRelationshipKind::Validates,
                    source: check_entity,
                    target: MemoryRelationshipTarget::MemoryEntity {
                        entity_id: target.clone(),
                    },
                    provenance: provenance(&check.id)?,
                });
            }
        }
        Ok(fragment)
    }
}

fn provenance(
    input: MemoryExtractionInput<'_>,
    record: &str,
) -> Result<MemoryProvenance, MemoryExtractionFailure> {
    Ok(MemoryProvenance {
        source_category: input.source.category,
        source_locator: input.source.locator.clone(),
        source_fingerprint: input.source.fingerprint.clone(),
        extractor: extractor_identity("ferrus.runtime-provenance"),
        evidence: MemoryEvidenceLocator::Record(
            MemoryRecordId::new(record).map_err(|_| failure("runtime.invalid_record_id"))?,
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
        extractor: extractor_identity("ferrus.runtime-provenance"),
        code: super::super::diagnostics::MemoryDiagnosticCode::new(code)
            .expect("static memory diagnostic code is valid"),
    }
}
