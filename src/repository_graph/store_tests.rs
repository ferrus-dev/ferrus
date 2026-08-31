use super::*;
use crate::repository_graph::sqlite::{OpenSidecarResult, open_for_build_at};

fn repository() -> RepositoryRef {
    RepositoryRef {
        namespace: RepositoryNamespace::new("local:test-project").unwrap(),
        repository_id: RepositoryId::new("root").unwrap(),
    }
}

fn digest(value: &str) -> Digest {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(value.len() * 2);
    for byte in value.bytes() {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Digest::new("sha256", encoded).unwrap()
}

fn build(number: u32) -> GraphBuild {
    GraphBuild {
        id: BuildId::new(format!("build-{number}")).unwrap(),
        repository: repository(),
        source_revision_id: SourceRevisionId::new(format!("revision-{number}")).unwrap(),
        prospective_snapshot_id: SnapshotId::new(format!("snapshot-{number}")).unwrap(),
        state: BuildState::Building,
    }
}

fn snapshot(build: &GraphBuild) -> GraphSnapshot {
    GraphSnapshot {
        id: build.prospective_snapshot_id.clone(),
        repository: build.repository.clone(),
        source_revision_id: build.source_revision_id.clone(),
        source_manifest_digest: digest(&format!("manifest-{}", build.id.as_str())),
        graph_model_version: 1,
        analysis_config_digest: digest("config"),
        extractor_set_digest: digest("extractors"),
        completed_by: build.id.clone(),
    }
}

fn sidecar() -> (tempfile::TempDir, Sidecar) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("repo-graph.db");
    let OpenSidecarResult::Ready(sidecar) = open_for_build_at(&path).unwrap() else {
        panic!("test sidecar unexpectedly requires rebuild");
    };
    (directory, sidecar)
}

fn canonical() -> PublishedViewName {
    PublishedViewName::new("canonical").unwrap()
}

#[test]
fn partial_and_completed_builds_are_invisible_until_publication() {
    let (_directory, mut sidecar) = sidecar();
    let build = build(1);
    sidecar.start_build(&build).unwrap();
    assert_eq!(
        sidecar.build(&build.id).unwrap().unwrap().state,
        BuildState::Building
    );
    assert!(
        sidecar
            .published_snapshot(&repository(), &canonical())
            .unwrap()
            .is_none()
    );

    let completed = sidecar.complete_build(&snapshot(&build)).unwrap();
    assert_eq!(
        sidecar.build(&build.id).unwrap().unwrap().state,
        BuildState::Complete
    );
    assert!(
        sidecar
            .published_snapshot(&repository(), &canonical())
            .unwrap()
            .is_none()
    );

    let outcome = sidecar
        .publish(&PublishRequest {
            repository: repository(),
            view_name: canonical(),
            build_id: build.id.clone(),
            expected: None,
        })
        .unwrap();
    let PublicationOutcome::Published { view } = outcome else {
        panic!("first publication was unexpectedly superseded");
    };
    assert_eq!(view.generation, 1);
    assert_eq!(
        sidecar
            .published_snapshot(&repository(), &canonical())
            .unwrap(),
        Some(completed)
    );
}

#[test]
fn failed_build_does_not_replace_last_published_snapshot() {
    let (_directory, mut sidecar) = sidecar();
    let first = build(1);
    sidecar.start_build(&first).unwrap();
    sidecar.complete_build(&snapshot(&first)).unwrap();
    sidecar
        .publish(&PublishRequest {
            repository: repository(),
            view_name: canonical(),
            build_id: first.id.clone(),
            expected: None,
        })
        .unwrap();

    let failed = build(2);
    sidecar.start_build(&failed).unwrap();
    let result = sidecar
        .fail_build(&BuildFailure {
            build_id: failed.id.clone(),
            code: DiagnosticCode::new("extractor_failed").unwrap(),
        })
        .unwrap();
    assert_eq!(result.state, BuildState::Failed);
    let stored_failure: (String, Option<String>) = sidecar
        .connection()
        .query_row(
            "SELECT failure_code, failure_message FROM index_builds WHERE id = ?1",
            [failed.id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(stored_failure, ("extractor_failed".to_string(), None));
    assert_eq!(
        sidecar
            .published_view(&repository(), &canonical())
            .unwrap()
            .unwrap()
            .build_id,
        first.id
    );
}

#[test]
fn build_failure_wire_contract_rejects_free_form_details() {
    let with_message = serde_json::json!({
        "build_id": "build-1",
        "code": "extractor_failed",
        "message": "parser failed near source text /absolute/path token=secret"
    });
    assert!(serde_json::from_value::<BuildFailure>(with_message).is_err());

    let invalid_code = serde_json::json!({
        "build_id": "build-1",
        "code": "token=secret"
    });
    assert!(serde_json::from_value::<BuildFailure>(invalid_code).is_err());
}

#[test]
fn older_build_cannot_overwrite_a_newer_publication() {
    let (_directory, mut sidecar) = sidecar();
    let older = build(1);
    let newer = build(2);
    sidecar.start_build(&older).unwrap();
    sidecar.start_build(&newer).unwrap();
    sidecar.complete_build(&snapshot(&newer)).unwrap();
    sidecar
        .publish(&PublishRequest {
            repository: repository(),
            view_name: canonical(),
            build_id: newer.id.clone(),
            expected: None,
        })
        .unwrap();
    sidecar.complete_build(&snapshot(&older)).unwrap();
    let current = sidecar
        .published_view(&repository(), &canonical())
        .unwrap()
        .unwrap();

    let outcome = sidecar
        .publish(&PublishRequest {
            repository: repository(),
            view_name: canonical(),
            build_id: older.id.clone(),
            expected: Some(PublicationVersion {
                snapshot_id: current.snapshot_id.clone(),
                generation: current.generation,
            }),
        })
        .unwrap();
    assert_eq!(
        outcome,
        PublicationOutcome::Superseded {
            current: current.clone()
        }
    );
    assert_eq!(
        sidecar.build(&older.id).unwrap().unwrap().state,
        BuildState::Superseded
    );
    assert_eq!(
        sidecar.published_view(&repository(), &canonical()).unwrap(),
        Some(current)
    );
}

#[test]
fn displaced_published_build_is_persisted_as_superseded_on_retry() {
    let (_directory, mut sidecar) = sidecar();
    let older = build(1);
    sidecar.start_build(&older).unwrap();
    sidecar.complete_build(&snapshot(&older)).unwrap();
    let PublicationOutcome::Published { view: older_view } = sidecar
        .publish(&PublishRequest {
            repository: repository(),
            view_name: canonical(),
            build_id: older.id.clone(),
            expected: None,
        })
        .unwrap()
    else {
        panic!("initial publication was unexpectedly superseded");
    };

    let newer = build(2);
    sidecar.start_build(&newer).unwrap();
    sidecar.complete_build(&snapshot(&newer)).unwrap();
    let PublicationOutcome::Published { view: newer_view } = sidecar
        .publish(&PublishRequest {
            repository: repository(),
            view_name: canonical(),
            build_id: newer.id.clone(),
            expected: Some(PublicationVersion {
                snapshot_id: older_view.snapshot_id,
                generation: older_view.generation,
            }),
        })
        .unwrap()
    else {
        panic!("newer publication was unexpectedly superseded");
    };
    assert_eq!(
        sidecar.build(&older.id).unwrap().unwrap().state,
        BuildState::Published
    );

    let outcome = sidecar
        .publish(&PublishRequest {
            repository: repository(),
            view_name: canonical(),
            build_id: older.id.clone(),
            expected: Some(PublicationVersion {
                snapshot_id: newer_view.snapshot_id.clone(),
                generation: newer_view.generation,
            }),
        })
        .unwrap();

    assert_eq!(
        outcome,
        PublicationOutcome::Superseded {
            current: newer_view.clone()
        }
    );
    assert_eq!(
        sidecar.build(&older.id).unwrap().unwrap().state,
        BuildState::Superseded
    );
    assert_eq!(
        sidecar.published_view(&repository(), &canonical()).unwrap(),
        Some(newer_view)
    );
}

#[test]
fn stale_compare_and_set_leaves_pointer_and_candidate_unchanged() {
    let (_directory, mut sidecar) = sidecar();
    let first = build(1);
    sidecar.start_build(&first).unwrap();
    sidecar.complete_build(&snapshot(&first)).unwrap();
    sidecar
        .publish(&PublishRequest {
            repository: repository(),
            view_name: canonical(),
            build_id: first.id.clone(),
            expected: None,
        })
        .unwrap();
    let second = build(2);
    sidecar.start_build(&second).unwrap();
    sidecar.complete_build(&snapshot(&second)).unwrap();

    let error = sidecar
        .publish(&PublishRequest {
            repository: repository(),
            view_name: canonical(),
            build_id: second.id.clone(),
            expected: None,
        })
        .unwrap_err();
    assert!(matches!(error, StoreError::PublicationConflict { .. }));
    assert_eq!(
        sidecar.build(&second.id).unwrap().unwrap().state,
        BuildState::Complete
    );
    assert_eq!(
        sidecar
            .published_view(&repository(), &canonical())
            .unwrap()
            .unwrap()
            .build_id,
        first.id
    );
}

#[test]
fn no_op_publish_still_enforces_compare_and_set_expectation() {
    let (_directory, mut sidecar) = sidecar();
    let build = build(1);
    sidecar.start_build(&build).unwrap();
    sidecar.complete_build(&snapshot(&build)).unwrap();
    let published = match sidecar
        .publish(&PublishRequest {
            repository: repository(),
            view_name: canonical(),
            build_id: build.id.clone(),
            expected: None,
        })
        .unwrap()
    {
        PublicationOutcome::Published { view } => view,
        PublicationOutcome::Superseded { .. } => unreachable!(),
    };
    let actual = PublicationVersion {
        snapshot_id: published.snapshot_id.clone(),
        generation: published.generation,
    };

    let missing_expectation = sidecar
        .publish(&PublishRequest {
            repository: repository(),
            view_name: canonical(),
            build_id: build.id.clone(),
            expected: None,
        })
        .unwrap_err();
    assert!(matches!(
        missing_expectation,
        StoreError::PublicationConflict {
            expected: None,
            actual: Some(ref version),
        } if version == &actual
    ));

    let stale_expectation = sidecar
        .publish(&PublishRequest {
            repository: repository(),
            view_name: canonical(),
            build_id: build.id.clone(),
            expected: Some(PublicationVersion {
                snapshot_id: actual.snapshot_id.clone(),
                generation: actual.generation - 1,
            }),
        })
        .unwrap_err();
    assert!(matches!(
        stale_expectation,
        StoreError::PublicationConflict {
            actual: Some(ref version),
            ..
        } if version == &actual
    ));

    let no_op = sidecar
        .publish(&PublishRequest {
            repository: repository(),
            view_name: canonical(),
            build_id: build.id,
            expected: Some(actual),
        })
        .unwrap();
    assert_eq!(no_op, PublicationOutcome::Published { view: published });
}

#[test]
fn completed_snapshot_identity_is_immutable_across_build_attempts() {
    let (_directory, mut sidecar) = sidecar();
    let first = build(1);
    sidecar.start_build(&first).unwrap();
    let original = sidecar.complete_build(&snapshot(&first)).unwrap();

    let retry = GraphBuild {
        id: BuildId::new("build-retry").unwrap(),
        repository: first.repository.clone(),
        source_revision_id: first.source_revision_id.clone(),
        prospective_snapshot_id: first.prospective_snapshot_id.clone(),
        state: BuildState::Building,
    };
    sidecar.start_build(&retry).unwrap();
    let mut conflicting = original.clone();
    conflicting.completed_by = retry.id.clone();
    conflicting.extractor_set_digest = digest("different-extractors");

    assert!(matches!(
        sidecar.complete_build(&conflicting),
        Err(StoreError::IdentityMismatch("existing snapshot contents"))
    ));
    assert_eq!(
        sidecar.snapshot(&original.id).unwrap(),
        Some(original.clone())
    );
    assert_eq!(
        sidecar.build(&retry.id).unwrap().unwrap().state,
        BuildState::Building
    );
}

#[test]
fn identical_snapshot_is_reused_across_source_revisions() {
    let (_directory, mut sidecar) = sidecar();
    let first = build(1);
    sidecar.start_build(&first).unwrap();
    let original = sidecar.complete_build(&snapshot(&first)).unwrap();
    let PublicationOutcome::Published {
        view: published_view,
    } = sidecar
        .publish(&PublishRequest {
            repository: repository(),
            view_name: canonical(),
            build_id: first.id.clone(),
            expected: None,
        })
        .unwrap()
    else {
        panic!("initial publication was unexpectedly superseded");
    };

    let retry = GraphBuild {
        id: BuildId::new("build-retry").unwrap(),
        repository: first.repository.clone(),
        source_revision_id: SourceRevisionId::new("revision-with-identical-tree").unwrap(),
        prospective_snapshot_id: first.prospective_snapshot_id.clone(),
        state: BuildState::Building,
    };
    sidecar.start_build(&retry).unwrap();
    let equivalent = GraphSnapshot {
        source_revision_id: retry.source_revision_id.clone(),
        completed_by: retry.id.clone(),
        ..original.clone()
    };

    assert_eq!(sidecar.complete_build(&equivalent).unwrap(), original);
    assert_eq!(
        sidecar.build(&retry.id).unwrap().unwrap().state,
        BuildState::Complete
    );

    let PublicationOutcome::Published { view } = sidecar
        .publish(&PublishRequest {
            repository: repository(),
            view_name: canonical(),
            build_id: retry.id.clone(),
            expected: Some(PublicationVersion {
                snapshot_id: published_view.snapshot_id.clone(),
                generation: published_view.generation,
            }),
        })
        .unwrap()
    else {
        panic!("equivalent snapshot build was unexpectedly superseded");
    };
    assert_eq!(view, published_view);
    assert_eq!(
        sidecar.published_view(&repository(), &canonical()).unwrap(),
        Some(published_view)
    );
    assert_eq!(
        sidecar.build(&retry.id).unwrap().unwrap().state,
        BuildState::Complete
    );
}

#[test]
fn completed_candidate_can_be_explicitly_superseded() {
    let (_directory, mut sidecar) = sidecar();
    let candidate = build(1);
    sidecar.start_build(&candidate).unwrap();
    sidecar.complete_build(&snapshot(&candidate)).unwrap();

    let superseded = sidecar.supersede_build(&candidate.id).unwrap();
    assert_eq!(superseded.state, BuildState::Superseded);
    assert!(matches!(
        sidecar.publish(&PublishRequest {
            repository: repository(),
            view_name: canonical(),
            build_id: candidate.id,
            expected: None,
        }),
        Err(StoreError::InvalidTransition {
            state: BuildState::Superseded,
            operation: "publish"
        })
    ));
}

#[test]
fn publication_database_failure_rolls_back_pointer_and_build_state() {
    let (_directory, mut sidecar) = sidecar();
    let first = build(1);
    sidecar.start_build(&first).unwrap();
    sidecar.complete_build(&snapshot(&first)).unwrap();
    let first_view = match sidecar
        .publish(&PublishRequest {
            repository: repository(),
            view_name: canonical(),
            build_id: first.id.clone(),
            expected: None,
        })
        .unwrap()
    {
        PublicationOutcome::Published { view } => view,
        PublicationOutcome::Superseded { .. } => unreachable!(),
    };
    let second = build(2);
    sidecar.start_build(&second).unwrap();
    sidecar.complete_build(&snapshot(&second)).unwrap();
    sidecar
        .connection()
        .execute_batch(
            "CREATE TRIGGER reject_test_publication BEFORE UPDATE ON published_views \
             BEGIN SELECT RAISE(ABORT, 'simulated publication failure'); END;",
        )
        .unwrap();

    let result = sidecar.publish(&PublishRequest {
        repository: repository(),
        view_name: canonical(),
        build_id: second.id.clone(),
        expected: Some(PublicationVersion {
            snapshot_id: first_view.snapshot_id.clone(),
            generation: first_view.generation,
        }),
    });
    assert!(matches!(result, Err(StoreError::Database(_))));
    assert_eq!(
        sidecar.published_view(&repository(), &canonical()).unwrap(),
        Some(first_view)
    );
    assert_eq!(
        sidecar.build(&second.id).unwrap().unwrap().state,
        BuildState::Complete
    );
}
