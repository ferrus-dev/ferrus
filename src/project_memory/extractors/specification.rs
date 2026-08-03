use crate::project_memory::{
    documents::{OutcomeSectionKind, parse_spec_memory},
    domain::{
        MemoryConfidence, MemoryEntity, MemoryEntityData, MemoryEvidenceLocator, MemoryFragment,
        MemoryIndexTimestamps, MemoryProvenance, MemoryRelationship, MemoryRelationshipKind,
        MemoryRelationshipTarget, MemoryResolutionState, MemorySourceCategory, MemoryText,
        MemoryTitle, MilestoneCompletion,
    },
    ports::{MemoryExtractionFailure, MemoryExtractionInput, MemoryExtractor},
};

use super::{entity_id, extractor_identity, relationship_id};

#[derive(Debug, Clone, Copy)]
pub struct SpecificationExtractor;

impl MemoryExtractor for SpecificationExtractor {
    fn identity(&self) -> super::super::domain::MemoryExtractorIdentity {
        extractor_identity("ferrus.specification")
    }

    fn supports(&self, category: MemorySourceCategory) -> bool {
        matches!(
            category,
            MemorySourceCategory::SpecificationStructure | MemorySourceCategory::ApprovedOutcome
        )
    }

    fn extract(
        &self,
        input: MemoryExtractionInput<'_>,
    ) -> Result<MemoryFragment, MemoryExtractionFailure> {
        let content = std::str::from_utf8(input.content).map_err(|_| failure("source.non_utf8"))?;
        let parsed = parse_spec_memory(content);
        match input.source.category {
            MemorySourceCategory::SpecificationStructure => {
                extract_structure(input, parsed).map_err(|_| failure("spec.invalid"))
            }
            MemorySourceCategory::ApprovedOutcome => {
                extract_outcome(input, parsed).map_err(|_| failure("outcome.invalid"))
            }
            _ => Err(failure("source.unsupported")),
        }
    }
}

fn extract_structure(
    input: MemoryExtractionInput<'_>,
    parsed: crate::project_memory::documents::ParsedSpecMemory,
) -> Result<MemoryFragment, ()> {
    let title = parsed.structure.title.ok_or(())?;
    let title_span = parsed.title_span.ok_or(())?;
    let spec_id = entity_id(
        &input.context.project,
        &("specification", &input.source.locator),
    );
    let mut fragment = MemoryFragment::default();
    fragment.entities.push(MemoryEntity {
        project: input.context.project.clone(),
        memory_revision_id: input.context.revision_id.clone(),
        id: spec_id.clone(),
        data: MemoryEntityData::Specification {
            title: MemoryTitle::new(title).map_err(|_| ())?,
        },
        provenance: provenance(input, title_span),
    });
    for milestone in parsed.structure.milestones {
        let milestone_id =
            super::super::domain::MemoryRecordId::new(&milestone.id).map_err(|_| ())?;
        let entity_id = entity_id(
            &input.context.project,
            &("milestone", &input.source.locator, &milestone.id),
        );
        let evidence = milestone.span.clone();
        fragment.entities.push(MemoryEntity {
            project: input.context.project.clone(),
            memory_revision_id: input.context.revision_id.clone(),
            id: entity_id.clone(),
            data: MemoryEntityData::Milestone {
                milestone_id,
                title: MemoryTitle::new(milestone.title).map_err(|_| ())?,
                completion: if milestone.completed {
                    MilestoneCompletion::Complete
                } else {
                    MilestoneCompletion::Pending
                },
            },
            provenance: provenance(input, evidence.clone()),
        });
        fragment.relationships.push(MemoryRelationship {
            project: input.context.project.clone(),
            memory_revision_id: input.context.revision_id.clone(),
            id: relationship_id(&input.context.project, &("contains", &spec_id, &entity_id)),
            kind: MemoryRelationshipKind::Contains,
            source: spec_id.clone(),
            target: MemoryRelationshipTarget::MemoryEntity { entity_id },
            provenance: provenance(input, evidence),
        });
    }
    Ok(fragment)
}

fn extract_outcome(
    input: MemoryExtractionInput<'_>,
    parsed: crate::project_memory::documents::ParsedSpecMemory,
) -> Result<MemoryFragment, ()> {
    let outcome = parsed.outcome.ok_or(())?;
    let outcome_id = entity_id(&input.context.project, &("outcome", &input.source.locator));
    let spec_id = entity_id(
        &input.context.project,
        &("specification", &input.source.locator),
    );
    let mut fragment = MemoryFragment::default();
    fragment.entities.push(MemoryEntity {
        project: input.context.project.clone(),
        memory_revision_id: input.context.revision_id.clone(),
        id: outcome_id.clone(),
        data: MemoryEntityData::Outcome {
            text: MemoryText::new(outcome.text).map_err(|_| ())?,
        },
        provenance: provenance(input, outcome.span.clone()),
    });
    fragment.relationships.push(MemoryRelationship {
        project: input.context.project.clone(),
        memory_revision_id: input.context.revision_id.clone(),
        id: relationship_id(&input.context.project, &("concerns", &outcome_id, &spec_id)),
        kind: MemoryRelationshipKind::Concerns,
        source: outcome_id.clone(),
        target: MemoryRelationshipTarget::MemoryEntity { entity_id: spec_id },
        provenance: provenance(input, outcome.span),
    });
    let mut section_occurrences = std::collections::BTreeMap::new();
    for section in outcome.sections {
        let section_digest = super::canonical_digest(&section.text);
        let occurrence = section_occurrences
            .entry((section.kind, section_digest.clone()))
            .or_insert(0u32);
        let section_id = entity_id(
            &input.context.project,
            &(
                "outcome-section",
                &input.source.locator,
                section.kind,
                &section_digest,
                *occurrence,
            ),
        );
        *occurrence += 1;
        let text = MemoryText::new(section.text).map_err(|_| ())?;
        let data = match section.kind {
            OutcomeSectionKind::Decision => MemoryEntityData::Decision { text },
            OutcomeSectionKind::Deviation => MemoryEntityData::Deviation { text },
            OutcomeSectionKind::Validation => MemoryEntityData::ValidationEvidence {
                text: Some(text),
                check_id: None,
                status: None,
            },
            OutcomeSectionKind::FollowUp => MemoryEntityData::FollowUpWork {
                text,
                milestone_id: None,
            },
        };
        let evidence = section.span;
        fragment.entities.push(MemoryEntity {
            project: input.context.project.clone(),
            memory_revision_id: input.context.revision_id.clone(),
            id: section_id.clone(),
            data,
            provenance: provenance(input, evidence.clone()),
        });
        fragment.relationships.push(MemoryRelationship {
            project: input.context.project.clone(),
            memory_revision_id: input.context.revision_id.clone(),
            id: relationship_id(
                &input.context.project,
                &("contains", &outcome_id, &section_id),
            ),
            kind: MemoryRelationshipKind::Contains,
            source: outcome_id.clone(),
            target: MemoryRelationshipTarget::MemoryEntity {
                entity_id: section_id,
            },
            provenance: provenance(input, evidence),
        });
    }
    Ok(fragment)
}

fn provenance(
    input: MemoryExtractionInput<'_>,
    span: crate::repository_graph::domain::SourceSpan,
) -> MemoryProvenance {
    MemoryProvenance {
        source_category: input.source.category,
        source_locator: input.source.locator.clone(),
        source_fingerprint: input.source.fingerprint.clone(),
        extractor: extractor_identity("ferrus.specification"),
        evidence: MemoryEvidenceLocator::Span(span),
        resolution: MemoryResolutionState::Resolved,
        confidence: MemoryConfidence::Exact,
        timestamps: MemoryIndexTimestamps {
            source_observed_at: input.context.indexed_at,
            indexed_at: input.context.indexed_at,
        },
    }
}

fn failure(code: &str) -> MemoryExtractionFailure {
    MemoryExtractionFailure {
        extractor: extractor_identity("ferrus.specification"),
        code: super::super::diagnostics::MemoryDiagnosticCode::new(code)
            .expect("static memory diagnostic code is valid"),
    }
}
