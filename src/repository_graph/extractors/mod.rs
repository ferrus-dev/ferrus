//! Deterministic, storage-independent repository graph extractors.
//!
//! Extractors consume only immutable manifest metadata plus verified content.
//! They never access SQLite, invoke project tools, or execute repository code.

pub mod cargo;
pub mod generic;
pub mod rust;

use sha2::{Digest as _, Sha256};

use super::{
    domain::{EdgeId, EdgeTarget, ExtractorIdentity, NodeId, SnapshotId},
    ports::Extractor,
};

/// Canonical built-in extractor set used in source and snapshot identity.
pub fn builtin_extractor_identities() -> Vec<ExtractorIdentity> {
    vec![
        generic::GenericExtractor.identity(),
        cargo::CargoExtractor.identity(),
        rust::RustSyntaxExtractor.identity(),
    ]
}

pub(crate) fn deterministic_node_id(
    snapshot_id: &SnapshotId,
    extractor: &ExtractorIdentity,
    kind: &str,
    local_key: &str,
) -> NodeId {
    NodeId::new(format!(
        "node:{}",
        framed_digest(&[
            snapshot_id.as_str(),
            extractor.id.as_str(),
            &extractor.version,
            kind,
            local_key,
        ])
    ))
    .expect("a prefixed sha256 digest is never empty")
}

pub(crate) fn deterministic_edge_id(
    snapshot_id: &SnapshotId,
    extractor: &ExtractorIdentity,
    kind: &str,
    source: &NodeId,
    target: &EdgeTarget,
    local_key: &str,
) -> EdgeId {
    let target = match target {
        EdgeTarget::Node(id) => format!("node:{}", id.as_str()),
        EdgeTarget::External(value) => format!("external:{value}"),
        EdgeTarget::Unresolved(value) => format!("unresolved:{value}"),
    };
    EdgeId::new(format!(
        "edge:{}",
        framed_digest(&[
            snapshot_id.as_str(),
            extractor.id.as_str(),
            &extractor.version,
            kind,
            source.as_str(),
            &target,
            local_key,
        ])
    ))
    .expect("a prefixed sha256 digest is never empty")
}

fn framed_digest(parts: &[&str]) -> String {
    let mut digest = Sha256::new();
    for part in parts {
        let bytes = part.as_bytes();
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
    }
    let bytes = digest.finalize();
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_frames_canonical_parts() {
        assert_ne!(framed_digest(&["ab", "c"]), framed_digest(&["a", "bc"]));
        assert_eq!(framed_digest(&["ab", "c"]), framed_digest(&["ab", "c"]));
    }

    #[test]
    fn built_in_extractor_set_is_stable_and_unique() {
        let first = builtin_extractor_identities();
        let second = builtin_extractor_identities();
        assert_eq!(first, second);
        assert_eq!(first.len(), 3);
        let mut ids = first
            .iter()
            .map(|identity| identity.id.as_str())
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 3);
    }
}
