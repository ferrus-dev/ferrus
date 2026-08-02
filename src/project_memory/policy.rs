//! Explicit source authorization and privacy defaults.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest as ShaDigest, Sha256};

use crate::repository_graph::domain::Digest;

use super::domain::MemorySourceCategory;

pub const MEMORY_POLICY_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemorySourceSensitivity {
    Curated,
    OperationalMetadata,
    Sensitive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryContentAccess {
    StructureOnly,
    CuratedSections,
    IdentityAndCountsOnly,
    RawBody,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemorySourcePolicy {
    pub enabled: bool,
    pub sensitivity: MemorySourceSensitivity,
    pub content_access: MemoryContentAccess,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryPolicy {
    pub schema_version: u32,
    pub categories: BTreeMap<MemorySourceCategory, MemorySourcePolicy>,
}

impl Default for MemoryPolicy {
    fn default() -> Self {
        let mut categories = BTreeMap::new();
        for category in MemorySourceCategory::ALL {
            let policy = match category {
                MemorySourceCategory::SpecificationStructure => MemorySourcePolicy {
                    enabled: true,
                    sensitivity: MemorySourceSensitivity::Curated,
                    content_access: MemoryContentAccess::StructureOnly,
                },
                MemorySourceCategory::ApprovedOutcome => MemorySourcePolicy {
                    enabled: true,
                    sensitivity: MemorySourceSensitivity::Curated,
                    content_access: MemoryContentAccess::CuratedSections,
                },
                MemorySourceCategory::ArchiveManifest | MemorySourceCategory::RuntimeProvenance => {
                    MemorySourcePolicy {
                        enabled: true,
                        sensitivity: MemorySourceSensitivity::OperationalMetadata,
                        content_access: MemoryContentAccess::IdentityAndCountsOnly,
                    }
                }
                _ => MemorySourcePolicy {
                    enabled: false,
                    sensitivity: MemorySourceSensitivity::Sensitive,
                    content_access: MemoryContentAccess::RawBody,
                },
            };
            categories.insert(category, policy);
        }
        Self {
            schema_version: MEMORY_POLICY_SCHEMA_VERSION,
            categories,
        }
    }
}

impl MemoryPolicy {
    pub fn digest(&self) -> Digest {
        let encoded = serde_json::to_vec(self).expect("memory policy is serializable");
        let value = Sha256::digest(encoded)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        Digest::new("sha256", value).expect("sha256 output is lowercase hexadecimal")
    }

    pub fn category(&self, category: MemorySourceCategory) -> Option<&MemorySourcePolicy> {
        self.categories.get(&category)
    }

    pub fn is_authorized(&self, category: MemorySourceCategory) -> bool {
        self.category(category).is_some_and(|policy| policy.enabled)
    }

    pub fn authorized_categories(&self) -> impl Iterator<Item = MemorySourceCategory> + '_ {
        self.categories
            .iter()
            .filter_map(|(category, policy)| policy.enabled.then_some(*category))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_authorizes_only_curated_content_and_bounded_metadata() {
        let policy = MemoryPolicy::default();
        assert_eq!(policy.categories.len(), MemorySourceCategory::ALL.len());
        assert_eq!(policy.authorized_categories().count(), 4);
        assert!(policy.is_authorized(MemorySourceCategory::SpecificationStructure));
        assert!(policy.is_authorized(MemorySourceCategory::ApprovedOutcome));
        assert!(policy.is_authorized(MemorySourceCategory::ArchiveManifest));
        assert!(policy.is_authorized(MemorySourceCategory::RuntimeProvenance));
        assert!(!policy.is_authorized(MemorySourceCategory::TaskBody));
        assert_eq!(
            policy.category(MemorySourceCategory::PatchBody),
            Some(&MemorySourcePolicy {
                enabled: false,
                sensitivity: MemorySourceSensitivity::Sensitive,
                content_access: MemoryContentAccess::RawBody,
            })
        );
    }

    #[test]
    fn policy_digest_is_deterministic() {
        assert_eq!(
            MemoryPolicy::default().digest(),
            MemoryPolicy::default().digest()
        );
    }
}
