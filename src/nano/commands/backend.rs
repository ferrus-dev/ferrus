//! Trusted-local process ownership. No shell policy here is an OS security boundary.

use crate::platform::{self, HeadlessProcessGuard, ShutdownSignal};
use anyhow::{Result, ensure};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
};
use tokio::process::{Child, Command};

/// The host captures only execution necessities. Provider/MCP credentials, startup
/// hooks, Git overrides, and Ferrus authority are never inherited by tool children.
#[derive(Clone)]
pub(crate) struct ChildEnvironment(BTreeMap<OsString, OsString>);

impl ChildEnvironment {
    pub(crate) fn capture() -> Self {
        Self::select(std::env::vars_os())
    }

    pub(crate) fn select(values: impl IntoIterator<Item = (OsString, OsString)>) -> Self {
        const ALLOWED: &[&str] = &[
            "PATH",
            "HOME",
            "USERPROFILE",
            "SYSTEMROOT",
            "WINDIR",
            "COMSPEC",
            "PATHEXT",
            "TMP",
            "TEMP",
            "TMPDIR",
            "LANG",
            "LC_ALL",
        ];
        Self(
            values
                .into_iter()
                .filter(|(name, _)| {
                    name.to_str()
                        .is_some_and(|name| ALLOWED.contains(&name.to_ascii_uppercase().as_str()))
                })
                .collect(),
        )
    }
}

/// Substitute an enforced backend here later. Model arguments cannot select one.
pub(crate) trait ExecutionBackend {
    fn kind(&self) -> &'static str;
    fn spawn(&self, command: &str, cwd: &str) -> Result<Spawned>;
}

pub(crate) struct TrustedLocal {
    workspace: PathBuf,
    environment: ChildEnvironment,
}

impl TrustedLocal {
    pub(crate) fn new(workspace: &Path, environment: ChildEnvironment) -> Result<Self> {
        ensure!(
            workspace.is_absolute(),
            "Command workspace must be absolute"
        );
        let workspace = workspace.canonicalize()?;
        ensure!(workspace.is_dir(), "Command workspace must be a directory");
        Ok(Self {
            workspace,
            environment,
        })
    }

    fn command(&self, text: &str, cwd: &str) -> Result<Command> {
        let directory = super::super::workspace::command_directory(&self.workspace, cwd)
            .map_err(|_| anyhow::anyhow!("Invalid command directory"))?;

        #[cfg(unix)]
        let mut command = platform::shell_command(text);
        // Disable cmd.exe AutoRun even when the host has it configured.
        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new(
                self.environment
                    .0
                    .iter()
                    .find(|(key, _)| key.to_string_lossy().eq_ignore_ascii_case("SYSTEMROOT"))
                    .map(|(_, root)| PathBuf::from(root).join("System32/cmd.exe"))
                    .ok_or_else(|| anyhow::anyhow!("Missing Windows system directory"))?,
            );
            command.args(["/D", "/S", "/C", text]);
            command
        };

        command
            .current_dir(directory)
            .env_clear()
            .envs(&self.environment.0)
            .env("TERM", "dumb")
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        platform::configure_headless_command(command.as_std_mut());

        #[cfg(windows)]
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_SUSPENDED);

        Ok(command)
    }
}

impl ExecutionBackend for TrustedLocal {
    fn kind(&self) -> &'static str {
        "trusted_local"
    }

    fn spawn(&self, text: &str, cwd: &str) -> Result<Spawned> {
        let child = self.command(text, cwd)?.spawn()?;
        let pid = child
            .id()
            .ok_or_else(|| anyhow::anyhow!("Missing command PID"))?;

        let mut spawned = Spawned {
            child,
            tree: Arc::new(Mutex::new(ProcessTree {
                pid,
                job: None,
                stopped: false,
            })),
        };

        let job = platform::attach_headless_process(pid)?;
        spawned.tree.lock().unwrap().job = Some(job);

        #[cfg(windows)]
        resume(pid)?;

        // On any setup error Spawned::drop kills the still-owned process tree.
        spawned.child.stdin.take();
        Ok(spawned)
    }
}

pub(crate) struct Spawned {
    pub child: Child,
    pub tree: Arc<Mutex<ProcessTree>>,
}

impl Drop for Spawned {
    fn drop(&mut self) {
        self.tree.lock().unwrap().stop();
    }
}

pub(crate) struct ProcessTree {
    pid: u32,
    job: Option<HeadlessProcessGuard>,
    stopped: bool,
}

impl ProcessTree {
    pub(super) fn stop(&mut self) {
        if self.stopped {
            return;
        }
        // Windows closes the job, including all descendants. Unix kills the owned
        // process group before reaping its leader, so the ID cannot be reused here.
        #[cfg(unix)]
        platform::signal_process_group(self.pid, ShutdownSignal::Kill);
        #[cfg(windows)]
        if self.job.is_none() {
            platform::signal_process(self.pid, ShutdownSignal::Kill);
        }

        self.job.take();
        self.stopped = true;
    }
}

/// Observe Unix exit without reaping: tree cleanup must precede release of the PID.
pub(super) fn exited(process: &mut Spawned) -> std::io::Result<bool> {
    #[cfg(unix)]
    {
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        let pid = process.tree.lock().unwrap().pid;
        // SAFETY: waitid writes one initialized siginfo for this owned child; WNOHANG
        // does not block, and WNOWAIT leaves the leader available for Child::wait.
        let status = unsafe {
            libc::waitid(
                libc::P_PID,
                pid,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };

        if status != 0 {
            return Err(std::io::Error::last_os_error());
        }

        // SAFETY: waitid succeeded; si_pid is zero if no exit was available.
        Ok(unsafe { info.assume_init().si_pid() } != 0)
    }
    #[cfg(windows)]
    {
        process.child.try_wait().map(|status| status.is_some())
    }
}

#[cfg(windows)]
fn resume(pid: u32) -> Result<()> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First,
                Thread32Next,
            },
            Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME},
        },
    };
    // The primary thread is still suspended, so it cannot start another thread or
    // child before assignment to the kill-on-close job. Rust's primary-thread
    // handle API is unstable; find the one thread via the documented ToolHelp API.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        ensure!(
            snapshot != INVALID_HANDLE_VALUE,
            "Cannot inspect suspended command"
        );

        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };

        let mut found = false;
        let mut more = Thread32First(snapshot, &mut entry) != 0;

        while more {
            if entry.th32OwnerProcessID == pid {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if !thread.is_null() {
                    found = ResumeThread(thread) != u32::MAX;
                    CloseHandle(thread);
                }
                break;
            }
            more = Thread32Next(snapshot, &mut entry) != 0;
        }

        CloseHandle(snapshot);

        ensure!(found, "Cannot resume supervised command");
    }
    Ok(())
}
