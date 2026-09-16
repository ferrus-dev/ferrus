//! Instruction loading and native graph/memory parity under managed authority.

use super::*;

#[tokio::test]
async fn instructions_are_scoped_lazy_reloaded_and_bounded() {
    use crate::nano::instructions::{Instructions, Limits};
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    std::fs::create_dir_all(f.root.join("src/deep")).unwrap();
    std::fs::create_dir_all(f.root.join("other")).unwrap();
    std::fs::create_dir_all(f.root.join(".agents/skills/rust")).unwrap();
    std::fs::create_dir_all(f.root.join(".agents/skills/unused")).unwrap();
    for (path, text) in [
        ("AGENTS.md", "Root rules"),
        ("src/AGENTS.md", "Source rules"),
        ("src/deep/AGENTS.md", "Deep rules"),
        ("other/AGENTS.md", "Unrelated rules"),
        (".agents/skills/rust/SKILL.md", "Selected skill"),
        (".agents/skills/unused/SKILL.md", "Never preload"),
    ] {
        std::fs::write(f.root.join(path), text).unwrap();
    }
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    let instructions = Instructions::new(session.clone(), Limits::default()).unwrap();
    let before = instructions
        .load(&["src/deep/new.rs".into()], &[])
        .await
        .unwrap();
    assert_eq!(before.documents.len(), 5);
    assert!(
        before
            .documents
            .iter()
            .all(|d| !d.path.contains("skills") && !d.path.contains("other"))
    );
    let old = before
        .documents
        .iter()
        .find(|d| d.path == "src/AGENTS.md")
        .unwrap();
    std::fs::write(f.root.join("src/AGENTS.md"), "Changed source constraints").unwrap();
    let after = instructions
        .load(&["src/deep/new.rs".into()], &["rust".into()])
        .await
        .unwrap();
    let new = after
        .documents
        .iter()
        .find(|d| d.path == "src/AGENTS.md")
        .unwrap();
    assert_ne!(old.digest, new.digest);
    assert_eq!(after.documents.len(), 6);
    assert!(
        after
            .constraint_text(4096)
            .unwrap()
            .contains("Changed source constraints")
    );
    assert!(after.constraint_text(64).is_err());
    assert!(
        instructions
            .load(&["../outside".into()], &[])
            .await
            .is_err()
    );
    assert!(instructions.load(&[], &["../rust".into()]).await.is_err());
    assert!(instructions.load(&[], &["missing".into()]).await.is_err());
    let limited = Instructions::new(
        session.clone(),
        Limits {
            file_bytes: 4,
            ..Limits::default()
        },
    )
    .unwrap();
    assert!(limited.load(&[], &[]).await.is_err());
    let limited = Instructions::new(
        session,
        Limits {
            documents: 3,
            ..Limits::default()
        },
    )
    .unwrap();
    assert!(
        limited
            .load(&["src/deep/new.rs".into()], &[])
            .await
            .is_err()
    );
    std::fs::remove_file(f.root.join("src/AGENTS.md")).unwrap();
    assert!(
        !instructions
            .load(&["src/deep/new.rs".into()], &[])
            .await
            .unwrap()
            .documents
            .iter()
            .any(|d| d.path == "src/AGENTS.md")
    );
}

#[tokio::test]
async fn instruction_recovery_requires_current_task_and_rejection() {
    use crate::nano::instructions::{Instructions, Limits};
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    f.connection()
        .execute(
            "UPDATE tasks SET status = 'addressing', review_cycles = 1 WHERE id = ?1",
            [TASK],
        )
        .unwrap();
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    assert!(
        Instructions::new(session.clone(), Limits::default())
            .unwrap()
            .load(&[], &[])
            .await
            .is_err()
    );
    std::fs::create_dir_all(f.root.join(".ferrus/runs/t-001")).unwrap();
    std::fs::write(
        f.root.join(".ferrus/runs/t-001/REVIEW.md"),
        "Correct the rejected implementation.",
    )
    .unwrap();
    let recovered = Instructions::new(session.clone(), Limits::default())
        .unwrap()
        .load(&[], &[])
        .await
        .unwrap();
    assert_eq!(recovered.task_status, "addressing");
    assert_eq!(
        recovered.documents[2].text,
        "Correct the rejected implementation."
    );
    std::fs::write(f.root.join(".ferrus/tasks/t-001.md"), "Current task intent").unwrap();
    assert_eq!(
        Instructions::new(session, Limits::default())
            .unwrap()
            .load(&[], &[])
            .await
            .unwrap()
            .documents[1]
            .text,
        "Current task intent"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn instructions_reject_symlinks_and_hardlinks() {
    use crate::nano::instructions::{Instructions, Limits};
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let instructions = Instructions::new(
        FerrusSession::bind(f.launch()).await.unwrap(),
        Limits::default(),
    )
    .unwrap();
    std::fs::write(f.root.join("outside"), "Forbidden alias").unwrap();
    std::os::unix::fs::symlink(f.root.join("outside"), f.root.join("AGENTS.md")).unwrap();
    assert!(instructions.load(&[], &[]).await.is_err());
    std::fs::remove_file(f.root.join("AGENTS.md")).unwrap();
    std::fs::hard_link(f.root.join("outside"), f.root.join("AGENTS.md")).unwrap();
    assert!(instructions.load(&[], &[]).await.is_err());
}

#[tokio::test]
async fn native_context_uses_bound_root_and_never_builds_or_retargets() {
    use crate::nano::context::{Context, Request, Response};
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    let context = Context::new(session);
    let events = f.events();
    let tasks = project::list_tasks().await.unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    std::env::set_current_dir(elsewhere.path()).unwrap();
    let result = context
        .retrieve(
            "repository_graph_status",
            Request::parse("repository_graph_status", serde_json::json!({})).unwrap(),
        )
        .await
        .unwrap();
    let Response::RepositoryStatus(status) = result else {
        panic!("status response")
    };
    assert_eq!(status.repository.namespace.as_str(), "local:test-project");
    assert!(status.data.task_view_status.is_some());
    let memory = context
        .retrieve(
            "project_memory_status",
            Request::parse("project_memory_status", serde_json::json!({})).unwrap(),
        )
        .await
        .unwrap();
    let Response::MemoryStatus(memory) = memory else {
        panic!("memory status")
    };
    assert_eq!(memory.project.project_id.as_str(), "test-project");
    assert!(!f.data.join("repo-graph.db").exists());
    assert!(!f.data.join("project-memory.db").exists());
    f.connection()
        .execute("UPDATE runs SET status = 'exited' WHERE id = ?1", [RUN])
        .unwrap();
    assert!(
        context
            .retrieve(
                "repository_graph_status",
                Request::parse("repository_graph_status", serde_json::json!({})).unwrap()
            )
            .await
            .is_err()
    );
    assert!(context.fallback(serde_json::from_value(serde_json::json!({"operation":"read","reason":"missing","input":{"path":"file.rs"}})).unwrap(), &crate::nano::tools::Cancellation::default()).await.is_err());
    std::env::set_current_dir(&f.root).unwrap();
    f.assert_no_effect(tasks, events).await;
}

#[tokio::test]
async fn fallback_is_labeled_current_workspace_evidence() {
    use crate::nano::{context::Context, tools::Cancellation};
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let context = Context::new(FerrusSession::bind(f.launch()).await.unwrap());
    std::fs::write(f.root.join("sample.rs"), "pub struct LocalOnly;\n").unwrap();
    for reason in ["missing", "disabled", "stale", "ambiguous", "unsupported"] {
        let result = context.fallback(serde_json::from_value(serde_json::json!({"operation":"read","reason":reason,"input":{"path":"sample.rs"}})).unwrap(), &Cancellation::default()).await.unwrap();
        assert_eq!(result["kind"], "workspace_fallback");
        assert_eq!(result["requested_reason"], reason);
        assert_eq!(result["evidence"]["source"]["kind"], "workspace");
        assert_eq!(result["evidence"]["text"], "pub struct LocalOnly;\n");
    }
    let result = context.fallback(serde_json::from_value(serde_json::json!({"operation":"search","reason":"unsupported","input":{"query":"LocalOnly","paths":["sample.rs"]}})).unwrap(), &Cancellation::default()).await.unwrap();
    assert_eq!(
        result["evidence"]["matches"][0]["source"]["kind"],
        "workspace"
    );
    assert!(context.fallback(serde_json::from_value(serde_json::json!({"operation":"read","reason":"missing","input":{"path":".ferrus/tasks/t-001.md"}})).unwrap(), &Cancellation::default()).await.is_err());
}

#[test]
fn native_context_requires_domains_and_bounds_inputs() {
    use crate::nano::context::Request;
    for (name, value) in [
        (
            "project_context_search",
            serde_json::json!({"query":"thing"}),
        ),
        (
            "repository_search",
            serde_json::json!({"query":"thing","task_id":"other"}),
        ),
        (
            "repository_search",
            serde_json::json!({"query":"thing","paths":["../secret"]}),
        ),
        (
            "repository_search",
            serde_json::json!({"query":"thing","max_bytes":0}),
        ),
        (
            "project_context",
            serde_json::json!({"domain":"repository","seeds":[{"type":"task","value":"t-001"}]}),
        ),
        (
            "project_context",
            serde_json::json!({"domain":"memory","seeds":[{"type":"node","value":"n1"}]}),
        ),
    ] {
        assert!(Request::parse(name, value).is_err(), "{name}");
    }
}

#[tokio::test]
async fn native_graph_preserves_pinned_views_and_verified_snippets() {
    use crate::{
        nano::context::{Context, Request, Response},
        repository_graph::{domain::*, index::*, sqlite::*},
        repository_graph_runtime::LocalGraphContext,
    };
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let config = std::fs::read_to_string(f.root.join("ferrus.toml")).unwrap();
    std::fs::write(
        f.root.join("ferrus.toml"),
        format!("{config}\n[repository_graph]\nenabled = true\n"),
    )
    .unwrap();
    std::fs::create_dir(f.root.join("src")).unwrap();
    std::fs::write(f.root.join("src/lib.rs"), "pub struct BaselineSymbol;\n").unwrap();
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    session.claim().await.unwrap();
    let local = LocalGraphContext::load_for_runtime(
        &f.root,
        session.project_id(),
        session.data_dir(),
        &session.status().await.unwrap(),
    )
    .await
    .unwrap();
    let index = |id: &str| {
        let source = local.discover().unwrap();
        let OpenSidecarResult::Ready(mut sidecar) =
            open_for_build_at(&f.data.join(SIDECAR_FILE_NAME)).unwrap()
        else {
            panic!("sidecar")
        };
        IndexCoordinator::new(&mut sidecar)
            .index(
                &source,
                &local.config,
                IndexRequest {
                    build_id: BuildId::new(id).unwrap(),
                    view_name: PublishedViewName::new("canonical").unwrap(),
                    force_full: false,
                },
            )
            .unwrap()
            .snapshot
            .id
    };
    let baseline = index("nano-baseline");
    project::record_task_repository_view(
        TASK,
        &project::RepositoryViewReference::new(
            Some(baseline.clone()),
            None,
            project::RepositoryViewStatus::Available,
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let native = Context::new(session.clone());
    let input = serde_json::json!({"seeds":[{"type":"path","value":"src/lib.rs"}],"include_snippets":true,"max_results":32,"max_bytes":24576,"max_depth":2,"max_duration_ms":1000,"max_diagnostics":16,"max_snippet_bytes":4096});
    let Response::RepositoryContext(Ok(response)) = native
        .retrieve(
            "repository_context",
            Request::parse("repository_context", input.clone()).unwrap(),
        )
        .await
        .unwrap()
    else {
        panic!("context")
    };
    let mcp: Value = serde_json::from_str(
        &crate::server::tools::repository_context::handler_for_agent(AGENT, input.clone())
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(serde_json::to_value(&response).unwrap(), mcp);
    assert_eq!(response.snapshot_id, baseline);
    assert!(
        serde_json::to_string(&response)
            .unwrap()
            .contains("pub struct BaselineSymbol;")
    );
    let events = f.events();
    let tasks = project::list_tasks().await.unwrap();
    std::fs::write(f.root.join("src/lib.rs"), "pub struct OverlaySymbol;\n").unwrap();
    let changed = native
        .retrieve(
            "repository_context",
            Request::parse("repository_context", input).unwrap(),
        )
        .await
        .unwrap();
    assert!(
        !serde_json::to_string(&changed)
            .unwrap()
            .contains("pub struct OverlaySymbol;")
    );
    let overlay = index("nano-overlay");
    let search =
        |q: &str| Request::parse("repository_search", serde_json::json!({"query":q})).unwrap();
    let Response::RepositorySearch(Ok(old)) = native
        .retrieve("repository_search", search("BaselineSymbol"))
        .await
        .unwrap()
    else {
        panic!("baseline search")
    };
    assert_eq!(
        old.snapshot_id, baseline,
        "canonical publication must not retarget the task"
    );
    f.assert_no_effect(tasks, events).await;
    f.connection().execute("UPDATE tasks SET repository_view_snapshot_id = ?1, overlay_revision_id = 'overlay-1' WHERE id = ?2", [overlay.as_str(), TASK]).unwrap();
    let Response::RepositorySearch(Ok(new)) = native
        .retrieve("repository_search", search("OverlaySymbol"))
        .await
        .unwrap()
    else {
        panic!("overlay search")
    };
    assert_eq!(new.snapshot_id, overlay);
    let task_view = new.task_view.unwrap();
    assert!(
        serde_json::to_string(&task_view)
            .unwrap()
            .contains(baseline.as_str())
    );
    assert!(
        serde_json::to_string(&task_view)
            .unwrap()
            .contains("overlay-1")
    );
    let bounded = native
        .retrieve(
            "repository_context",
            Request::parse(
                "repository_context",
                serde_json::json!({"seeds":[{"type":"path","value":"src/lib.rs"}],"max_bytes":1,"max_depth":2}),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let Response::RepositoryContext(result) = bounded else {
        panic!("bounded context")
    };
    let mcp: Value = serde_json::from_str(&crate::server::tools::repository_context::handler_for_agent(AGENT, serde_json::json!({"seeds":[{"type":"path","value":"src/lib.rs"}],"max_bytes":1,"max_results":64,"max_depth":2,"max_duration_ms":1000,"max_diagnostics":16})).await.unwrap()).unwrap();
    match result {
        Ok(response) => assert_eq!(serde_json::to_value(response).unwrap(), mcp),
        Err(error) => assert_eq!(serde_json::to_value(error).unwrap(), mcp),
    }
}

#[tokio::test]
async fn native_memory_only_ignores_disabled_or_invalid_graph_settings() {
    use crate::nano::context::{Context, Request, Response};
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    let config = std::fs::read_to_string(f.root.join("ferrus.toml")).unwrap();
    std::fs::write(
        f.root.join("ferrus.toml"),
        format!(
            "{config}\n[repository_graph]\nenabled = false\nunsupported_graph_setting = true\n"
        ),
    )
    .unwrap();
    let native = Context::new(session);
    assert!(matches!(
        native
            .retrieve(
                "project_memory_status",
                Request::parse("project_memory_status", serde_json::json!({})).unwrap()
            )
            .await
            .unwrap(),
        Response::MemoryStatus(_)
    ));
    assert!(
        native
            .retrieve(
                "repository_graph_status",
                Request::parse("repository_graph_status", serde_json::json!({})).unwrap()
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn native_tool_composition_is_bounded_and_revalidates_authority() {
    use crate::nano::{
        coding::CodingTools, commands, instructions, native::NativeTools, tools::*, workspace,
    };
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let session = FerrusSession::bind(f.launch()).await.unwrap();
    let storage = f.root.join("native-tools");
    crate::nano::private::directory(&storage, true).unwrap();
    let coding = CodingTools {
        workspace: workspace::Workspace::new(&f.root, workspace::Limits::default()).unwrap(),
        commands: commands::Commands::trusted_local(
            &f.root,
            "native-tools",
            &storage,
            commands::Limits::default(),
        )
        .unwrap(),
    };
    let mut tools = NativeTools::new(session, coding, instructions::Limits::default()).unwrap();
    let names: Vec<_> = tools.descriptors().into_iter().map(|d| d.name).collect();
    assert_eq!(names.len(), 15);
    assert!(
        !names
            .iter()
            .any(|n| n == "submit" || n == "create_task" || n == "archive_spec")
    );
    let call = ValidatedCall {
        call_id: "c1".into(),
        provider_call_id: "p1".into(),
        name: "load_instructions".into(),
        arguments: serde_json::json!({}),
    };
    tools.validate(&call.name, &call.arguments).unwrap();
    let ToolOutcome::Success(value) = tools.execute(&call, &Cancellation::default()).await else {
        panic!("instructions")
    };
    assert_eq!(value["documents"].as_array().unwrap().len(), 2);
    assert_eq!(value["documents"][0]["kind"], "runtime_policy");
    let cancel = Cancellation::default();
    cancel.cancel();
    assert_eq!(
        tools.execute(&call, &cancel).await,
        ToolOutcome::Failed(ToolError::Interrupted)
    );
    f.connection()
        .execute("UPDATE runs SET status = 'exited' WHERE id = ?1", [RUN])
        .unwrap();
    assert_eq!(
        tools.execute(&call, &Cancellation::default()).await,
        ToolOutcome::Failed(ToolError::Denied)
    );
    assert!(tools.shutdown().await);
}

#[tokio::test]
async fn native_memory_preserves_revision_and_source_policy() {
    use crate::{
        nano::context::{Context, Request, Response},
        project_memory::{index, source::LocalMemorySource},
        project_memory_runtime::LocalProjectContext,
    };
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .current_dir(&f.root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    };
    git(&["init", "--quiet"]);
    git(&["config", "core.autocrlf", "false"]);
    std::fs::create_dir_all(f.root.join("docs/specs")).unwrap();
    std::fs::write(f.root.join("docs/specs/native.md"), "# Native context\n\n## Milestone 1: Memory retrieval\n\nID: native-memory\n\nKeep revisions independent.\n").unwrap();
    git(&["add", "docs/specs/native.md"]);
    let baseline = git(&["write-tree"]);
    std::fs::create_dir_all(f.data.join("worktrees/.baseline-trees")).unwrap();
    std::fs::write(
        f.data.join("worktrees/.baseline-trees/t-001.txt"),
        &baseline,
    )
    .unwrap();
    let mut launch = f.launch();
    launch.baseline_tree = Some(baseline);
    let session = FerrusSession::bind(launch).await.unwrap();
    session.claim().await.unwrap();
    let source = LocalMemorySource::discover_current().await.unwrap();
    let mut store = crate::project_memory::sqlite::MemorySidecar::open_at(&f.data).unwrap();
    let indexed = index::MemoryIndexer::new(&source, &mut store)
        .unwrap()
        .index(index::MemoryIndexOptions::default())
        .unwrap();
    drop(store);
    let events = f.events();
    let tasks = project::list_tasks().await.unwrap();
    let native = Context::new(session);
    let Response::MemoryStatus(status) = native.retrieve("project_memory_status", Request::parse("project_memory_status", serde_json::json!({"max_results":32,"max_bytes":24576,"max_duration_ms":1000,"max_diagnostics":16})).unwrap()).await.unwrap() else { panic!("memory status") };
    let existing = LocalProjectContext::load_for_agent(AGENT, false, false)
        .await
        .unwrap();
    let budget = existing
        .requested_budget(
            Some(32),
            Some(24576),
            Some(4096),
            Some(2),
            Some(1000),
            Some(16),
        )
        .unwrap();
    assert_eq!(status, existing.memory_status(budget).unwrap());
    assert_eq!(status.revision_id, Some(indexed.revision.id.clone()));
    let Response::ProjectSearch(Ok(search)) = native
        .retrieve(
            "project_context_search",
            Request::parse(
                "project_context_search",
                serde_json::json!({"domain":"memory","query":"Native"}),
            )
            .unwrap(),
        )
        .await
        .unwrap()
    else {
        panic!("memory search")
    };
    assert_eq!(
        search.memory.unwrap().revision_id,
        Some(indexed.revision.id.clone())
    );
    assert!(search.repository.is_none());
    assert!(!search.results.is_empty());
    let Response::ProjectContext(Ok(context)) = native.retrieve("project_context", Request::parse("project_context", serde_json::json!({"domain":"memory","seeds":[{"type":"path","value":"docs/specs/native.md"}],"include_snippets":true})).unwrap()).await.unwrap() else { panic!("memory context") };
    assert_eq!(
        context.memory.unwrap().revision_id,
        Some(indexed.revision.id)
    );
    assert!(context.repository.is_none());
    assert!(!f.data.join("repo-graph.db").exists());
    f.assert_no_effect(tasks, events).await;
}
