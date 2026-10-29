//! Tests for cancellation.

use super::*;

#[tokio::test]
async fn native_consultant_human_wait_cancels_and_relaunches_without_taking_a_lease() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let executor = FerrusSession::bind(f.launch()).await.unwrap();
    executor.claim().await.unwrap();
    lifecycle::ask(&executor, false, QUESTION.into())
        .await
        .unwrap();
    let consultant = reviewer(&f, "human-consult", &f.root).await;
    let (native, journal) = review_native(&f, &consultant);
    let cancel = Cancellation::default();
    let engine = review::run(
        consultant,
        Limits::default(),
        script(vec![tool(
            "question",
            "ask_human",
            json!({"question":"Which public API should be used?"}),
        )]),
        native,
        journal,
        &cancel,
    );
    let interrupt = async {
        for _ in 0..100 {
            if project::task_human_question_owner(TASK)
                .await
                .unwrap()
                .is_some()
            {
                cancel.cancel();
                return;
            }

            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        panic!("No human question");
    };
    let (end, ()) = tokio::join!(engine, interrupt);
    assert_eq!(end.unwrap().reason, EndReason::Cancelled);
    assert_eq!(
        project::list_tasks()
            .await
            .unwrap()
            .into_iter()
            .find(|task| task.id == TASK)
            .unwrap()
            .claimed_by
            .as_deref(),
        Some(AGENT)
    );
    project::record_run_finished("human-consult", 0)
        .await
        .unwrap();
    std::fs::write(
        f.root.join(".ferrus/runs/t-001/ANSWER.md"),
        "Use the stable API",
    )
    .unwrap();
    let resumed = reviewer(&f, "human-resumed", &f.root).await;
    let end = decide(
        &f,
        resumed,
        vec![
            tool("context", "consultation_context", json!({})),
            tool(
                "answer",
                "respond_consult",
                json!({"response":"Use the stable API"}),
            ),
        ],
    )
    .await;
    assert_eq!(end.reason, EndReason::ConsultationResponded);
    assert_eq!(executor.status().await.unwrap().status, "consultation");
    assert_eq!(
        lifecycle::poll_answer(&executor, false)
            .await
            .unwrap()
            .unwrap()["answer"],
        "Use the stable API"
    );
    project::record_run_finished("human-resumed", 0)
        .await
        .unwrap();
    lifecycle::ask(&executor, false, QUESTION.into())
        .await
        .unwrap();
    let fresh = reviewer(&f, "fresh-consult", &f.root).await;
    let end = decide(
        &f,
        fresh,
        vec![
            tool("context", "consultation_context", json!({})),
            tool(
                "response",
                "respond_consult",
                json!({"response":"A new answer"}),
            ),
        ],
    )
    .await;
    assert_eq!(end.reason, EndReason::ConsultationResponded);
    let records =
        std::fs::read_to_string(f.data.join("nano/sessions/fresh-consult/events.jsonl")).unwrap();
    let started: serde_json::Value = serde_json::from_str(records.lines().next().unwrap()).unwrap();
    let input: serde_json::Value =
        serde_json::from_str(started["event"]["input"].as_str().unwrap()).unwrap();
    assert!(
        input.get("human_answer").is_none(),
        "An identical new consultation must not inherit an old human answer"
    );
}
