//! Archive proof, authorized source discovery, and privacy boundaries.

use super::*;
use std::num::NonZeroU64;

use crate::project_memory::{domain::MemoryEvidenceLocator, query::MemoryTruncationReason};
use rusqlite::params;
use tempfile::TempDir;

fn project_ref() -> ProjectRef {
    ProjectRef {
        namespace: ProjectNamespace::new("local:test").unwrap(),
        project_id: ProjectId::new("project-1").unwrap(),
    }
}

fn init_git(root: &Path) {
    assert!(
        Command::new("git")
            .arg("init")
            .arg(root)
            .status()
            .unwrap()
            .success()
    );
    fs::create_dir_all(root.join("docs/specs")).unwrap();
}

fn add_tracked(root: &Path, path: &str, content: &str) {
    fs::write(root.join(path), content).unwrap();
    assert!(
        Command::new("git")
            .current_dir(root)
            .args(["add", "--", path])
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn approved_outcomes_require_exact_archive_proof_without_affecting_structure() {
    let root = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    init_git(root.path());
    let path = "docs/specs/example.md";
    let first_content =
        "# Example\n\n- [x] #1.0 Done\n\nID: one\nDepends on: none\n\n## Outcome\n\nFirst.\n";
    add_tracked(root.path(), path, first_content);
    let unproved = LocalMemorySource::discover_at(
        root.path().to_path_buf(),
        data.path().to_path_buf(),
        project_ref(),
        RepoPath::new("docs/specs").unwrap(),
        MemoryPolicy::default(),
    )
    .unwrap();
    assert!(
        unproved
            .manifest
            .sources
            .iter()
            .all(|source| { source.category != MemorySourceCategory::ApprovedOutcome })
    );

    record_approved_outcome_for_test(data.path(), path, first_content);
    let approved = LocalMemorySource::discover_at(
        root.path().to_path_buf(),
        data.path().to_path_buf(),
        project_ref(),
        RepoPath::new("docs/specs").unwrap(),
        MemoryPolicy::default(),
    )
    .unwrap();
    assert!(
        approved
            .manifest
            .sources
            .iter()
            .any(|source| { source.category == MemorySourceCategory::ApprovedOutcome })
    );

    fs::write(
        root.path().join(path),
        "# Example\n\n- [x] #1.0 Done\n\nID: one\nDepends on: none\n\n## Outcome\n\nSecond.\n",
    )
    .unwrap();
    let second = LocalMemorySource::discover_at(
        root.path().to_path_buf(),
        data.path().to_path_buf(),
        project_ref(),
        RepoPath::new("docs/specs").unwrap(),
        MemoryPolicy::default(),
    )
    .unwrap();
    let structure_fingerprint = |source: &LocalMemorySource| {
        source.manifest.sources.iter().find_map(|source| {
            (source.category == MemorySourceCategory::SpecificationStructure)
                .then(|| source.fingerprint.clone())
        })
    };
    assert_eq!(
        structure_fingerprint(&approved),
        structure_fingerprint(&second)
    );
    assert!(
        second
            .manifest
            .sources
            .iter()
            .all(|source| { source.category != MemorySourceCategory::ApprovedOutcome })
    );
}

#[test]
fn archive_manifest_sources_require_durable_archive_records() {
    let root = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    init_git(root.path());
    let spec_path = "docs/specs/example.md";
    let spec_content = "# Example\n\n## Outcome\n\nApproved.\n";
    record_approved_outcome_for_test(data.path(), spec_path, spec_content);

    let orphan = data.path().join("archive/specs/orphan");
    fs::create_dir_all(&orphan).unwrap();
    fs::write(
        orphan.join("manifest.toml"),
        "spec_path = \"docs/specs/orphan.md\"\narchived_at = \"2026-08-14T00:00:00Z\"\ntasks = []\n",
    )
    .unwrap();

    let source = LocalMemorySource::discover_at(
        root.path().to_path_buf(),
        data.path().to_path_buf(),
        project_ref(),
        RepoPath::new("docs/specs").unwrap(),
        MemoryPolicy::default(),
    )
    .unwrap();
    let archive_ids = source
        .manifest
        .sources
        .iter()
        .filter_map(|source| match &source.locator {
            MemorySourceLocator::ArchiveManifest { archive_id } => Some(archive_id.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(archive_ids, vec!["proof-1"]);
}

#[test]
fn runtime_source_serializes_only_bounded_provenance_fields() {
    let root = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    init_git(root.path());
    let connection = Connection::open(data.path().join("ferrus.db")).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE tasks(id TEXT, milestone_id TEXT, status TEXT, spec_path TEXT, \
                failure_reason TEXT, baseline_snapshot_id TEXT, \
                repository_view_snapshot_id TEXT); \
             CREATE TABLE runs(id TEXT, task_id TEXT, status TEXT, agent TEXT, pid INTEGER, \
                workspace_path TEXT, baseline_snapshot_id TEXT, \
                repository_view_snapshot_id TEXT); \
             CREATE TABLE events(id INTEGER, run_id TEXT, type TEXT, payload_json TEXT);",
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO tasks VALUES (?1, ?2, 'complete', ?3, ?4, NULL, NULL)",
            params!["t-1", "one", "docs/specs/example.md", "do not persist"],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO runs VALUES (?1, ?2, 'completed', ?3, 999, ?4, NULL, NULL)",
            params!["run-1", "t-1", "private-agent", "/private/worktree"],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO runs VALUES (?1, ?2, 'running', ?3, 1000, ?4, NULL, NULL)",
            params!["run-2", "t-1", "active-agent", "/active/worktree"],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO events VALUES (1, ?1, 'check_passed', ?2)",
            params!["run-1", r#"{"commands":3,"secret":"hidden"}"#],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO events VALUES (2, ?1, 'check_failed', ?2)",
            params!["run-2", r#"{"commands":4,"task_id":"t-1"}"#],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO events VALUES (3, NULL, 'task_check_failed', ?1)",
            params![r#"{"task_id":"t-1"}"#],
        )
        .unwrap();
    let source = LocalMemorySource::discover_at(
        root.path().to_path_buf(),
        data.path().to_path_buf(),
        project_ref(),
        RepoPath::new("docs/specs").unwrap(),
        MemoryPolicy::default(),
    )
    .unwrap();
    let runtime = source
        .materials
        .iter()
        .find(|material| material.descriptor.category == MemorySourceCategory::RuntimeProvenance)
        .unwrap();
    let runtime_descriptor = runtime.descriptor.clone();
    let MaterialContent::Sanitized(bytes) = &runtime.content else {
        panic!("runtime content must be sanitized");
    };
    let value = String::from_utf8(bytes.clone()).unwrap();
    for forbidden in [
        "do not persist",
        "private-agent",
        "/private/worktree",
        "hidden",
        "pid",
        "failure_reason",
    ] {
        assert!(!value.contains(forbidden));
    }

    assert!(value.contains("event:1"));
    assert!(value.contains("event:3"));
    assert!(!value.contains("run-2"));
    assert!(!value.contains("event:2"));
    let error = source
        .content(MemoryContentRequest {
            project: project_ref(),
            revision_id: source.manifest.revision_id().unwrap(),
            source_category: runtime_descriptor.category,
            locator: runtime_descriptor.locator,
            expected_fingerprint: runtime_descriptor.fingerprint,
            evidence: Some(MemoryEvidenceLocator::Record(
                MemoryRecordId::new("task:t-1").unwrap(),
            )),
            max_bytes: NonZeroU64::new(1024).unwrap(),
        })
        .unwrap_err();
    assert_eq!(error, MemoryQueryError::SourceNotAuthorized);
}

#[test]
fn verified_memory_content_clamps_utf8_and_rejects_changed_fingerprints() {
    let root = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    init_git(root.path());
    add_tracked(
        root.path(),
        "docs/specs/example.md",
        "# Example\n\n## Outcome\n\ncaf\u{00e9}\n",
    );
    record_approved_outcome_for_test(
        data.path(),
        "docs/specs/example.md",
        "# Example\n\n## Outcome\n\ncaf\u{00e9}\n",
    );
    let source = LocalMemorySource::discover_at(
        root.path().to_path_buf(),
        data.path().to_path_buf(),
        project_ref(),
        RepoPath::new("docs/specs").unwrap(),
        MemoryPolicy::default(),
    )
    .unwrap();
    let descriptor = source
        .manifest
        .sources
        .iter()
        .find(|source| source.category == MemorySourceCategory::ApprovedOutcome)
        .unwrap();
    let content = fs::read(root.path().join("docs/specs/example.md")).unwrap();
    let start = content
        .windows("caf\u{00e9}".len())
        .position(|window| window == "caf\u{00e9}".as_bytes())
        .unwrap() as u64;
    let response = source
        .content(MemoryContentRequest {
            project: project_ref(),
            revision_id: source.manifest.revision_id().unwrap(),
            source_category: descriptor.category,
            locator: descriptor.locator.clone(),
            expected_fingerprint: descriptor.fingerprint.clone(),
            evidence: Some(MemoryEvidenceLocator::Span(
                crate::repository_graph::domain::SourceSpan {
                    start: crate::repository_graph::domain::SourcePosition {
                        byte_offset: start,
                        line: None,
                        column: None,
                    },
                    end: crate::repository_graph::domain::SourcePosition {
                        byte_offset: start + 5,
                        line: None,
                        column: None,
                    },
                },
            )),
            max_bytes: NonZeroU64::new(4).unwrap(),
        })
        .unwrap();
    assert!(std::str::from_utf8(&response.bytes).is_ok());
    assert_eq!(response.bytes, b"caf");
    assert!(response.truncated);

    let error = source
        .content(MemoryContentRequest {
            project: project_ref(),
            revision_id: source.manifest.revision_id().unwrap(),
            source_category: descriptor.category,
            locator: descriptor.locator.clone(),
            expected_fingerprint: descriptor.fingerprint.clone(),
            evidence: Some(MemoryEvidenceLocator::Record(
                MemoryRecordId::new("outcome").unwrap(),
            )),
            max_bytes: NonZeroU64::new(1024).unwrap(),
        })
        .unwrap_err();
    assert_eq!(error, MemoryQueryError::SourceNotAuthorized);

    let error = source
        .content(MemoryContentRequest {
            project: project_ref(),
            revision_id: source.manifest.revision_id().unwrap(),
            source_category: descriptor.category,
            locator: descriptor.locator.clone(),
            expected_fingerprint: Digest::new("sha256", "00").unwrap(),
            evidence: Some(MemoryEvidenceLocator::Span(
                crate::repository_graph::domain::SourceSpan {
                    start: crate::repository_graph::domain::SourcePosition {
                        byte_offset: start,
                        line: None,
                        column: None,
                    },
                    end: crate::repository_graph::domain::SourcePosition {
                        byte_offset: start + 5,
                        line: None,
                        column: None,
                    },
                },
            )),
            max_bytes: NonZeroU64::new(4).unwrap(),
        })
        .unwrap_err();
    assert_eq!(error, MemoryQueryError::ContentChanged);
}

#[test]
fn specification_fingerprint_binds_title_evidence_layout() {
    let root = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    init_git(root.path());
    let path = "docs/specs/example.md";
    let initial = "# Example\n\nPrivate body.\n";
    add_tracked(root.path(), path, initial);

    let source = LocalMemorySource::discover_at(
        root.path().to_path_buf(),
        data.path().to_path_buf(),
        project_ref(),
        RepoPath::new("docs/specs").unwrap(),
        MemoryPolicy::default(),
    )
    .unwrap();
    let descriptor = source
        .manifest
        .sources
        .iter()
        .find(|source| source.category == MemorySourceCategory::SpecificationStructure)
        .unwrap()
        .clone();
    let title_span = parse_spec_memory(initial).structure.title_span.unwrap();

    fs::write(
        root.path().join(path),
        "Private prefix.\n# Example\n\nPrivate body.\n",
    )
    .unwrap();

    let error = source
        .content(MemoryContentRequest {
            project: project_ref(),
            revision_id: source.manifest.revision_id().unwrap(),
            source_category: descriptor.category,
            locator: descriptor.locator,
            expected_fingerprint: descriptor.fingerprint,
            evidence: Some(MemoryEvidenceLocator::Span(title_span)),
            max_bytes: NonZeroU64::new(1024).unwrap(),
        })
        .unwrap_err();
    assert_eq!(error, MemoryQueryError::ContentChanged);
}

#[test]
fn content_read_deadline_rejects_an_expired_budget() {
    let error = ensure_content_read_deadline(Instant::now(), Duration::ZERO).unwrap_err();
    assert_eq!(
        error,
        MemoryQueryError::BudgetExceeded(MemoryTruncationReason::Duration)
    );
}
