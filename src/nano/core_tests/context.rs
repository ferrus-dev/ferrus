//! Tests for context.

use super::*;

#[tokio::test]
async fn context_assembly_includes_preparation_time() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut engine = Engine::new(
        identity(),
        limits(),
        scripted(vec![response("done", vec![])]),
        EvidenceTools {
            workspace: super::super::workspace::Workspace::new(
                &root,
                super::super::workspace::Limits::default(),
            )
            .unwrap(),
            enabled: true,
            preparation_delay: Duration::from_millis(40),
        },
        FakeHost::default(),
        FileJournal::create(&root, "session-1", Quotas::default()).unwrap(),
    )
    .unwrap();
    assert_eq!(
        run(&mut engine, &Cancellation::default()).await.reason,
        EndReason::ModelFinished
    );
    let assembly = engine
        .host
        .records
        .iter()
        .find_map(|record| match &record.event {
            SessionEvent::ContextComposed { composition } => composition.assembly_elapsed_ms,
            _ => None,
        })
        .unwrap();
    assert!(
        assembly >= 40,
        "assembly time omitted preparation: {assembly}"
    );
}
