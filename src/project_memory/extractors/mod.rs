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

pub fn built_in_extractor_set_digest() -> Digest {
    let identities = built_in_extractors()
        .iter()
        .map(|extractor| extractor.identity())
        .collect::<Vec<_>>();
    canonical_digest(&identities)
}

pub(crate) fn extractor_identity(id: &str) -> MemoryExtractorIdentity {
    MemoryExtractorIdentity::current(
        MemoryExtractorId::new(id).expect("built-in memory extractor id is valid"),
        MemoryStatusToken::new(env!("CARGO_PKG_VERSION"))
            .expect("package version is a bounded token"),
    )
}

pub(crate) fn entity_id(project: &ProjectRef, key: &impl Serialize) -> MemoryEntityId {
    let digest = canonical_digest(&("entity", project, key));
    MemoryEntityId::new(format!("memory-entity:{}", digest.value()))
        .expect("sha256 memory entity id is bounded")
}

pub(crate) fn relationship_id(project: &ProjectRef, key: &impl Serialize) -> MemoryRelationshipId {
    let digest = canonical_digest(&("relationship", project, key));
    MemoryRelationshipId::new(format!("memory-relationship:{}", digest.value()))
        .expect("sha256 memory relationship id is bounded")
}

pub(crate) fn canonical_digest(value: &impl Serialize) -> Digest {
    let encoded = serde_json::to_vec(value).expect("memory extractor identity is serializable");
    let value = Sha256::digest(encoded)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Digest::new("sha256", value).expect("sha256 output is lowercase hexadecimal")
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
            built_in_extractor_set_digest(),
            built_in_extractor_set_digest()
        );
    }
}
