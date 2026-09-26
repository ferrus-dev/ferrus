//! Real child processes exercise pipe isolation, quotas, cancellation and tree cleanup.

use super::*;
use std::{
    fs,
    io::{Read, Write},
    process::{Command, Stdio},
};
use tempfile::TempDir;

struct Fixture {
    _root: TempDir,
    workspace: PathBuf,
    session: PathBuf,
    commands: Commands,
}

fn fixture(id: &str, limits: Limits) -> Fixture {
    let root = TempDir::new().unwrap();
    let canonical = root.path().canonicalize().unwrap();
    let workspace = canonical.join("workspace");
    fs::create_dir(&workspace).unwrap();
    let session = canonical.join(id);
    private::directory(&session, true).unwrap();
    let commands = Commands::trusted_local(&workspace, id, &session, limits).unwrap();
    Fixture {
        _root: root,
        workspace,
        session,
        commands,
    }
}

fn child_command() -> String {
    let exe = std::env::current_exe()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    #[cfg(unix)]
    let exe = format!("'{}'", exe.replace('\'', "'\\''"));
    #[cfg(windows)]
    let exe = format!("\"{exe}\"");
    format!("{exe} --exact nano::commands::tests::child_fixture --nocapture")
}

fn request(command: &str) -> ExecRequest {
    ExecRequest {
        command: command.into(),
        cwd: ".".into(),
        timeout_ms: 15_000,
    }
}

async fn launch(f: &mut Fixture, mode: &str) -> Snapshot {
    fs::write(f.workspace.join("fixture-mode"), mode).unwrap();
    f.commands
        .exec(request(&child_command()), &Cancellation::default())
        .await
        .unwrap()
}

async fn terminal(commands: &mut Commands, id: &str) -> Snapshot {
    terminal_with_timeout(commands, id, Duration::from_secs(20)).await
}

async fn terminal_with_timeout(commands: &mut Commands, id: &str, timeout: Duration) -> Snapshot {
    tokio::time::timeout(timeout, async {
        loop {
            let status = commands.read_process(id, MAX_WAIT_MS).await.unwrap();
            if status.completion != Completion::Running {
                return status;
            }
        }
    })
    .await
    .expect("command must terminate")
}

async fn until_file(path: &Path) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child readiness");
}

async fn dead(pid: u32) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if !crate::platform::pid_is_alive(pid) {
                return;
            }
            // An orphaned Linux zombie has exited and cannot write; PID 1 in a
            // container may not reap it promptly. Never signal that stale PID.
            #[cfg(target_os = "linux")]
            if fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| s.contains(") Z")) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("owned descendant must stop");
}

#[test]
// The fixture deliberately leaves descendants for the command supervisor to reap/stop.
#[allow(clippy::zombie_processes)]
fn child_fixture() {
    let Ok(mode) =
        std::env::var("FERRUS_NANO_CHILD_MODE").or_else(|_| fs::read_to_string("fixture-mode"))
    else {
        return;
    };
    match mode.as_str() {
        "streams" => {
            let mut input = Vec::new();
            std::io::stdin().read_to_end(&mut input).unwrap();
            assert!(input.is_empty());
            println!("STDIN_EOF");
            println!("CWD={}", std::env::current_dir().unwrap().display());
            for _ in 0..2000 {
                std::io::stdout().write_all(b"out-line\n").unwrap();
                std::io::stderr().write_all(b"err-line\n").unwrap();
            }
            println!("OUT_END");
            eprintln!("ERR_END");
        }
        "flood" => loop {
            std::io::stdout().write_all(&[b'x'; 8192]).unwrap();
            std::io::stderr().write_all(&[b'y'; 8192]).unwrap();
        },
        "exit" => std::process::exit(7),
        "wait" | "leaf" => {
            fs::write(format!("{mode}.pid.tmp"), std::process::id().to_string()).unwrap();
            fs::rename(format!("{mode}.pid.tmp"), format!("{mode}.pid")).unwrap();
            loop {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        "descendant" | "descendant_exit" => {
            let _child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "nano::commands::tests::child_fixture",
                    "--nocapture",
                ])
                .env("FERRUS_NANO_CHILD_MODE", "leaf")
                .stdin(Stdio::null())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap();
            while !Path::new("leaf.pid").exists() {
                std::thread::sleep(Duration::from_millis(10));
            }
            fs::write("ready", "ready").unwrap();
            if mode == "descendant" {
                loop {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
        "environment" => {
            for name in [
                "OPENAI_API_KEY",
                "ANTHROPIC_API_KEY",
                "FERRUS_NANO_SECRET",
                "BASH_ENV",
                "ENV",
                "GIT_CONFIG_COUNT",
            ] {
                assert!(
                    std::env::var_os(name).is_none(),
                    "unexpected inherited {name}"
                );
            }
            println!("CLEAN_ENVIRONMENT");
        }
        _ => panic!("unknown fixture"),
    }
}

#[cfg(windows)]
#[tokio::test]
async fn windows_shell_runs_builtin_without_parsing_its_own_path_as_input() {
    let mut f = fixture("shell-builtin", Limits::default());
    let started = f
        .commands
        .exec(request("echo NANO_SHELL_OK"), &Cancellation::default())
        .await
        .unwrap();
    let finished = terminal(&mut f.commands, &started.process_id).await;
    let stderr = f
        .commands
        .read_output(&finished.stderr.handle, 0, MAX_PAGE)
        .await
        .unwrap();
    assert_eq!(
        finished.completion,
        Completion::Exited {
            code: Some(0),
            success: true
        },
        "stderr: {stderr:?}"
    );
    let stdout = f
        .commands
        .read_output(&finished.stdout.handle, 0, MAX_PAGE)
        .await
        .unwrap();
    assert_eq!(stdout.text.trim(), "NANO_SHELL_OK");
    assert!(stderr.text.is_empty(), "stderr: {stderr:?}");
    assert!(f.commands.shutdown().await);
}

#[cfg(windows)]
#[tokio::test]
async fn windows_shell_preserves_quoted_paths_arguments_and_redirection() {
    let mut f = fixture("shell-quotes", Limits::default());
    fs::create_dir(f.workspace.join("tools & scripts")).unwrap();
    fs::write(
        f.workspace.join("tools & scripts/fixture.cmd"),
        "@echo off\r\necho %1\r\nexit /b 7\r\n",
    )
    .unwrap();
    let started = f
        .commands
        .exec(
            request(r#""tools & scripts\fixture.cmd" "value & spaces" > "captured output.txt""#),
            &Cancellation::default(),
        )
        .await
        .unwrap();
    let finished = terminal(&mut f.commands, &started.process_id).await;
    let stderr = f
        .commands
        .read_output(&finished.stderr.handle, 0, MAX_PAGE)
        .await
        .unwrap();
    assert_eq!(
        finished.completion,
        Completion::Exited {
            code: Some(7),
            success: false
        },
        "stderr: {stderr:?}"
    );
    assert_eq!(
        fs::read_to_string(f.workspace.join("captured output.txt"))
            .unwrap()
            .trim(),
        r#""value & spaces""#
    );
    assert!(f.commands.shutdown().await);
}

#[tokio::test]
async fn streams_are_spooled_paginated_and_stdin_is_eof() {
    let mut f = fixture("streams", Limits::default());
    let started_at = Instant::now();
    fs::write(f.workspace.join("fixture-mode"), "streams").unwrap();
    let started = f
        .commands
        .exec(
            ExecRequest {
                timeout_ms: 30_000,
                ..request(&child_command())
            },
            &Cancellation::default(),
        )
        .await
        .unwrap();
    assert_eq!(started.completion, Completion::Running);
    let finished = terminal_with_timeout(
        &mut f.commands,
        &started.process_id,
        Duration::from_secs(35),
    )
    .await;
    assert_eq!(
        finished.completion,
        Completion::Exited {
            code: Some(0),
            success: true
        },
        "elapsed: {:?}, snapshot: {finished:?}",
        started_at.elapsed()
    );
    assert!(finished.output_complete);
    assert_eq!(f.commands.potentially_active_writers(), 0);
    for (output, expected) in [(&finished.stdout, "OUT_END"), (&finished.stderr, "ERR_END")] {
        let mut cursor = 0;
        let mut text = String::new();
        loop {
            let page = f
                .commands
                .read_output(&output.handle, cursor, 137)
                .await
                .unwrap();
            assert!(page.next_offset - cursor <= 137);
            assert!(encode(&page, 16 * 1024).is_ok());
            text.push_str(&page.text);
            cursor = page.next_offset;
            if page.complete {
                break;
            }
        }
        assert_eq!(cursor, output.bytes);
        assert!(text.contains(expected));
        if expected == "OUT_END" {
            assert!(text.contains("STDIN_EOF"));
            assert!(text.contains("CWD="));
        }
    }
    let stored: Snapshot = serde_json::from_slice(
        &fs::read(
            f.session
                .join("commands")
                .join(format!("{}.json", started.process_id)),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(stored.completion, finished.completion);
    f.commands.shutdown().await;
    f.commands.shutdown().await;
}

#[tokio::test]
async fn nonzero_exit_is_not_a_check_receipt() {
    let mut f = fixture("nonzero", Limits::default());
    let started = launch(&mut f, "exit").await;
    let finished = terminal(&mut f.commands, &started.process_id).await;
    assert_eq!(
        finished.completion,
        Completion::Exited {
            code: Some(7),
            success: false
        }
    );
    let description = f
        .commands
        .descriptors()
        .into_iter()
        .find(|d| d.name == "exec")
        .unwrap()
        .description;
    assert!(
        description.contains("MUST use Ferrus check") && description.contains("Ferrus owns Git")
    );
    assert_eq!(finished.mutation_scope, "unknown");
}

#[tokio::test]
async fn quotas_bound_disk_usage_and_stop_unbounded_output() {
    for limits in [
        Limits {
            process_output_bytes: 16 * 1024,
            ..Limits::default()
        },
        Limits {
            total_bytes: STATE_BYTES + 24 * 1024,
            process_output_bytes: 24 * 1024,
            ..Limits::default()
        },
    ] {
        let mut f = fixture("quota", limits.clone());
        let started = launch(&mut f, "flood").await;
        let finished = terminal(&mut f.commands, &started.process_id).await;
        assert_eq!(finished.completion, Completion::OutputLimit);
        assert!(finished.stdout.bytes + finished.stderr.bytes <= limits.process_output_bytes);
        assert!(!finished.output_complete);
        let total: u64 = fs::read_dir(f.session.join("commands"))
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap().len())
            .sum();
        assert!(total <= limits.total_bytes);
        let page = f
            .commands
            .read_output(&finished.stdout.handle, finished.stdout.bytes, 100)
            .await
            .unwrap();
        assert!(page.truncated && !page.complete);
        f.commands.shutdown().await;
    }
}

#[tokio::test]
async fn timeout_cancellation_and_control_remain_independent() {
    let mut f = fixture(
        "control",
        Limits {
            duration_ms: 100,
            ..Limits::default()
        },
    );
    let started = launch(&mut f, "wait").await;
    let finished = terminal(&mut f.commands, &started.process_id).await;
    assert_eq!(finished.completion, Completion::TimedOut);

    let mut f = fixture("cancel", Limits::default());
    let cancellation = Cancellation::default();
    fs::write(f.workspace.join("fixture-mode"), "wait").unwrap();
    let started = f
        .commands
        .exec(request(&child_command()), &cancellation)
        .await
        .unwrap();
    until_file(&f.workspace.join("wait.pid")).await;
    let pid = fs::read_to_string(f.workspace.join("wait.pid"))
        .unwrap()
        .parse()
        .unwrap();
    // On the current-thread runtime, a heartbeat can tick during a process wait.
    let wait = f.commands.read_process(&started.process_id, 200);
    let control = async {
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancellation.cancel();
    };
    let (_, ()) = tokio::join!(wait, control);
    let finished = terminal(&mut f.commands, &started.process_id).await;
    assert_eq!(finished.completion, Completion::Cancelled);
    dead(pid).await;
}

#[tokio::test]
async fn stop_drop_and_parent_exit_clean_up_descendants() {
    for action in ["stop", "drop", "exit"] {
        let mut f = fixture("tree", Limits::default());
        let started = launch(
            &mut f,
            if action == "exit" {
                "descendant_exit"
            } else {
                "descendant"
            },
        )
        .await;
        until_file(&f.workspace.join("ready")).await;
        let pid = fs::read_to_string(f.workspace.join("leaf.pid"))
            .unwrap()
            .parse()
            .unwrap();
        if action == "stop" {
            f.commands.stop_process(&started.process_id).await.unwrap();
            let stopped = terminal(&mut f.commands, &started.process_id).await;
            assert_eq!(stopped.completion, Completion::Cancelled);
        } else if action == "exit" {
            assert_eq!(
                terminal(&mut f.commands, &started.process_id)
                    .await
                    .completion,
                Completion::Exited {
                    code: Some(0),
                    success: true
                }
            );
        } else {
            drop(f.commands);
        }
        dead(pid).await;
    }
}

#[tokio::test]
async fn environment_and_output_handles_are_session_scoped() {
    let mut f = fixture("first", Limits::default());
    let environment = ChildEnvironment::select(std::env::vars_os().chain([
        ("OPENAI_API_KEY".into(), "sentinel-provider-secret".into()),
        (
            "ANTHROPIC_API_KEY".into(),
            "sentinel-provider-secret".into(),
        ),
        (
            "FERRUS_NANO_SECRET".into(),
            "sentinel-provider-secret".into(),
        ),
        ("BASH_ENV".into(), "never-source-this".into()),
        ("ENV".into(), "never-source-this".into()),
        ("GIT_CONFIG_COUNT".into(), "123".into()),
    ]));
    f.commands.backend = TrustedLocal::new(&f.workspace, environment).unwrap();
    let started = launch(&mut f, "environment").await;
    let finished = terminal(&mut f.commands, &started.process_id).await;
    assert_eq!(
        finished.completion,
        Completion::Exited {
            code: Some(0),
            success: true
        }
    );
    let mut other = fixture("second", Limits::default());
    for handle in [
        finished.stdout.handle.as_str(),
        "../first/commands/first-p1-stdout",
        "events.jsonl",
    ] {
        assert_eq!(
            other
                .commands
                .read_output(handle, 0, 100)
                .await
                .unwrap_err(),
            ToolError::InvalidArguments
        );
    }
    assert_eq!(
        other
            .commands
            .stop_process(&started.process_id)
            .await
            .unwrap_err(),
        ToolError::InvalidArguments
    );
    assert_eq!(
        f.commands
            .read_output(&finished.stdout.handle, finished.stdout.bytes + 1, 1)
            .await
            .unwrap_err(),
        ToolError::InvalidArguments
    );
}

#[tokio::test]
async fn validation_cwd_concurrency_and_restart_fail_closed() {
    let mut f = fixture(
        "validation",
        Limits {
            concurrent: 1,
            processes: 2,
            ..Limits::default()
        },
    );
    assert!(f.commands.validate("exec", &json!({"command":"echo hi","cwd":".","timeout_ms":1,"env":{"OPENAI_API_KEY":"bad"}})).is_err());
    assert!(
        f.commands
            .exec(
                ExecRequest {
                    cwd: "..".into(),
                    ..request("echo should-not-run")
                },
                &Cancellation::default()
            )
            .await
            .is_err()
    );
    let started = launch(&mut f, "wait").await;
    assert_eq!(
        f.commands
            .exec(request("echo no"), &Cancellation::default())
            .await
            .unwrap_err(),
        ToolError::Denied
    );
    f.commands.stop_process(&started.process_id).await.unwrap();
    f.commands.shutdown().await;
    assert!(
        Commands::trusted_local(&f.workspace, "validation", &f.session, Limits::default()).is_err()
    );
    assert!(
        Commands::trusted_local(&f.workspace, "another", &f.session, Limits::default()).is_err()
    );
}

#[tokio::test]
async fn concurrent_commands_share_one_disk_allowance() {
    let limits = Limits {
        process_output_bytes: 32 * 1024,
        total_bytes: 2 * STATE_BYTES + 32 * 1024,
        ..Limits::default()
    };
    let mut f = fixture("shared-quota", limits.clone());
    let first = launch(&mut f, "flood").await;
    let second = launch(&mut f, "flood").await;
    let first = terminal(&mut f.commands, &first.process_id).await;
    let second = terminal(&mut f.commands, &second.process_id).await;
    assert_eq!(first.completion, Completion::OutputLimit);
    assert_eq!(second.completion, Completion::OutputLimit);
    let total: u64 = fs::read_dir(f.session.join("commands"))
        .unwrap()
        .map(|entry| entry.unwrap().metadata().unwrap().len())
        .sum();
    assert!(total <= limits.total_bytes);
    assert_eq!(
        f.commands
            .charged
            .load(std::sync::atomic::Ordering::Acquire),
        limits.total_bytes
    );
}

#[tokio::test]
async fn cancelling_busy_output_reconciles_retained_byte_cursors() {
    let mut f = fixture(
        "cancel-output",
        Limits {
            process_output_bytes: 64 * 1024 * 1024,
            total_bytes: 128 * 1024 * 1024,
            ..Limits::default()
        },
    );
    let started = launch(&mut f, "flood").await;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let snapshot = f
                .commands
                .read_process(&started.process_id, 0)
                .await
                .unwrap();
            if snapshot.stdout.bytes + snapshot.stderr.bytes > 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    f.commands.stop_process(&started.process_id).await.unwrap();
    let stopped = terminal(&mut f.commands, &started.process_id).await;
    assert_eq!(stopped.completion, Completion::Cancelled);
    for output in [&stopped.stdout, &stopped.stderr] {
        assert_eq!(
            output.bytes,
            fs::metadata(f.session.join("commands").join(&output.handle))
                .unwrap()
                .len()
        );
        let page = f
            .commands
            .read_output(&output.handle, output.bytes, 1)
            .await
            .unwrap();
        assert_eq!(page.next_offset, output.bytes);
    }
    assert!(f.commands.shutdown().await);
}

#[tokio::test]
async fn lost_supervisor_is_unknown_and_shutdown_rejects_further_work() {
    let mut f = fixture("unknown", Limits::default());
    let started = launch(&mut f, "wait").await;
    until_file(&f.workspace.join("wait.pid")).await;
    let pid = fs::read_to_string(f.workspace.join("wait.pid"))
        .unwrap()
        .parse()
        .unwrap();
    f.commands
        .entries
        .get(&started.process_id)
        .unwrap()
        .task
        .as_ref()
        .unwrap()
        .abort();
    assert_eq!(
        terminal(&mut f.commands, &started.process_id)
            .await
            .completion,
        Completion::Unknown
    );
    assert!(!f.commands.shutdown().await);
    dead(pid).await;
    assert_eq!(
        f.commands
            .exec(request("echo no"), &Cancellation::default())
            .await
            .unwrap_err(),
        ToolError::Interrupted
    );
    let stored: Snapshot = serde_json::from_slice(
        &fs::read(
            f.session
                .join("commands")
                .join(format!("{}.json", started.process_id)),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(stored.completion, Completion::Unknown);
}

#[tokio::test]
async fn explicit_cwd_uses_workspace_directory_validation() {
    let mut f = fixture("cwd", Limits::default());
    fs::create_dir(f.workspace.join("nested")).unwrap();
    fs::write(f.workspace.join("nested/fixture-mode"), "environment").unwrap();
    let started = f
        .commands
        .exec(
            ExecRequest {
                cwd: "nested".into(),
                ..request(&child_command())
            },
            &Cancellation::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        terminal(&mut f.commands, &started.process_id)
            .await
            .completion,
        Completion::Exited {
            code: Some(0),
            success: true
        }
    );
    for cwd in ["nested/..", ".git", "nested/fixture-mode"] {
        assert!(
            f.commands
                .exec(
                    ExecRequest {
                        cwd: cwd.into(),
                        ..request("echo no")
                    },
                    &Cancellation::default()
                )
                .await
                .is_err()
        );
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(f._root.path(), f.workspace.join("outside")).unwrap();
        assert!(
            f.commands
                .exec(
                    ExecRequest {
                        cwd: "outside".into(),
                        ..request("echo no")
                    },
                    &Cancellation::default()
                )
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn engine_completion_stops_background_coding_tools() {
    use super::super::{
        coding::CodingTools,
        engine::Engine,
        journal::{FileJournal, Quotas},
        provider::*,
        session::*,
    };
    struct FinalProvider;
    impl Provider for FinalProvider {
        async fn start(&mut self, _: ModelRequest) -> std::result::Result<(), ProviderError> {
            Ok(())
        }
        async fn next_event(
            &mut self,
        ) -> std::result::Result<Option<ProviderEvent>, ProviderError> {
            Ok(Some(ProviderEvent::Completed {
                response: ModelResponse {
                    finish: FinishReason::Stop,
                    text: "Done".into(),
                    calls: vec![],
                    continuation: None,
                },
                usage: None,
            }))
        }
    }
    struct TestHost;
    impl Host for TestHost {
        async fn authorize(&mut self, _: &ValidatedCall) -> std::result::Result<(), ToolError> {
            Ok(())
        }
        fn committed(&mut self, _: &Record) {}
    }
    let root = TempDir::new().unwrap();
    let workspace = root.path().canonicalize().unwrap();
    fs::write(workspace.join("fixture-mode"), "wait").unwrap();
    let journal = FileJournal::create(&workspace, "engine-session", Quotas::default()).unwrap();
    let mut commands = Commands::trusted_local(
        &workspace,
        "engine-session",
        journal.directory(),
        super::Limits::default(),
    )
    .unwrap();
    let started = commands
        .exec(request(&child_command()), &Cancellation::default())
        .await
        .unwrap();
    until_file(&workspace.join("wait.pid")).await;
    let pid = fs::read_to_string(workspace.join("wait.pid"))
        .unwrap()
        .parse()
        .unwrap();
    let tools = CodingTools {
        workspace: super::super::workspace::Workspace::new(
            &workspace,
            super::super::workspace::Limits::default(),
        )
        .unwrap(),
        commands,
    };
    let mut engine = Engine::new(
        SessionIdentity {
            session_id: "engine-session".into(),
            project_id: "project".into(),
            task_id: None,
            run_id: None,
        },
        super::super::session::Limits::default(),
        FinalProvider,
        tools,
        TestHost,
        journal,
    )
    .unwrap();
    let end = engine
        .run(
            SessionCommand::Start {
                input: "finish".into(),
            },
            &Cancellation::default(),
        )
        .await
        .unwrap();
    assert_eq!(end.reason, EndReason::ModelFinished);
    assert!(end.durable);
    let snapshot = engine
        .tools
        .commands
        .read_process(&started.process_id, 0)
        .await
        .unwrap();
    assert_eq!(snapshot.completion, Completion::Cancelled);
    assert_eq!(engine.tools.commands.potentially_active_writers(), 0);
    dead(pid).await;
}
