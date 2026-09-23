use super::*;
use crate::nano::{
    provider::{FinishReason, ModelResponse},
    tools::ToolCall,
    workspace::{Limits, ReadRequest},
};

fn read(workspace: &Workspace, path: &str, start: usize, lines: usize) -> Value {
    serde_json::to_value(
        workspace
            .read_file(ReadRequest {
                path: path.into(),
                start_line: start,
                max_lines: lines,
                max_bytes: 4096,
            })
            .unwrap(),
    )
    .unwrap()
}

fn append(messages: &mut Vec<Message>, name: &str, args: Value, value: Value) {
    let id = format!("tool-{}", messages.len());
    messages.push(Message::Assistant {
        response: ModelResponse {
            finish: FinishReason::ToolCalls,
            text: String::new(),
            calls: vec![ToolCall {
                provider_call_id: id.clone(),
                name: name.into(),
                arguments: args.to_string(),
            }],
            continuation: None,
        },
    });
    messages.push(Message::Tool {
        provider_call_id: id,
        outcome: ToolOutcome::Success(value),
    });
}

fn fixture() -> (tempfile::TempDir, Workspace) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "one\ntwo\nthree\nfour\n").unwrap();
    let workspace = Workspace::new(dir.path(), Limits::default()).unwrap();
    (dir, workspace)
}

#[test]
fn duplicate_selection_is_deterministic_and_preserves_journal_and_constraints() {
    let (_dir, workspace) = fixture();
    let mut messages = vec![Message::User {
        text: "required constraints".into(),
    }];
    for _ in 0..2 {
        append(
            &mut messages,
            "read_file",
            json!({"path":"a.rs"}),
            read(&workspace, "a.rs", 1, 4),
        );
    }
    let before = messages.clone();
    let a = prepare(
        &messages,
        &json!({"task":"a"}),
        &json!({}),
        &workspace,
        false,
    )
    .unwrap();
    assert_eq!(
        a,
        prepare(
            &messages,
            &json!({"task":"a"}),
            &json!({}),
            &workspace,
            false
        )
        .unwrap()
    );
    assert_eq!(a.replacements.len(), 1);
    assert_eq!(a.replacements[0].value["kind"], "evidence_reference");
    assert_eq!(a.replacements[0].value["message"], 4);
    assert_eq!(messages, before);
    assert_eq!(a.apply(&messages).unwrap()[0], before[0]);
    let mut corrupt = a;
    corrupt.replacements[0].message = 0;
    assert!(corrupt.apply(&messages).is_err());
}

#[test]
fn overlapping_lines_merge_only_matching_content_and_origin() {
    let (dir, workspace) = fixture();
    let mut messages = Vec::new();
    append(
        &mut messages,
        "read_file",
        json!({}),
        read(&workspace, "a.rs", 1, 3),
    );
    append(
        &mut messages,
        "read_file",
        json!({}),
        read(&workspace, "a.rs", 2, 3),
    );
    let projection = prepare(&messages, &json!({}), &json!({}), &workspace, false).unwrap();
    let merged = projection
        .replacements
        .iter()
        .find(|r| r.message == 3)
        .unwrap();
    assert_eq!(merged.value["text"], "one\ntwo\nthree\nfour\n");
    assert_eq!(merged.value["start_line"], 1);
    assert_eq!(merged.value["returned_lines"], 4);
    std::fs::write(dir.path().join("a.rs"), "changed\n").unwrap();
    append(
        &mut messages,
        "read_file",
        json!({}),
        read(&workspace, "a.rs", 1, 3),
    );
    let projection = prepare(&messages, &json!({}), &json!({}), &workspace, false).unwrap();
    assert!(
        projection
            .replacements
            .iter()
            .all(|r| r.value["kind"] == "evidence_unavailable")
    );
    let read = read(&workspace, "a.rs", 1, 3);
    let fallback =
        json!({"kind":"workspace_fallback", "requested_reason":"unsupported", "evidence":read});
    let direct = evidence("read_file", &read, &json!({})).unwrap();
    let fallback = evidence("repository_fallback", &fallback, &json!({})).unwrap();
    assert_ne!(direct.provenance, fallback.provenance);
}

#[test]
fn external_add_edit_delete_rename_never_uses_generation_as_freshness() {
    for operation in ["edit", "delete", "rename"] {
        let (dir, workspace) = fixture();
        let mut messages = Vec::new();
        append(
            &mut messages,
            "read_file",
            json!({}),
            read(&workspace, "a.rs", 1, 4),
        );
        match operation {
            "edit" => std::fs::write(dir.path().join("a.rs"), "external\n").unwrap(),
            "delete" => std::fs::remove_file(dir.path().join("a.rs")).unwrap(),
            _ => std::fs::rename(dir.path().join("a.rs"), dir.path().join("b.rs")).unwrap(),
        }
        std::fs::write(dir.path().join("new.rs"), "new source\n").unwrap();
        append(
            &mut messages,
            "read_file",
            json!({}),
            read(&workspace, "new.rs", 1, 4),
        );
        let result = prepare(&messages, &json!({}), &json!({}), &workspace, false).unwrap();
        assert_eq!(result.replacements.len(), 1, "{operation}");
        assert_eq!(
            result.replacements[0].value["reason"],
            "source_changed_or_unverified"
        );
        assert_eq!(result.observations[0]["freshness"], "unknown");
    }
}

#[test]
fn patches_commands_checks_and_background_writers_invalidate_evidence() {
    let (_dir, workspace) = fixture();
    for name in ["apply_patch", "exec", "check"] {
        let mut messages = Vec::new();
        append(
            &mut messages,
            "read_file",
            json!({}),
            read(&workspace, "a.rs", 1, 4),
        );
        append(
            &mut messages,
            name,
            json!({"edits":[{"path":"a.rs"}]}),
            json!({}),
        );
        let result = prepare(&messages, &json!({}), &json!({}), &workspace, false).unwrap();
        assert_eq!(result.replacements[0].value["kind"], "evidence_unavailable");
    }
    let mut messages = Vec::new();
    append(
        &mut messages,
        "read_file",
        json!({}),
        read(&workspace, "a.rs", 1, 4),
    );
    let result = prepare(&messages, &json!({}), &json!({}), &workspace, true).unwrap();
    assert_eq!(result.replacements[0].value["reason"], "active_writer");
}

#[test]
fn changed_publications_and_memory_revisions_cannot_reuse_packets() {
    let (_dir, workspace) = fixture();
    let packet = json!({"kind":"project_context", "result":{"Ok":{
        "repository":{"snapshot_id":"snapshot-1", "task_view":{"baseline_snapshot_id":"baseline", "overlay_revision_id":"overlay-1"}},
        "memory":{"revision_id":"memory-1"}, "cross_domain_links":[{"id":"pair-specific"}], "items":[] }}});
    let mut messages = Vec::new();
    append(&mut messages, "project_context", json!({}), packet);
    for revision in [
        json!({"snapshot_id":"snapshot-2","memory_revision_id":"memory-1"}),
        json!({"snapshot_id":"snapshot-1","memory_revision_id":"memory-2"}),
    ] {
        let result = prepare(&messages, &json!({}), &revision, &workspace, false).unwrap();
        assert_eq!(result.replacements.len(), 1);
        assert_eq!(result.replacements[0].value["kind"], "evidence_unavailable");
    }
}

#[test]
fn cache_keys_and_fifo_eviction_are_bounded_and_deterministic() {
    let mut cache = QueryCache::default();
    let base = json!({"query":"name", "cursor":"cursor-1", "snapshot":"s1", "task":"t1", "policy":"p1", "max_bytes":1024});
    let key = identity(&base);
    cache.insert(key.clone(), json!({"packet":1}));
    for field in ["query", "cursor", "snapshot", "task", "policy", "max_bytes"] {
        let mut changed = base.clone();
        changed[field] = json!("different");
        assert!(cache.get(&identity(&changed)).is_none());
    }
    for i in 0..ENTRIES {
        cache.insert(i.to_string(), json!({"packet":i}));
    }
    assert!(cache.get(&key).is_none());
    assert_eq!(cache.entries.len(), ENTRIES);
    cache.insert("oversized".into(), json!("x".repeat(33 * 1024)));
    assert!(cache.get("oversized").is_none());
    cache.clear();
    assert!(cache.entries.is_empty());
}
