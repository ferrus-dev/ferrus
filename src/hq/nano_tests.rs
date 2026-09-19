//! HQ retains dispatch/workspace ownership for the native protocol transport.

use super::*;
use crate::agents::{AgentRunMode, ExecutorAgent, HeadlessPromptTransport};
use std::{path::PathBuf, process::Command as StdCommand, sync::Arc};

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
    fn spawn_with_index(&self, _: AgentRunMode<'_>, _: u32) -> Result<StdCommand> {
        #[cfg(unix)]
        {
            let mut command = StdCommand::new("/bin/sh");
            command.arg(&self.script);
            Ok(command)
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            let shell = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
                .join("System32")
                .join("cmd.exe");
            let mut command = StdCommand::new(shell);
            command.args(["/D", "/S", "/C"]).raw_arg(format!(
                "\"\"{}\"\"",
                self.script.file_name().unwrap().to_string_lossy()
            ));
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
    let script = f.root.join("malformed.cmd");
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
    let script = f.root.join("protocol fixture.cmd");
    #[cfg(unix)]
    let source = "echo '{\"version\":1,\"event\":{\"type\":\"ready\"}}'\nread start\nread cancel\necho '{\"version\":1,\"event\":{\"type\":\"ended\",\"reason\":{\"reason\":\"cancelled\"},\"durable\":true}}'\n";
    #[cfg(windows)]
    let source = "@echo off\r\necho {\"version\":1,\"event\":{\"type\":\"ready\"}}\r\nset /p start=\r\nset /p cancel=\r\necho {\"version\":1,\"event\":{\"type\":\"ended\",\"reason\":{\"reason\":\"cancelled\"},\"durable\":true}}\r\n";
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
    assert!(handle.is_alive());
    handle.terminate().await;
    assert_eq!(*exit.borrow_and_update(), Some(0));
    assert!(!crate::platform::pid_is_alive(pid));
    assert!(std::fs::read_to_string(log).unwrap().contains("Cancelled"));
    assert_eq!(
        crate::project::list_tasks().await.unwrap()[0].status,
        "pending"
    );
}
