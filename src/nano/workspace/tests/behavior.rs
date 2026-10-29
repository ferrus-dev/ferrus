//! Tests for behavior.

use super::*;

#[tokio::test]
async fn malformed_patch_feedback_repairs_real_call_shapes_without_effects() {
    let (dir, mut workspace) = setup();
    disk::write(dir.path().join("source.txt"), "old\n").unwrap();
    disk::write(dir.path().join("temporary.txt"), "temporary\n").unwrap();
    for (arguments, expected) in [
        (
            json!({"edits":"[{\"operation\":\"update\",\"secret\":\"do-not-echo\"}]"}),
            "JSON array",
        ),
        (
            json!({"edits":[{"operation":"delete","path":"temporary.txt"}]}),
            "source.digest",
        ),
        (
            json!({"edits":[{"operation":"update","path":"source.txt","expected_digest":digest(b"old\n"),"hunks":[{"start_line":1,"old_text":"old\n","new_text":"new\n","secret":"do-not-echo"}]}]}),
            "No other fields",
        ),
    ] {
        let call = ValidatedCall {
            call_id: "invalid".into(),
            provider_call_id: "invalid".into(),
            name: "apply_patch".into(),
            arguments,
        };
        let error = workspace.validate(&call.name, &call.arguments).unwrap_err();
        let encoded = serde_json::to_string(&error).unwrap();
        assert!(encoded.contains(expected), "{encoded}");
        assert!(!encoded.contains("do-not-echo"));
        assert!(encoded.len() < 512);
        assert_eq!(
            workspace.execute(&call, &Cancellation::default()).await,
            ToolOutcome::Failed(error)
        );
        assert_eq!(
            disk::read_to_string(dir.path().join("source.txt")).unwrap(),
            "old\n"
        );
        assert!(dir.path().join("temporary.txt").exists());
    }

    let arguments = json!({"edits":[
        {"operation":"delete","path":"temporary.txt","expected_digest":read(&workspace,"temporary.txt").source.digest},
        {"operation":"update","path":"source.txt","expected_digest":read(&workspace,"source.txt").source.digest,"hunks":[{"start_line":1,"old_text":"old\n","new_text":"new\n"}]}
    ]});
    workspace.validate("apply_patch", &arguments).unwrap();
    let call = ValidatedCall {
        call_id: "repaired".into(),
        provider_call_id: "repaired".into(),
        name: "apply_patch".into(),
        arguments,
    };
    assert!(matches!(
        workspace.execute(&call, &Cancellation::default()).await,
        ToolOutcome::Success(_)
    ));
    assert_eq!(
        disk::read_to_string(dir.path().join("source.txt")).unwrap(),
        "new\n"
    );
    assert!(!dir.path().join("temporary.txt").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn unix_patch_target_keys_use_the_opened_parent_identity() {
    let (dir, mut workspace) = setup();
    let first = "caf\u{e9}";
    let second = "cafe\u{301}";
    disk::create_dir(dir.path().join(first)).unwrap();
    let aliases = dir.path().join(second).exists();
    if !aliases {
        disk::create_dir(dir.path().join(second)).unwrap();
    }

    let result = apply(
        &mut workspace,
        [first, second]
            .into_iter()
            .map(|parent| Edit::Create {
                path: format!("{parent}/new"),
                content: parent.into(),
            })
            .collect(),
    )
    .await;
    if aliases {
        assert!(!result.complete && result.changes.is_empty(), "{result:?}");
        assert_eq!(result.failure.unwrap().code, Code::InvalidPatch);
        assert!(!dir.path().join(first).join("new").exists());
    } else {
        assert!(result.complete, "{result:?}");
        assert_eq!(
            disk::read_to_string(dir.path().join(first).join("new")).unwrap(),
            first
        );
        assert_eq!(
            disk::read_to_string(dir.path().join(second).join("new")).unwrap(),
            second
        );
    }
}

#[tokio::test]
async fn ordered_hunks_insert_and_delete_against_original_line_numbers() {
    let (dir, mut workspace) = setup();
    let before = "one\r\ntwo\nthree\r\nlast";
    disk::write(dir.path().join("source"), before).unwrap();
    let result = apply(
        &mut workspace,
        vec![Edit::Update {
            path: "source".into(),
            expected_digest: digest(before.as_bytes()),
            hunks: vec![
                Hunk {
                    start_line: 2,
                    old_text: String::new(),
                    new_text: "insert\n".into(),
                },
                Hunk {
                    start_line: 3,
                    old_text: "three\r\n".into(),
                    new_text: String::new(),
                },
                Hunk {
                    start_line: 4,
                    old_text: "last".into(),
                    new_text: "end".into(),
                },
            ],
        }],
    )
    .await;
    assert!(result.complete, "{result:?}");
    assert_eq!(
        disk::read(dir.path().join("source")).unwrap(),
        b"one\r\ninsert\ntwo\nend"
    );
    assert_eq!(
        path(".g\u{131}t/config").unwrap_err().code,
        Code::ProtectedPath
    );
    assert_eq!(
        path("ferru\u{17f}.db").unwrap_err().code,
        Code::ProtectedPath
    );
    assert!(
        workspace
            .validate("read_file", &json!({"path":"source","start_line":0}))
            .is_err()
    );
}
