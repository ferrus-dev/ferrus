//! Tests for compaction.

use super::*;

#[tokio::test]
async fn working_set_refresh_publishes_edits_without_lifecycle_checks() {
    use crate::nano::{
        context::{Context, Request, Response},
        refresh::Refresh,
    };
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = git_session(&f, true).await;
    let before = session.status().await.unwrap();
    std::fs::write(f.root.join("src/lib.rs"), "pub struct WorkingSetEdit;\n").unwrap();
    let mut refresh = Refresh::default();
    refresh.invalidate();
    assert_eq!(
        refresh.prepare(&session.clone().into(), false).await[0]["status"],
        "scheduled"
    );
    assert_eq!(refresh.settle().await.unwrap()["status"], "published");
    let after = session.status().await.unwrap();
    assert_eq!(after.status, before.status);
    assert_eq!(after.check_retries, before.check_retries);
    assert_eq!(
        after.repository_view.baseline_snapshot_id,
        before.repository_view.baseline_snapshot_id
    );
    assert_ne!(
        after.repository_view.view_snapshot_id,
        before.repository_view.view_snapshot_id
    );
    let context = Context::new(session);
    let Response::RepositorySearch(Ok(result)) = context
        .retrieve(
            "repository_search",
            Request::parse("repository_search", json!({"query":"WorkingSetEdit"})).unwrap(),
        )
        .await
        .unwrap()
    else {
        panic!("search")
    };
    assert!(!result.data.hits.is_empty());
    assert_eq!(
        Some(result.snapshot_id),
        after.repository_view.view_snapshot_id
    );
}

#[tokio::test]
async fn working_set_settles_refresh_before_reusing_graph_evidence_without_prefetch() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = git_session(&f, true).await;
    let previous = session
        .status()
        .await
        .unwrap()
        .repository_view
        .view_snapshot_id;
    let (mut tools, _journal) = native(&f, session.clone(), "working-set-after-patch");
    let query = call("repository_search", json!({"query":"Baseline"}));
    let result = tools.execute(&query, &Cancellation::default()).await;
    assert!(
        matches!(&result, ToolOutcome::Success(value) if serde_json::to_string(value).unwrap().contains("Baseline"))
    );
    let messages = vec![
        Message::Assistant {
            response: ModelResponse {
                finish: FinishReason::ToolCalls,
                text: String::new(),
                calls: vec![ToolCall {
                    provider_call_id: query.provider_call_id.clone(),
                    name: query.name.clone(),
                    arguments: query.arguments.to_string(),
                }],
                continuation: None,
            },
        },
        Message::Tool {
            provider_call_id: query.provider_call_id,
            outcome: result,
        },
    ];
    let patch = baseline_patch(&tools, "CurrentSymbol");
    assert!(
        matches!(tools.execute(&patch, &Cancellation::default()).await, ToolOutcome::Success(value) if value["complete"] == true)
    );
    let prepared = tools
        .prepare_context(&messages, &Cancellation::default())
        .await
        .unwrap()
        .unwrap();
    assert!(prepared.replacements.iter().any(|replacement| {
        replacement.message == 1 && replacement.value["kind"] == "evidence_unavailable"
    }));
    assert!(
        !serde_json::to_string(&prepared.apply(&messages).unwrap())
            .unwrap()
            .contains("pub struct Baseline;")
    );
    assert_ne!(
        session
            .status()
            .await
            .unwrap()
            .repository_view
            .view_snapshot_id,
        previous
    );
    assert!(tools.shutdown().await);
}

#[tokio::test]
async fn disabled_working_set_keeps_workspace_tools_but_never_schedules_refresh() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = git_session(&f, true).await;
    let previous = session.status().await.unwrap().repository_view;
    let (mut tools, _journal) = native(&f, session.clone(), "disabled-working-set");
    tools.working_set_enabled = false;
    tools.context.cache_enabled = false;
    let patch = baseline_patch(&tools, "DisabledModeEdit");
    assert!(
        matches!(tools.execute(&patch, &Cancellation::default()).await,
        ToolOutcome::Success(value) if value["complete"] == true)
    );
    let messages = vec![Message::User {
        text: "task".into(),
    }];
    assert!(
        tools
            .prepare_context(&messages, &Cancellation::default())
            .await
            .unwrap()
            .is_none()
    );
    tools.prefetch = vec![json!({"type":"path", "value":"src/lib.rs"})];
    let prepared = tools
        .prepare_context(&messages, &Cancellation::default())
        .await
        .unwrap()
        .unwrap();
    assert!(
        prepared
            .observations
            .iter()
            .any(|item| item["kind"] == "prefetch_unavailable")
    );
    assert!(
        !prepared
            .observations
            .iter()
            .any(|item| item["kind"] == "overlay_refresh")
    );
    assert_eq!(session.status().await.unwrap().repository_view, previous);
    assert!(tools.shutdown().await);
    assert_eq!(session.status().await.unwrap().repository_view, previous);
}

#[tokio::test]
async fn prefetch_waits_for_the_new_overlay_after_a_quick_patch() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = git_session(&f, true).await;
    let previous = session
        .status()
        .await
        .unwrap()
        .repository_view
        .view_snapshot_id;
    let (mut tools, _journal) = native(&f, session.clone(), "prefetch-after-patch");
    tools.prefetch = vec![json!({"type":"path", "value":"src/lib.rs"})];
    let patch = baseline_patch(&tools, "PrefetchedEdit");
    assert!(
        matches!(tools.execute(&patch, &Cancellation::default()).await,
        ToolOutcome::Success(value) if value["complete"] == true)
    );
    let prepared = tools
        .prepare_context(
            &[Message::User {
                text: "task".into(),
            }],
            &Cancellation::default(),
        )
        .await
        .unwrap()
        .unwrap();
    let prefetched = prepared
        .observations
        .iter()
        .find(|item| item["kind"] == "prefetch")
        .expect("prefetch should use the new task view");
    let view = session.status().await.unwrap().repository_view;
    assert_ne!(view.view_snapshot_id, previous);
    assert_eq!(
        prefetched["evidence"]["provenance"]["snapshot_id"],
        json!(view.view_snapshot_id)
    );
    assert!(
        !serde_json::to_string(prefetched)
            .unwrap()
            .contains("pub struct Baseline;")
    );
    assert!(tools.shutdown().await);
}

#[tokio::test]
async fn explicit_prefetch_is_read_only_host_evidence_and_rechecks_source_bytes() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = git_session(&f, true).await;
    let (mut tools, _journal) = native(&f, session, "prefetch-test");
    tools.working_set_enabled = false;
    tools.context.cache_enabled = false;
    let messages = vec![Message::User {
        text: "bound task and constraints".into(),
    }];
    assert!(
        tools
            .prepare_context(&messages, &Cancellation::default())
            .await
            .unwrap()
            .is_none()
    );
    tools.prefetch = vec![json!({"type":"path", "value":"src/lib.rs"})];
    let events = f.events();
    let tasks = project::list_tasks().await.unwrap();
    let prepared = tools
        .prepare_context(&messages, &Cancellation::default())
        .await
        .unwrap()
        .unwrap();
    assert!(prepared.replacements.is_empty());
    assert_eq!(prepared.observations.len(), 1);
    assert_eq!(prepared.observations[0]["kind"], "prefetch");
    assert_eq!(
        prepared.observations[0]["evidence"]["binding"]["task"],
        TASK
    );
    assert!(
        !prepared.observations[0]["evidence"]["sources"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let projected = prepared.apply(&messages).unwrap();
    assert_eq!(projected.len(), 2);
    assert_eq!(projected[0], messages[0]);
    assert!(
        matches!(&projected[1], Message::User {text} if text.contains("Ferrus host observation") && text.contains("pub struct Baseline;"))
    );
    std::fs::write(f.root.join("src/lib.rs"), "pub struct ExternalChange;\n").unwrap();
    let next = tools
        .prepare_context(&messages, &Cancellation::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(next.observations[0]["kind"], "prefetch_unavailable");
    assert!(
        !serde_json::to_string(&next.apply(&messages).unwrap())
            .unwrap()
            .contains("pub struct Baseline;")
    );
    assert_eq!(messages.len(), 1);
    f.assert_no_effect(tasks, events).await;
    assert!(tools.shutdown().await);
}
