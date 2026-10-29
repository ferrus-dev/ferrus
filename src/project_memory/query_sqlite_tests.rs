//! Memory query tests for search, context, cursor validation, and bounded responses.

use std::{cell::Cell, fs, path::Path, process::Command};

use tempfile::TempDir;

use crate::project_memory::{
    domain::{MemoryRecordId, MemoryViewName, ProjectId, ProjectNamespace, ProjectRef},
    index::{MemoryIndexOptions, MemoryIndexer},
    policy::MemoryPolicy,
    ports::MemorySource,
    query::{MemoryFreshnessComparison, MemoryPageRequest, MemoryQueryScope},
    source::LocalMemorySource,
    sqlite::{MEMORY_SIDECAR_FILE_NAME, OpenMemoryQuerySidecarResult, open_for_query_at},
};
use crate::repository_graph::{domain::RepoPath, query::EdgeDirection};

use super::*;

fn project() -> ProjectRef {
    ProjectRef {
        namespace: ProjectNamespace::new("local:test").unwrap(),
        project_id: ProjectId::new("query-project").unwrap(),
    }
}

fn initialize_repository(root: &Path) {
    assert!(
        Command::new("git")
            .arg("init")
            .arg(root)
            .status()
            .unwrap()
            .success()
    );
    fs::create_dir_all(root.join("docs/specs")).unwrap();
    fs::write(
        root.join("docs/specs/query.md"),
        "# Query memory\n\n- [x] #4.4 Federation\n\nID: rg-test\nDepends on: none\n\n## Outcome\n\nDelivered bounded SQLite retrieval.\n\n### Decisions\n\nUse deterministic ranking.\n",
    )
    .unwrap();
    assert!(
        Command::new("git")
            .current_dir(root)
            .args(["add", "--", "docs/specs/query.md"])
            .status()
            .unwrap()
            .success()
    );
}

fn indexed_fixture() -> (TempDir, TempDir, ProjectRef, MemoryRevisionId) {
    let root = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    initialize_repository(root.path());
    let project = project();
    crate::project_memory::source::record_approved_outcome_for_test(
        data.path(),
        "docs/specs/query.md",
        &fs::read_to_string(root.path().join("docs/specs/query.md")).unwrap(),
    );
    let source = LocalMemorySource::discover_at(
        root.path().to_path_buf(),
        data.path().to_path_buf(),
        project.clone(),
        RepoPath::new("docs/specs").unwrap(),
        MemoryPolicy::default(),
    )
    .unwrap();
    let mut sidecar = MemorySidecar::open_at(data.path()).unwrap();
    let outcome = MemoryIndexer::new(&source, &mut sidecar)
        .unwrap()
        .index(MemoryIndexOptions::default())
        .unwrap();
    (root, data, project, outcome.revision.id)
}

fn published_scope(project: ProjectRef, limits: &QueryLimitsConfig) -> MemoryQueryScope {
    MemoryQueryScope::current(
        project,
        super::super::query::MemoryRevisionSelector::Published(
            MemoryViewName::new("project").unwrap(),
        ),
        default_budget(limits).unwrap(),
    )
}

struct RecordingContent {
    max_requested: Cell<u64>,
    max_duration: Cell<Duration>,
}

struct ExpiredContent;

impl MemoryContent for ExpiredContent {
    fn content(
        &self,
        _request: MemoryContentRequest,
    ) -> Result<super::super::query::MemoryContentResponse, MemoryQueryError> {
        unreachable!("deadline-aware context reads use content_with_deadline")
    }

    fn content_with_deadline(
        &self,
        _request: MemoryContentRequest,
        _max_duration: Duration,
    ) -> Result<super::super::query::MemoryContentResponse, MemoryQueryError> {
        Err(MemoryQueryError::BudgetExceeded(
            MemoryTruncationReason::Duration,
        ))
    }
}

impl MemoryContent for RecordingContent {
    fn content(
        &self,
        request: MemoryContentRequest,
    ) -> Result<super::super::query::MemoryContentResponse, MemoryQueryError> {
        Ok(super::super::query::MemoryContentResponse {
            verified_fingerprint: request.expected_fingerprint,
            bytes: vec![b'x'; request.max_bytes.get() as usize],
            truncated: true,
        })
    }

    fn content_with_deadline(
        &self,
        request: MemoryContentRequest,
        max_duration: Duration,
    ) -> Result<super::super::query::MemoryContentResponse, MemoryQueryError> {
        self.max_requested
            .set(self.max_requested.get().max(request.max_bytes.get()));
        self.max_duration.set(max_duration);
        self.content(request)
    }
}

#[path = "query_sqlite_tests/authorization.rs"]
mod authorization_tests;
#[path = "query_sqlite_tests/cancellation.rs"]
mod cancellation_tests;
#[path = "query_sqlite_tests/context.rs"]
mod context_tests;
#[path = "query_sqlite_tests/limits.rs"]
mod limits_tests;
#[path = "query_sqlite_tests/memory.rs"]
mod memory_tests;
#[path = "query_sqlite_tests/pagination.rs"]
mod pagination_tests;
#[path = "query_sqlite_tests/snapshots.rs"]
mod snapshots_tests;

#[test]
fn cursor_rejects_a_different_request_fingerprint() {
    let revision = MemoryRevisionId::new("memory:revision").unwrap();
    let cursor = encode_cursor("search", &revision, "one", 2).unwrap();
    assert_eq!(
        decode_cursor(Some(&cursor), "search", &revision, "two"),
        Err(MemoryQueryError::StaleCursor)
    );
}
