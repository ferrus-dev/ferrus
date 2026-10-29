//! Tests for human waits.

use super::*;

#[test]
fn answered_human_resume_only_blocks_live_question_owner() {
    let live_run_agents = HashSet::from(["executor:codex:t-001".to_string()]);

    assert!(!answered_human_owner_is_live(
        "supervisor:codex:t-001",
        &live_run_agents,
        false,
    ));
    assert!(answered_human_owner_is_live(
        "executor:codex:t-001",
        &live_run_agents,
        false,
    ));
    assert!(answered_human_owner_is_live(
        "supervisor:codex:t-001",
        &HashSet::new(),
        true,
    ));
}

#[test]
fn human_question_selection_uses_fifo_or_presented_task_id() {
    let question = |task_id: &str| crate::project::HumanQuestion {
        task_id: task_id.to_string(),
        task_path: format!(".ferrus/tasks/{task_id}.md"),
        run_dir: format!(".ferrus/runs/{task_id}"),
        question: format!("Question for {task_id}"),
    };
    let questions = vec![question("t-001"), question("t-002")];

    let selected = select_human_question(questions.clone(), None).unwrap();
    assert_eq!(selected.task_id, "t-001");

    let selected = select_human_question(questions, Some("t-002")).unwrap();
    assert_eq!(selected.task_id, "t-002");
}

#[tokio::test]
async fn answered_consultation_selection_returns_tasks_with_existing_response() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let previous = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();
    store::write_consult_response_for_run_dir(".ferrus/runs/t-002", "existing answer")
        .await
        .unwrap();
    let tasks = vec![
        task_record("t-001", crate::project::TaskStatus::Consultation),
        task_record("t-002", crate::project::TaskStatus::Consultation),
        task_record("t-003", crate::project::TaskStatus::Executing),
    ];

    let selected = answered_consultation_tasks(&tasks).await.unwrap();

    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].id, "t-002");
    std::env::set_current_dir(previous).unwrap();
}

#[tokio::test]
async fn plain_input_answers_first_scoped_human_question() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let previous = std::env::current_dir().unwrap();
    std::fs::create_dir_all(dir.path().join(".ferrus/tasks")).unwrap();
    std::fs::create_dir_all(dir.path().join(".ferrus/runs/t-007")).unwrap();
    std::env::set_current_dir(dir.path()).unwrap();

    let data_dir = dir.path().join(".ferrus/projects/test-project");
    tokio::fs::create_dir_all(&data_dir).await.unwrap();
    let local_ref = crate::project::LocalProjectRef {
        project_id: "test-project".to_string(),
        name: "test".to_string(),
        data_dir: data_dir.to_string_lossy().into_owned(),
    };
    tokio::fs::write(
        ".ferrus/project.toml",
        toml::to_string_pretty(&local_ref).unwrap(),
    )
    .await
    .unwrap();
    crate::project::record_task_status(
        "t-007",
        ".ferrus/tasks/t-007.md",
        crate::project::TaskStatus::Executing,
    )
    .await
    .unwrap();
    crate::project::record_task_human_question_requested(
        "t-007",
        crate::project::TaskStatus::Executing,
        "executor:codex:7",
    )
    .await
    .unwrap();
    store::write_question_for_run_dir(".ferrus/runs/t-007", "Need human input")
        .await
        .unwrap();

    let (_state_tx, state_rx) = watch::channel::<Option<WatchedState>>(None);
    let (msg_tx, _msg_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut ctx = HqContext::new(state_rx, Display(msg_tx), false);

    let error = dispatch("Use option A", &mut ctx)
        .await
        .unwrap_err()
        .to_string();

    crate::test_support::assert_no_state_json();
    assert!(error.contains("Executor agent is not configured"));
    assert_eq!(
        store::read_answer_for_run_dir(".ferrus/runs/t-007")
            .await
            .unwrap(),
        "Use option A"
    );
    assert_eq!(
        store::read_question_for_run_dir(".ferrus/runs/t-007")
            .await
            .unwrap(),
        "Need human input"
    );
    let tasks = crate::project::list_tasks().await.unwrap();
    let task = tasks.iter().find(|task| task.id == "t-007").unwrap();
    assert_eq!(task.status, "awaiting_human");

    std::env::set_current_dir(previous).unwrap();
}

#[tokio::test]
async fn queued_human_answers_follow_fifo_and_preserve_presented_task() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let previous = std::env::current_dir().unwrap();
    std::fs::create_dir_all(dir.path().join(".ferrus")).unwrap();
    std::env::set_current_dir(dir.path()).unwrap();

    let data_dir = dir.path().join("runtime");
    tokio::fs::create_dir_all(&data_dir).await.unwrap();
    let local_ref = crate::project::LocalProjectRef {
        project_id: "test-project".to_string(),
        name: "test".to_string(),
        data_dir: data_dir.to_string_lossy().into_owned(),
    };
    tokio::fs::write(
        ".ferrus/project.toml",
        toml::to_string_pretty(&local_ref).unwrap(),
    )
    .await
    .unwrap();

    for task_id in ["t-021", "t-022"] {
        crate::project::record_task_status(
            task_id,
            &format!(".ferrus/tasks/{task_id}.md"),
            crate::project::TaskStatus::Executing,
        )
        .await
        .unwrap();
        crate::project::record_task_human_question_requested(
            task_id,
            crate::project::TaskStatus::Executing,
            &format!("executor:codex:{task_id}"),
        )
        .await
        .unwrap();
        store::write_question_for_run_dir(
            &format!(".ferrus/runs/{task_id}"),
            &format!("Question for {task_id}"),
        )
        .await
        .unwrap();
    }

    let (_state_tx, state_rx) = watch::channel::<Option<WatchedState>>(None);
    let (msg_tx, _msg_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut ctx = HqContext::new(state_rx, Display(msg_tx), false);

    let error = dispatch("First answer", &mut ctx)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("Executor agent is not configured"));
    assert_eq!(
        store::read_answer_for_run_dir(".ferrus/runs/t-021")
            .await
            .unwrap(),
        "First answer"
    );
    assert!(!std::path::Path::new(".ferrus/runs/t-022/ANSWER.md").exists());
    let questions = crate::project::list_human_questions().await.unwrap();
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].task_id, "t-022");

    let error = dispatch_with_human_question_target(
        "Stale answer",
        Some("t-021"),
        None,
        None,
        false,
        &mut ctx,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("Task t-021 is not waiting"));
    assert!(!std::path::Path::new(".ferrus/runs/t-022/ANSWER.md").exists());

    let error = dispatch_with_human_question_target(
        "Use option B\n\n- preserve formatting",
        Some("t-022"),
        None,
        None,
        false,
        &mut ctx,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("Executor agent is not configured"));
    assert_eq!(
        store::read_answer_for_run_dir(".ferrus/runs/t-022")
            .await
            .unwrap(),
        "Use option B\n\n- preserve formatting"
    );

    std::env::set_current_dir(previous).unwrap();
}

#[tokio::test]
async fn plain_input_answers_scoped_human_question_without_state_json() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let previous = std::env::current_dir().unwrap();
    std::fs::create_dir_all(dir.path().join(".ferrus/tasks")).unwrap();
    std::fs::create_dir_all(dir.path().join(".ferrus/runs/t-009")).unwrap();
    std::env::set_current_dir(dir.path()).unwrap();

    let data_dir = dir.path().join(".ferrus/projects/test-project");
    tokio::fs::create_dir_all(&data_dir).await.unwrap();
    let local_ref = crate::project::LocalProjectRef {
        project_id: "test-project".to_string(),
        name: "test".to_string(),
        data_dir: data_dir.to_string_lossy().into_owned(),
    };
    tokio::fs::write(
        ".ferrus/project.toml",
        toml::to_string_pretty(&local_ref).unwrap(),
    )
    .await
    .unwrap();
    crate::project::record_task_status(
        "t-009",
        ".ferrus/tasks/t-009.md",
        crate::project::TaskStatus::Executing,
    )
    .await
    .unwrap();
    crate::project::record_task_human_question_requested(
        "t-009",
        crate::project::TaskStatus::Executing,
        "executor:codex:9",
    )
    .await
    .unwrap();
    store::write_question_for_run_dir(".ferrus/runs/t-009", "Need scoped input")
        .await
        .unwrap();

    let (_state_tx, state_rx) = watch::channel::<Option<WatchedState>>(None);
    let (msg_tx, _msg_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut ctx = HqContext::new(state_rx, Display(msg_tx), false);

    let error = dispatch("Use scoped answer", &mut ctx)
        .await
        .unwrap_err()
        .to_string();

    assert!(error.contains("Executor agent is not configured"));
    assert_eq!(
        store::read_answer_for_run_dir(".ferrus/runs/t-009")
            .await
            .unwrap(),
        "Use scoped answer"
    );
    assert_eq!(
        store::read_question_for_run_dir(".ferrus/runs/t-009")
            .await
            .unwrap(),
        "Need scoped input"
    );
    let task = crate::project::list_tasks()
        .await
        .unwrap()
        .into_iter()
        .find(|task| task.id == "t-009")
        .unwrap();
    assert_eq!(task.status, "awaiting_human");
    crate::test_support::assert_no_state_json();

    std::env::set_current_dir(previous).unwrap();
}
