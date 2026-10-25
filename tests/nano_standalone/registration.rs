//! Tests for registration.

use super::*;

#[test]
fn memory_context_is_explicit_read_only_and_independent_of_registration() {
    use ferrus::project_memory::{
        domain::*, index::*, policy::MemoryPolicy, source::LocalMemorySource, sqlite::MemorySidecar,
    };
    use ferrus::repository_graph::domain::RepoPath;
    let fixture = Fixture::new();
    success(
        Command::new("git")
            .arg("init")
            .arg("--object-format=sha1")
            .arg(&fixture.root),
    );
    fs::create_dir_all(fixture.root.join("docs/specs")).unwrap();
    fs::write(fixture.root.join("docs/specs/standalone.md"), "# Standalone memory\n\n- [ ] #1.0 Standalone memory milestone\n\nID: standalone-memory\nDepends on: none\n").unwrap();
    success(Command::new("git").current_dir(&fixture.root).args([
        "add",
        "--",
        "docs/specs/standalone.md",
    ]));
    let memory_data = fixture.root.parent().unwrap().join("memory");
    fs::create_dir(&memory_data).unwrap();
    let project = ProjectRef {
        namespace: ProjectNamespace::new("local:standalone-test").unwrap(),
        project_id: ProjectId::new("memory-fixture").unwrap(),
    };
    let source = LocalMemorySource::discover_at(
        fixture.root.clone(),
        memory_data.clone(),
        project,
        RepoPath::new("docs/specs").unwrap(),
        MemoryPolicy::default(),
    )
    .unwrap();
    let mut sidecar = MemorySidecar::open_at(&memory_data).unwrap();
    let outcome = MemoryIndexer::new(&source, &mut sidecar)
        .unwrap()
        .index(MemoryIndexOptions::default())
        .unwrap();
    drop(sidecar);
    let path = memory_data.join("project-memory.db");
    let before = fs::read(&path).unwrap();
    success(
        Command::new(env!("CARGO_BIN_EXE_ferrus-nano"))
            .env("HOME", &fixture.home)
            .env("USERPROFILE", &fixture.home)
            .args([
                "--workspace",
                fixture.root.to_str().unwrap(),
                "--storage",
                fixture.store.to_str().unwrap(),
                "--index-graph",
            ]),
    );
    let provider = Provider::new(vec![
        calls(&[
            ("project_memory_status", json!({})),
            (
                "project_context_search",
                json!({"domain":"memory","query":"Standalone memory"}),
            ),
            (
                "project_context_search",
                json!({"domain":"memory","query":"Standalone memory","kinds":["milestone"]}),
            ),
            (
                "project_context_search",
                json!({"domain":"all","query":"Standalone memory","kinds":["milestone"]}),
            ),
        ]),
        answer("Read structural memory."),
    ]);
    fixture.configure(&provider.url, "");
    success(fixture.command("memory-query").args([
        "--graph",
        "--memory-sidecar",
        path.to_str().unwrap(),
        "--memory-namespace",
        "local:standalone-test",
        "--memory-project",
        "memory-fixture",
        "--prompt",
        "Inspect explicit project memory.",
    ]));
    let requests = provider.finish();
    assert_eq!(before, fs::read(&path).unwrap());
    let results: Vec<_> = fixture
        .records("memory-query")
        .into_iter()
        .filter(|record| record["event"]["event"] == "tool_result")
        .collect();
    assert_eq!(
        results[0]["event"]["outcome"]["content"]["result"]["revision_id"],
        json!(outcome.revision.id)
    );
    assert_eq!(results[1]["event"]["outcome"]["status"], "success");
    assert!(results[1]["event"]["outcome"]["content"]["result"]["Ok"].is_object());
    let unfiltered = results[1]["event"]["outcome"]["content"]["result"]["Ok"]["results"]
        .as_array()
        .unwrap();
    assert!(
        unfiltered
            .iter()
            .any(|hit| hit["result"]["entity"]["data"]["type"] == "specification")
    );
    assert!(
        unfiltered
            .iter()
            .any(|hit| hit["result"]["entity"]["data"]["type"] == "milestone")
    );
    for filtered in &results[2..] {
        let hits = filtered["event"]["outcome"]["content"]["result"]["Ok"]["results"]
            .as_array()
            .unwrap();
        let memory: Vec<_> = hits
            .iter()
            .filter(|hit| hit["domain"] == "memory")
            .collect();
        assert!(!memory.is_empty());
        assert!(
            memory
                .iter()
                .all(|hit| hit["result"]["entity"]["data"]["type"] == "milestone")
        );
    }

    let selected: Vec<_> = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .map(|message| serde_json::from_str::<Value>(message["content"].as_str().unwrap()).unwrap())
        .collect();
    assert_eq!(selected.len(), results.len());
    for (selected, recorded) in selected.iter().zip(&results) {
        assert_eq!(selected, &recorded["event"]["outcome"]);
    }

    assert!(!find_database(&memory_data));
    fixture.assert_unregistered();
}
