//! Native file-tool behavior, confinement, bounds, and exact patch regressions.

use super::patch::{Edit, Hunk, State};
use super::*;
use std::fs as disk;
use tempfile::TempDir;

fn setup() -> (TempDir, Workspace) {
    let dir = TempDir::new().unwrap();
    let workspace = Workspace::new(&dir.path().canonicalize().unwrap(), Limits::default()).unwrap();
    (dir, workspace)
}

fn read(workspace: &Workspace, path: &str) -> ReadResult {
    workspace
        .read_file(serde_json::from_value(json!({"path":path})).unwrap())
        .unwrap()
}

fn update(path: &str, before: &str, start_line: usize, old: &str, new: &str) -> Edit {
    Edit::Update {
        path: path.into(),
        expected_digest: digest(before.as_bytes()),
        hunks: vec![Hunk {
            start_line,
            old_text: old.into(),
            new_text: new.into(),
        }],
    }
}

async fn apply(workspace: &mut Workspace, edits: Vec<Edit>) -> PatchResult {
    workspace
        .apply_patch(PatchRequest { edits }, &Cancellation::default())
        .await
}

use super::patch::PatchResult;

#[path = "tests/behavior.rs"]
mod behavior_tests;
#[path = "tests/context.rs"]
mod context_tests;
#[path = "tests/files.rs"]
mod files_tests;
#[path = "tests/limits.rs"]
mod limits_tests;

#[tokio::test]
async fn partial_tool_outcomes_retain_reconciliation_details() {
    use std::{future::Future, task::Poll};
    let (dir, mut workspace) = setup();
    disk::create_dir(dir.path().join("parent")).unwrap();
    let call = ValidatedCall {
        call_id: "c1".into(),
        provider_call_id: "p1".into(),
        name: "apply_patch".into(),
        arguments: json!({"edits":[
            {"operation":"create","path":"first","content":"one"},
            {"operation":"create","path":"parent/second","content":"two"}
        ]}),
    };
    let cancel = Cancellation::default();
    let mut operation = std::pin::pin!(workspace.execute(&call, &cancel));
    // The first publication yields before attempting the second one.
    std::future::poll_fn(|cx| {
        assert!(operation.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    disk::remove_dir(dir.path().join("parent")).unwrap();
    let outcome = operation.await;
    assert!(encode(&outcome, 24 * 1024).is_ok());
    let ToolOutcome::Unknown(ToolError::Workspace(result)) = outcome else {
        panic!("missing partial outcome")
    };
    assert_eq!(result["changes"][0]["state"], "applied");
    assert_eq!(result["changes"][0]["after_digest"], digest(b"one"));
    assert_eq!(result["changes"][1]["state"], "not_applied");
    assert_eq!(result["failure"]["code"], "not_found");
}

#[tokio::test]
async fn cancellation_and_schema_validation_precede_effects() {
    let (dir, mut workspace) = setup();
    let cancel = Cancellation::default();
    cancel.cancel();
    let result = workspace
        .apply_patch(
            PatchRequest {
                edits: vec![Edit::Create {
                    path: "x".into(),
                    content: "hello".into(),
                }],
            },
            &cancel,
        )
        .await;
    assert_eq!(result.failure.unwrap().code, Code::Interrupted);
    assert!(!dir.path().join("x").exists());
    assert_eq!(workspace.descriptors().len(), 3);
    assert!(workspace.validate("apply_patch", &json!({"edits":[{"operation":"create","path":"x","content":"hello","unexpected":true}]})).is_err());
    assert!(
        workspace
            .validate("read_file", &json!({"path":"x","start_line":-1}))
            .is_err()
    );
    assert_eq!(
        workspace.validate("exec", &json!({})),
        Err(ToolError::UnknownTool)
    );
    let call = ValidatedCall {
        call_id: "c1".into(),
        provider_call_id: "p1".into(),
        name: "apply_patch".into(),
        arguments: json!({"edits":[{"operation":"create","path":".git/index","content":"bad"}]}),
    };
    assert!(matches!(
        workspace.execute(&call, &Cancellation::default()).await,
        ToolOutcome::Failed(ToolError::Workspace(_))
    ));
}

#[cfg(feature = "nano-mcp")]
#[test]
fn patch_catalog_exposes_nested_types_without_weakening_operation_validation() {
    let (_dir, workspace) = setup();
    let descriptor = workspace
        .descriptors()
        .into_iter()
        .find(|tool| tool.name == "apply_patch")
        .unwrap();
    let schema = &descriptor.input_schema;
    let items = &schema["properties"]["edits"]["items"];
    assert_eq!(items["type"], "object");
    assert_eq!(items["properties"]["hunks"]["items"]["type"], "object");
    let validator = jsonschema::validator_for(schema).unwrap();
    for edit in [
        json!({"operation":"create","path":"x","content":"new\n"}),
        json!({"operation":"delete","path":"x","expected_digest":"a".repeat(64)}),
        json!({"operation":"update","path":"x","expected_digest":"a".repeat(64),"hunks":[{"start_line":1,"old_text":"old\n","new_text":"new\n"}]}),
    ] {
        let value = json!({"edits":[edit]});
        assert!(validator.is_valid(&value));
        workspace.validate("apply_patch", &value).unwrap();
    }

    for edit in [
        json!({"operation":"create","path":"x","content":"new\n","expected_digest":"a".repeat(64)}),
        json!({"operation":"delete","path":"x"}),
        json!({"operation":"update","path":"x","expected_digest":"a".repeat(64),"hunks":"[]"}),
    ] {
        let value = json!({"edits":[edit]});
        assert!(!validator.is_valid(&value));
        assert!(workspace.validate("apply_patch", &value).is_err());
    }

    assert!(!validator.is_valid(&json!({"edits":"[]"})));
    for flat in [
        json!({"operation":"create","path":"x","content":"new\n"}),
        json!({"operation":"delete","path":"x","expected_digest":"a".repeat(64)}),
        json!({"operation":"update","path":"x","expected_digest":"a".repeat(64),"old_text":"old\n","new_text":"new\n"}),
    ] {
        assert!(validator.is_valid(&flat));
        workspace.validate("apply_patch", &flat).unwrap();
    }

    assert!(!validator.is_valid(&json!({"operation":"delete","path":"x"})));
    assert!(
        !validator.is_valid(&json!({"operation":"create","path":"x","content":"new\n","edits":[]}))
    );
}

#[cfg(unix)]
#[tokio::test]
async fn unix_patch_preflight_rejects_canonically_equivalent_new_targets() {
    for (first, second) in [
        ("caf\u{e9}", "cafe\u{301}"),
        ("CAF\u{c9}", "cafe\u{301}"),
        ("a\u{301}\u{327}", "a\u{327}\u{301}"),
    ] {
        let (dir, mut workspace) = setup();
        let result = apply(
            &mut workspace,
            ["unrelated", first, second]
                .into_iter()
                .map(|path| Edit::Create {
                    path: path.into(),
                    content: "new\n".into(),
                })
                .collect(),
        )
        .await;
        assert!(!result.complete && result.changes.is_empty(), "{result:?}");
        assert_eq!(result.failure.unwrap().code, Code::InvalidPatch);
        assert_eq!(disk::read_dir(dir.path()).unwrap().count(), 0);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn unix_patch_preflight_rejects_full_casefold_aliases() {
    for (first, second) in [
        ("source", "\u{17f}ource"),
        ("strasse", "stra\u{df}e"),
        ("\u{3c3}", "\u{3c2}"),
        ("ffi", "\u{fb03}"),
        ("\u{1fc3}", "\u{3b7}\u{3b9}"),
    ] {
        let (dir, mut workspace) = setup();
        let result = apply(
            &mut workspace,
            ["unrelated", first, second]
                .into_iter()
                .map(|path| Edit::Create {
                    path: path.into(),
                    content: "new\n".into(),
                })
                .collect(),
        )
        .await;
        assert!(!result.complete && result.changes.is_empty(), "{result:?}");
        assert_eq!(result.failure.unwrap().code, Code::InvalidPatch);
        assert_eq!(disk::read_dir(dir.path()).unwrap().count(), 0);
    }
}

#[tokio::test]
async fn flat_edits_keep_digest_preflight_and_durable_effect_plans() {
    let (dir, mut workspace) = setup();
    let call = |arguments| ValidatedCall {
        call_id: "single".into(),
        provider_call_id: "single".into(),
        name: "apply_patch".into(),
        arguments,
    };
    let create = call(json!({"operation":"create","path":"source.txt","content":"old\n"}));
    workspace.validate(&create.name, &create.arguments).unwrap();
    assert!(
        matches!(workspace.effect_plan(&create),Some(EffectPlan::Patch {files}) if files.len()==1 && files[0].before_digest.is_none() && files[0].after_digest == Some(digest(b"old\n")))
    );
    assert!(matches!(
        workspace.execute(&create, &Cancellation::default()).await,
        ToolOutcome::Success(_)
    ));
    let update = call(
        json!({"operation":"update","path":"source.txt","expected_digest":read(&workspace,"source.txt").source.digest,"old_text":"old\n","new_text":"new\n"}),
    );
    workspace.validate(&update.name, &update.arguments).unwrap();
    assert!(
        matches!(workspace.effect_plan(&update),Some(EffectPlan::Patch {files}) if files.len()==1 && files[0].before_digest == Some(digest(b"old\n")) && files[0].after_digest == Some(digest(b"new\n")))
    );
    assert!(matches!(
        workspace.execute(&update, &Cancellation::default()).await,
        ToolOutcome::Success(_)
    ));
    assert!(
        matches!(workspace.execute(&update,&Cancellation::default()).await, ToolOutcome::Failed(ToolError::Workspace(value)) if value["failure"]["code"]=="conflict")
    );
    assert_eq!(
        disk::read_to_string(dir.path().join("source.txt")).unwrap(),
        "new\n"
    );
    let delete = call(
        json!({"operation":"delete","path":"source.txt","expected_digest":read(&workspace,"source.txt").source.digest}),
    );
    assert!(
        matches!(workspace.effect_plan(&delete),Some(EffectPlan::Patch {files}) if files.len()==1 && files[0].before_digest == Some(digest(b"new\n")) && files[0].after_digest.is_none())
    );
    assert!(matches!(
        workspace.execute(&delete, &Cancellation::default()).await,
        ToolOutcome::Success(_)
    ));
    assert!(!dir.path().join("source.txt").exists());
    for invalid in [
        json!({"operation":"create","path":"x","content":"new\n","edits":[{"operation":"create","path":"y","content":"new\n"}]}),
        json!({"operation":"update","path":"x","start_line":1,"old_text":"old\n","new_text":"new\n"}),
        json!({"operation":"update","path":"x","expected_digest":"a".repeat(64),"start_line":0,"old_text":"old\n","new_text":"new\n"}),
    ] {
        assert!(workspace.validate("apply_patch", &invalid).is_err());
    }
}

#[cfg(windows)]
#[tokio::test]
async fn patches_follow_nt_case_identity_before_any_publication() {
    for (first, second, required_alias) in [
        ("source", "SOURCE", true),
        ("caf\u{e9}", "CAF\u{c9}", true),
        ("source", "\u{17f}ource", false),
    ] {
        // Unicode lowercase/uppercase rules do not determine NT path equivalence.
        let aliases = fs::search_key(first) == fs::search_key(second);
        if required_alias {
            assert!(aliases, "expected case alias: {first} / {second}");
        }

        for create in [false, true] {
            let (dir, mut workspace) = setup();
            let edits = if create {
                vec![
                    Edit::Create {
                        path: first.into(),
                        content: "new\n".into(),
                    },
                    Edit::Create {
                        path: second.into(),
                        content: "other\n".into(),
                    },
                ]
            } else {
                disk::write(dir.path().join(first), "old\n").unwrap();
                if !aliases {
                    disk::write(dir.path().join(second), "old\n").unwrap();
                }

                vec![
                    update(first, "old\n", 1, "old\n", "new\n"),
                    update(second, "old\n", 1, "old\n", "other\n"),
                ]
            };
            let result = apply(&mut workspace, edits).await;
            if aliases {
                assert!(!result.complete && result.changes.is_empty(), "{result:?}");
                assert_eq!(result.failure.unwrap().code, Code::InvalidPatch);
                if create {
                    assert!(!dir.path().join(first).exists());
                    assert!(!dir.path().join(second).exists());
                } else {
                    assert_eq!(disk::read(dir.path().join(first)).unwrap(), b"old\n");
                    assert_eq!(disk::read(dir.path().join(second)).unwrap(), b"old\n");
                }
            } else {
                // Distinct NT names must remain independently writable, even if
                // Rust's Unicode uppercase would merge them (for example long-s).
                assert!(result.complete, "{result:?}");
                assert_eq!(result.changes.len(), 2);
                assert_eq!(disk::read(dir.path().join(first)).unwrap(), b"new\n");
                assert_eq!(disk::read(dir.path().join(second)).unwrap(), b"other\n");
            }
        }
    }
}
