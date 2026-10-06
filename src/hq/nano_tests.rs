//! HQ retains dispatch/workspace ownership for the native protocol transport.

use super::*;
use crate::agents::{AgentRunMode, ExecutorAgent, HeadlessPromptTransport};
use std::{path::PathBuf, process::Command as StdCommand, sync::Arc, time::Duration};

struct Fixture {
    _dir: tempfile::TempDir,
    previous: PathBuf,
    root: PathBuf,
    data: PathBuf,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.previous).unwrap();
    }
}
impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let previous = std::env::current_dir().unwrap();
        let data = root.join(".ferrus/data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(root.join(".ferrus/tasks")).unwrap();
        std::fs::write(
            root.join(".ferrus/project.toml"),
            toml::to_string(
                &serde_json::json!({"project_id":"nano-hq", "name":"test", "data_dir":data}),
            )
            .unwrap(),
        )
        .unwrap();
        std::fs::write(data.join("project.toml"), toml::to_string(&serde_json::json!({"id":"nano-hq", "name":"test", "workspace_dir":root, "ferrus_dir":root.join(".ferrus"), "created_at":"2026-09-19T00:00:00Z", "last_opened_at":"2026-09-19T00:00:00Z", "version":1})).unwrap()).unwrap();
        std::fs::write(root.join("ferrus.toml"), "[checks]\ncommands=[]\n[limits]\nmax_check_retries=3\nmax_review_cycles=3\nmax_feedback_lines=30\nwait_timeout_secs=1\n").unwrap();
        std::fs::write(root.join(".ferrus/tasks/t-001.md"), "test").unwrap();
        std::env::set_current_dir(&root).unwrap();
        crate::project::record_task_status(
            "t-001",
            ".ferrus/tasks/t-001.md",
            crate::project::TaskStatus::Pending,
        )
        .await
        .unwrap();
        Self {
            _dir: dir,
            previous,
            root,
            data,
        }
    }
    fn dispatches(&self) -> i64 {
        rusqlite::Connection::open(self.data.join("ferrus.db"))
            .unwrap()
            .query_row(
                "SELECT executor_dispatches FROM tasks WHERE id='t-001'",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }
}

struct FakeNative {
    script: PathBuf,
    reject: bool,
}
impl ExecutorAgent for FakeNative {
    fn capabilities(&self) -> crate::agents::ExecutorCapabilities {
        crate::agents::ExecutorCapabilities {
            interactive: true,
            headless: true,
            native: true,
            event_output: true,
        }
    }
    fn name(&self) -> &'static str {
        "nano"
    }
    fn model(&self) -> Option<&str> {
        None
    }
    fn headless_prompt_transport(&self) -> HeadlessPromptTransport {
        HeadlessPromptTransport::Jsonl
    }
    fn validate_headless_launch(&self, _: &str, _: u32) -> Result<()> {
        anyhow::ensure!(!self.reject, "invalid native config");
        Ok(())
    }
    fn validate_interactive_launch(&self, _: &str, _: u32) -> Result<()> {
        anyhow::ensure!(!self.reject, "invalid native config");
        Ok(())
    }
    fn spawn_with_index(&self, _: AgentRunMode<'_>, _: u32) -> Result<StdCommand> {
        #[cfg(unix)]
        {
            let mut command = StdCommand::new("/bin/sh");
            command.arg(&self.script);
            Ok(command)
        }
        #[cfg(windows)]
        {
            let shell = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
                .join("System32")
                .join("WindowsPowerShell")
                .join("v1.0")
                .join("powershell.exe");
            let mut command = StdCommand::new(shell);
            command
                .args([
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                ])
                .arg(self.script.file_name().unwrap());
            Ok(command)
        }
    }
}

fn context(agent: FakeNative, debug: bool) -> HqContext {
    let (_, state) = watch::channel(None);
    let (messages, _) = tokio::sync::mpsc::unbounded_channel();
    let mut context = HqContext::new(state, Display(messages), debug);
    context.executor = Some(Arc::new(agent));
    context
}

#[tokio::test]
async fn native_setup_failures_do_not_consume_dispatches() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let mut ctx = context(
        FakeNative {
            script: f.root.join("missing-script"),
            reject: true,
        },
        false,
    );
    assert!(
        ctx.spawn_headless_executor_for_task("executor:nano:1", "", 1, "t-001")
            .await
            .is_err()
    );
    assert!(!f.root.join(".ferrus/logs").exists());
    assert!(!f.data.join("worktrees").exists());
    assert_eq!(f.dispatches(), 0);
    let script = f.root.join("malformed.ps1");
    std::fs::write(&script, "echo malformed-event\n").unwrap();
    let mut ctx = context(
        FakeNative {
            script,
            reject: false,
        },
        true,
    );
    assert!(
        ctx.spawn_headless_executor_for_task("executor:nano:1", "", 1, "t-001")
            .await
            .is_err()
    );
    assert!(ctx.headless.is_empty());
    assert_eq!(f.dispatches(), 0);
    let runs = crate::project::list_runs(10).await.unwrap();
    assert!(runs.iter().all(|run| run.status != "running"));
}

#[tokio::test]
async fn native_debug_launch_keeps_protocol_and_stop_cleans_up_the_process() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let script = f.root.join("protocol fixture.ps1");
    #[cfg(unix)]
    let source = r#"echo '{"version":1,"event":{"type":"ready"}}'
IFS= read -r start
[ "$start" = '{"version":1,"command":"start"}' ] || exit 10
IFS= read -r cancel
[ "$cancel" = '{"version":1,"command":"cancel"}' ] || exit 11
echo '{"version":1,"event":{"type":"ended","reason":{"reason":"cancelled"},"durable":true}}'
"#;
    // Use a stream reader for the open JSONL pipe, not cmd.exe's console-oriented set /p.
    #[cfg(windows)]
    let source = r#"$ErrorActionPreference = 'Stop'
[Console]::Out.WriteLine('{"version":1,"event":{"type":"ready"}}')
[Console]::Out.Flush()
if ([Console]::In.ReadLine() -cne '{"version":1,"command":"start"}') { exit 10 }
if ([Console]::In.ReadLine() -cne '{"version":1,"command":"cancel"}') { exit 11 }
[Console]::Out.WriteLine('{"version":1,"event":{"type":"ended","reason":{"reason":"cancelled"},"durable":true}}')
[Console]::Out.Flush()
exit 0
"#;
    std::fs::write(&script, source).unwrap();
    let mut ctx = context(
        FakeNative {
            script,
            reject: false,
        },
        true,
    );
    ctx.spawn_headless_executor_for_task("executor:nano:1", "unused external prompt", 1, "t-001")
        .await
        .unwrap();
    assert_eq!(f.dispatches(), 1);
    let handle = ctx.headless.remove("executor:nano:1").unwrap();
    let pid = handle.pid;
    let mut exit = handle.exit_rx.clone();
    let log = handle.log_path.clone();
    assert!(
        handle.is_alive(),
        "fixture exited before cancellation: {:?}\n{}",
        *exit.borrow(),
        std::fs::read_to_string(&log).unwrap()
    );
    handle.terminate().await;
    assert_eq!(
        *exit.borrow_and_update(),
        Some(0),
        "{}",
        std::fs::read_to_string(&log).unwrap()
    );
    assert!(!crate::platform::pid_is_alive(pid));
    assert!(std::fs::read_to_string(log).unwrap().contains("Cancelled"));
    assert_eq!(
        crate::project::list_tasks().await.unwrap()[0].status,
        "pending"
    );
}

#[tokio::test]
async fn native_conversation_attach_steer_detach_and_cancel_preserve_run_ownership() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let f = Fixture::new().await;
    let script = f.root.join("interactive fixture.ps1");
    #[cfg(unix)]
    let source = r#"echo '{"version":1,"event":{"type":"ready"}}'
IFS= read -r start
[ "$start" = '{"version":1,"command":"start"}' ] || exit 10
if [ -f second_attach.marker ]; then
    IFS= read -r cancel
    [ "$cancel" = '{"version":1,"command":"cancel"}' ] || exit 14
    exit 0
fi
IFS= read -r open
[ "$open" = '{"version":1,"command":"interact"}' ] || exit 11
IFS= read -r steer
[ "$steer" = '{"version":1,"command":{"steer":{"text":"Keep the API stable"}}}' ] || exit 12
IFS= read -r open
[ "$open" = '{"version":1,"command":"interact"}' ] || exit 13
echo attached > second_attach.marker
IFS= read -r cancel
[ "$cancel" = '{"version":1,"command":"cancel"}' ] || exit 14
exit 0
"#;
    #[cfg(windows)]
    let source = r#"$ErrorActionPreference = 'Stop'
[Console]::Out.WriteLine('{"version":1,"event":{"type":"ready"}}')
[Console]::Out.Flush()
if ([Console]::In.ReadLine() -cne '{"version":1,"command":"start"}') { exit 10 }
if (Test-Path 'second_attach.marker') {
    if ([Console]::In.ReadLine() -cne '{"version":1,"command":"cancel"}') { exit 14 }
    exit 0
}

if ([Console]::In.ReadLine() -cne '{"version":1,"command":"interact"}') { exit 11 }
if ([Console]::In.ReadLine() -cne '{"version":1,"command":{"steer":{"text":"Keep the API stable"}}}') { exit 12 }
if ([Console]::In.ReadLine() -cne '{"version":1,"command":"interact"}') { exit 13 }
Set-Content -Path 'second_attach.marker' -Value 'attached'
if ([Console]::In.ReadLine() -cne '{"version":1,"command":"cancel"}') { exit 14 }
exit 0
"#;
    std::fs::write(&script, source).unwrap();
    let mut ctx = context(
        FakeNative {
            script,
            reject: false,
        },
        false,
    );
    let name = "executor:nano:t-001";
    ctx.supervisor = Some(crate::agents::parse_supervisor_agent("codex", None).unwrap());
    ctx.nano_paused_tasks.insert("t-001".into());
    crate::project::record_task_status(
        "t-001",
        ".ferrus/tasks/t-001.md",
        crate::project::TaskStatus::Consultation,
    )
    .await
    .unwrap();
    store::write_consult_response_for_run_dir(".ferrus/runs/t-001", "Keep the API stable")
        .await
        .unwrap();
    ctx.reconcile_runtime_schedule().await.unwrap();
    assert!(ctx.headless.is_empty());
    crate::project::record_task_status(
        "t-001",
        ".ferrus/tasks/t-001.md",
        crate::project::TaskStatus::Executing,
    )
    .await
    .unwrap();
    crate::project::record_task_human_question_requested(
        "t-001",
        crate::project::TaskStatus::Executing,
        name,
    )
    .await
    .unwrap();
    crate::project::record_task_human_answer("t-001")
        .await
        .unwrap();
    ctx.reconcile_runtime_schedule().await.unwrap();
    assert!(ctx.headless.is_empty());
    assert_eq!(f.dispatches(), 0);
    ctx.nano_paused_tasks.clear();
    crate::project::record_task_status(
        "t-001",
        ".ferrus/tasks/t-001.md",
        crate::project::TaskStatus::Pending,
    )
    .await
    .unwrap();
    ctx.spawn_headless_executor_for_task(name, "", 1, "t-001")
        .await
        .unwrap();
    ctx.attach_nano_conversation(name).await.unwrap();
    let run = ctx.nano_view.as_ref().unwrap().run_id.clone();
    assert!(
        dispatch_with_human_question_target(
            "Wrong run",
            None,
            Some("foreign"),
            None,
            false,
            &mut ctx
        )
        .await
        .is_err()
    );
    dispatch_with_human_question_target(
        "Keep the API stable",
        None,
        Some(&run),
        None,
        false,
        &mut ctx,
    )
    .await
    .unwrap();
    dispatch("/detach", &mut ctx).await.unwrap();
    assert!(ctx.nano_view.is_none());
    assert!(ctx.headless[name].is_alive());
    ctx.attach_nano_conversation(name).await.unwrap();
    assert_eq!(ctx.nano_view.as_ref().unwrap().run_id, run);
    tokio::time::timeout(Duration::from_secs(10), async {
        while !f.root.join("second_attach.marker").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    dispatch("/cancel", &mut ctx).await.unwrap();
    let mut exit = ctx.headless[name].exit_rx.clone();
    tokio::time::timeout(Duration::from_secs(10), async {
        while exit.borrow().is_none() {
            exit.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert_eq!(*exit.borrow(), Some(0));
    assert_eq!(f.dispatches(), 1);
    ctx.supervisor = Some(crate::agents::parse_supervisor_agent("codex", None).unwrap());
    ctx.reconcile_runtime_schedule().await.unwrap();
    assert_eq!(f.dispatches(), 1);
    ctx.resume().await.unwrap();
    assert!(ctx.nano_paused_tasks.is_empty());
    assert_eq!(f.dispatches(), 2);
    ctx.shutdown_all_headless().await;
}

#[tokio::test]
async fn native_executor_opens_directly_without_consuming_ready_tasks() {
    let _guard = crate::test_support::cwd_lock().lock().unwrap();
    let fixture = Fixture::new().await;
    let script = fixture.root.join("taskless fixture.ps1");
    #[cfg(unix)]
    let source = r#"[ -z "$FERRUS_TASK_ID" ] && [ -z "$FERRUS_BASELINE_TREE" ] || exit 20
echo '{"version":1,"event":{"type":"ready"}}'
IFS= read -r start
[ "$start" = '{"version":1,"command":"start"}' ] || exit 10
IFS= read -r open
[ "$open" = '{"version":1,"command":"interact"}' ] || exit 11
IFS= read -r steer
[ "$steer" = '{"version":1,"command":{"steer":{"text":"Inspect this workspace"}}}' ] || exit 12
IFS= read -r open
[ "$open" = '{"version":1,"command":"interact"}' ] || exit 13
echo attached > direct_attach.marker
IFS= read -r cancel
[ "$cancel" = '{"version":1,"command":"cancel"}' ] || exit 14
exit 0
"#;
    #[cfg(windows)]
    let source = r#"$ErrorActionPreference = 'Stop'
if ($env:FERRUS_TASK_ID -or $env:FERRUS_BASELINE_TREE) { exit 20 }
[Console]::Out.WriteLine('{"version":1,"event":{"type":"ready"}}')
[Console]::Out.Flush()
if ([Console]::In.ReadLine() -cne '{"version":1,"command":"start"}') { exit 10 }
if ([Console]::In.ReadLine() -cne '{"version":1,"command":"interact"}') { exit 11 }
if ([Console]::In.ReadLine() -cne '{"version":1,"command":{"steer":{"text":"Inspect this workspace"}}}') { exit 12 }
if ([Console]::In.ReadLine() -cne '{"version":1,"command":"interact"}') { exit 13 }
Set-Content -Path 'direct_attach.marker' -Value 'attached'
if ([Console]::In.ReadLine() -cne '{"version":1,"command":"cancel"}') { exit 14 }
exit 0
"#;
    std::fs::write(&script, source).unwrap();
    let mut ctx = context(
        FakeNative {
            script,
            reject: false,
        },
        false,
    );
    ctx.supervisor = Some(crate::agents::parse_supervisor_agent("codex", None).unwrap());
    dispatch("/executor", &mut ctx).await.unwrap();
    let name = "executor:nano:1";
    let run = ctx.nano_view.as_ref().unwrap().run_id.clone();
    assert!(ctx.headless[name].task_id.is_none());
    assert_eq!(fixture.dispatches(), 0);
    dispatch_with_human_question_target(
        "Inspect this workspace",
        None,
        Some(&run),
        None,
        false,
        &mut ctx,
    )
    .await
    .unwrap();
    dispatch("/detach", &mut ctx).await.unwrap();
    dispatch("/executor", &mut ctx).await.unwrap();
    assert_eq!(ctx.nano_view.as_ref().unwrap().run_id, run);
    tokio::time::timeout(Duration::from_secs(10), async {
        while !fixture.root.join("direct_attach.marker").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    dispatch("/cancel", &mut ctx).await.unwrap();
    assert!(ctx.nano_paused_tasks.is_empty());
    ctx.shutdown_all_headless().await;
    assert_eq!(fixture.dispatches(), 0);
    assert_eq!(
        crate::project::list_tasks()
            .await
            .unwrap()
            .iter()
            .find(|task| task.id == "t-001")
            .unwrap()
            .status,
        "pending"
    );
    assert!(!fixture.data.join("worktrees").exists());
}
