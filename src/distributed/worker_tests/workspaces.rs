//! Tests for workspaces.

use super::*;

#[test]
fn worker_reads_repository_manifests_through_the_object_size_boundary() {
    let repository_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repository_dir.path().join("src")).unwrap();
    std::fs::write(
        repository_dir.path().join("src/lib.rs"),
        b"pub struct Api;\n",
    )
    .unwrap();
    commit_repository(repository_dir.path());
    let config = RepositoryGraphConfig::default();
    let identities = builtin_extractor_identities();
    let context =
        SourceDiscoveryContext::from_config(local_repository(), &config, &identities).unwrap();
    let source = LocalRepositorySource::discover(repository_dir.path(), context).unwrap();
    let storage_dir = tempfile::tempdir().unwrap();
    let mut objects = object_store(&storage_dir.path().join("objects"));
    let manifest = package_repository_source(
        &source,
        remote_repository(),
        RepositoryPackagingPolicy {
            schema_version: 1,
            source_policy_digest: config.source_policy_digest().unwrap(),
        },
        packaging_limits(),
        &mut objects,
    )
    .unwrap();
    let manifest_bytes = objects
        .read_verified(&manifest.reference.manifest_object)
        .unwrap();
    let mut jobs = coordinator(&storage_dir.path().join("jobs.db"));
    let running = running_job(
        &mut jobs,
        IndexJobKind::RepositoryGraph,
        IndexInputRef::Repository(manifest.reference.clone()),
        IndexSemantics {
            semantic_config_digest: manifest.body.source_revision.analysis_config_digest.clone(),
            model_version: NonZeroU32::new(GRAPH_MODEL_VERSION).unwrap(),
            extractor_set_digest: manifest.body.extractor_set_digest.clone(),
        },
    );
    let request = execution_request(running);
    let mut limits = worker_limits();
    limits.max_object_bytes = NonZeroU64::new(
        u64::try_from(manifest_bytes.len())
            .unwrap()
            .saturating_sub(1),
    )
    .unwrap();
    let mut facts = fact_store(&storage_dir.path().join("facts.db"));

    assert_eq!(
        StatelessIndexWorker::new(limits).execute(&request, &jobs, &objects, &mut facts),
        Err(WorkerError::InputLimitExceeded)
    );
}

#[test]
fn repository_worker_extracts_cargo_facts_without_process_execution() {
    let repository_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repository_dir.path().join("src")).unwrap();
    std::fs::write(
        repository_dir.path().join("Cargo.toml"),
        b"[package]\nname = \"remote-app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nserde = \"1\"\n",
    )
    .unwrap();
    std::fs::write(
        repository_dir.path().join("src/lib.rs"),
        b"pub struct Api;\n",
    )
    .unwrap();
    commit_repository(repository_dir.path());
    let config = RepositoryGraphConfig::default();
    let identities = builtin_extractor_identities();
    let context =
        SourceDiscoveryContext::from_config(local_repository(), &config, &identities).unwrap();
    let source = LocalRepositorySource::discover(repository_dir.path(), context).unwrap();
    let storage_dir = tempfile::tempdir().unwrap();
    let mut objects = object_store(&storage_dir.path().join("objects"));
    let manifest = package_repository_source(
        &source,
        remote_repository(),
        RepositoryPackagingPolicy {
            schema_version: 1,
            source_policy_digest: config.source_policy_digest().unwrap(),
        },
        packaging_limits(),
        &mut objects,
    )
    .unwrap();
    let mut jobs = coordinator(&storage_dir.path().join("jobs.db"));
    let running = running_job(
        &mut jobs,
        IndexJobKind::RepositoryGraph,
        IndexInputRef::Repository(manifest.reference.clone()),
        IndexSemantics {
            semantic_config_digest: manifest.body.source_revision.analysis_config_digest.clone(),
            model_version: NonZeroU32::new(GRAPH_MODEL_VERSION).unwrap(),
            extractor_set_digest: manifest.body.extractor_set_digest.clone(),
        },
    );
    let request = execution_request(running);
    assert_eq!(
        request.sandbox.repository_execution,
        RepositoryExecutionPolicy::Denied
    );
    let mut facts = fact_store(&storage_dir.path().join("cargo-facts.db"));
    let bounded_objects = BoundedReadTracker::new(&objects);

    StatelessIndexWorker::new(worker_limits())
        .execute(&request, &jobs, &bounded_objects, &mut facts)
        .unwrap();
    assert_eq!(bounded_objects.reads.get(), manifest.body.files.len() + 1);
    let batches = facts.load_for_ingestion(&request.job.job).unwrap();
    let mut node_kinds = std::collections::BTreeSet::new();
    let mut diagnostic_codes = std::collections::BTreeSet::new();
    for batch in batches {
        let FactBatchPayload::RepositoryGraph {
            nodes, diagnostics, ..
        } = batch.payload
        else {
            panic!("repository worker emitted a memory fact batch");
        };
        node_kinds.extend(nodes.into_iter().map(|node| node.kind));
        diagnostic_codes.extend(
            diagnostics
                .into_iter()
                .map(|diagnostic| diagnostic.code.as_str().to_string()),
        );
    }

    assert!(node_kinds.contains("cargo_package"));
    assert!(node_kinds.contains("declared_dependency"));
    assert!(!diagnostic_codes.contains("cargo.parser_unavailable"));
}
