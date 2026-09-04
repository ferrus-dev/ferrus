//! Unix process groups, parent monitoring, shell execution, and terminal input modes.

use std::io::Stdout;
use std::process::Command as StdCommand;

use anyhow::Result;
use crossterm::{
    event::{KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags},
    queue,
};

use super::ShutdownSignal;

#[cfg(target_os = "linux")]
mod linux;

pub(crate) fn set_serve_process_name() {
    #[cfg(target_os = "linux")]
    linux::set_serve_process_name();
}

pub(crate) fn install_serve_parent_lifecycle_hooks() {
    #[cfg(target_os = "linux")]
    linux::install_serve_parent_death_signal();

    install_serve_parent_watcher();
}

fn install_serve_parent_watcher() {
    let parent_pid = unsafe { libc::getppid() };
    if parent_pid <= 1 {
        return;
    }

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            interval.tick().await;
            let current_ppid = unsafe { libc::getppid() };
            if current_ppid <= 1 || current_ppid != parent_pid {
                std::process::exit(0);
            }
        }
    });
}

pub(crate) fn configure_headless_command(command: &mut StdCommand) {
    use std::os::unix::process::CommandExt;

    // SAFETY: these libc calls are async-signal-safe and operate only on the
    // child process between fork and exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }

            #[cfg(target_os = "linux")]
            linux::install_headless_child_lifecycle_hook()?;

            Ok(())
        });
    }
}

pub(crate) struct HeadlessProcessGuard;

pub(crate) fn attach_headless_process(_pid: u32) -> Result<HeadlessProcessGuard> {
    Ok(HeadlessProcessGuard)
}

pub(crate) fn signal_process(pid: u32, signal: ShutdownSignal) {
    unsafe {
        let signal = match signal {
            ShutdownSignal::Terminate => libc::SIGTERM,
            ShutdownSignal::Kill => libc::SIGKILL,
        };
        libc::kill(pid as libc::pid_t, signal);
    }
}

pub(crate) fn signal_process_group(pid: u32, signal: ShutdownSignal) {
    unsafe {
        let signal = match signal {
            ShutdownSignal::Terminate => libc::SIGTERM,
            ShutdownSignal::Kill => libc::SIGKILL,
        };
        libc::kill(-(pid as libc::pid_t), signal);
    }
}

pub(crate) fn pid_is_alive(pid: u32) -> bool {
    let ret = unsafe { libc::kill(pid as i32, 0) };
    if ret == 0 {
        return true;
    }

    let err = std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or_default();
    err == libc::EPERM
}

pub(crate) fn shell_command(cmd: &str) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("sh");
    command.arg("-c").arg(cmd);
    command
}

pub(crate) fn flush_stdin_input_buffer() {
    // SAFETY: tcflush discards bytes queued on stdin. Errors are ignored because
    // some environments do not expose a flushable TTY.
    unsafe {
        let _ = libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
    }
}

pub(crate) fn enter_tui(stdout: &mut Stdout) {
    let _ = queue!(
        stdout,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
        )
    );
}

pub(crate) fn leave_tui(stdout: &mut Stdout) {
    let _ = queue!(stdout, PopKeyboardEnhancementFlags);
}

#[cfg(test)]
mod tests {
    //! Unix shell selection and the no-op process attachment guard.

    use super::*;
    use std::process::Command;

    #[test]
    fn shell_command_uses_non_login_sh() {
        assert_program_and_args(
            shell_command("printf test").into_std(),
            "sh",
            &["-c", "printf test"],
        );
    }

    #[test]
    fn attach_headless_process_is_noop_on_unix() {
        assert!(attach_headless_process(std::process::id()).is_ok());
    }

    fn assert_program_and_args(command: Command, program: &str, args: &[&str]) {
        assert_eq!(command.get_program().to_string_lossy(), program);
        assert_eq!(
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            args.iter()
                .map(|arg| (*arg).to_string())
                .collect::<Vec<_>>()
        );
    }
}
