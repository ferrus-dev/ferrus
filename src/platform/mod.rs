//! OS-specific process lifecycle, shell, and terminal operations behind a shared interface.

use anyhow::Result;
use std::io::Stdout;
use std::process::Command as StdCommand;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShutdownSignal {
    Terminate,
    Kill,
}

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use unix as imp;
#[cfg(windows)]
use windows as imp;

pub(crate) fn set_serve_process_name() {
    imp::set_serve_process_name();
}

pub(crate) fn install_serve_parent_lifecycle_hooks() {
    imp::install_serve_parent_lifecycle_hooks();
}

pub(crate) fn configure_headless_command(command: &mut StdCommand) {
    imp::configure_headless_command(command);
}

pub(crate) type HeadlessProcessGuard = imp::HeadlessProcessGuard;

pub(crate) fn attach_headless_process(pid: u32) -> Result<HeadlessProcessGuard> {
    imp::attach_headless_process(pid)
}

pub(crate) fn signal_process(pid: u32, signal: ShutdownSignal) {
    imp::signal_process(pid, signal);
}

pub(crate) fn signal_process_group(pid: u32, signal: ShutdownSignal) {
    imp::signal_process_group(pid, signal);
}

pub(crate) fn pid_is_alive(pid: u32) -> bool {
    imp::pid_is_alive(pid)
}

pub(crate) fn shell_command(cmd: &str) -> tokio::process::Command {
    imp::shell_command(cmd)
}

pub(crate) fn flush_stdin_input_buffer() {
    imp::flush_stdin_input_buffer();
}

pub(crate) fn enter_tui(stdout: &mut Stdout) {
    imp::enter_tui(stdout);
}

pub(crate) fn leave_tui(stdout: &mut Stdout) {
    imp::leave_tui(stdout);
}
