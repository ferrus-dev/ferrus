//! Exercise the real terminal frontend without a live model or Ferrus project.
use super::*;
use std::{
    fs::File,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::process::CommandExt,
    },
    process::Child,
};

struct Terminal {
    child: Child,
    master: File,
    slave: File,
    output: Vec<u8>,
}
impl Terminal {
    fn start(command: &mut Command) -> Self {
        let (mut master, mut slave) = (-1, -1);
        let mut window = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: openpty initializes both descriptors; the size pointer is valid.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut window,
                )
            },
            0
        );
        // SAFETY: the successful openpty call returned two new owned descriptors.
        let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
        // SAFETY: fcntl only changes this test-owned master's blocking mode.
        assert_ne!(
            unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) },
            -1
        );
        // Keep the controlling session alive long enough to compare terminal modes
        // after Nano exits. macOS invalidates tcgetattr after its session leader dies.
        let mut launch = Command::new("/bin/sh");
        launch.args(["-c", "before=$(stty -g); \"$@\"; result=$?; after=$(stty -g); [ \"$before\" = \"$after\" ] || exit 98; exit \"$result\"", "nano-terminal"])
            .arg(command.get_program()).args(command.get_args());
        for (key, value) in command.get_envs() {
            if let Some(value) = value {
                launch.env(key, value);
            } else {
                launch.env_remove(key);
            }
        }
        if let Some(directory) = command.get_current_dir() {
            launch.current_dir(directory);
        }
        launch
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(Stdio::piped());
        // SAFETY: only async-signal-safe session/terminal syscalls run before exec.
        unsafe {
            launch.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Self {
            child: launch.spawn().unwrap(),
            master,
            slave,
            output: Vec::new(),
        }
    }
    fn drain(&mut self) {
        let mut bytes = [0; 8192];
        while let Ok(count) = self.master.read(&mut bytes) {
            if count == 0 {
                break;
            }
            if self.output.len() < 256 * 1024 {
                self.output.extend_from_slice(&bytes[..count]);
            }
        }
    }
    fn resize(&self, width: u16, height: u16) {
        let window = libc::winsize {
            ws_row: height,
            ws_col: width,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: the retained terminal fd and winsize pointer are valid.
        assert_eq!(
            unsafe { libc::ioctl(self.slave.as_raw_fd(), libc::TIOCSWINSZ as _, &window) },
            0
        );
    }
    fn send(&mut self, text: &[u8]) {
        self.master.write_all(text).unwrap();
    }
    fn wait_for(&mut self, fixture: &Fixture, id: &str, predicate: impl Fn(&[Value]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            self.drain();
            let records: Vec<Value> = fs::read_to_string(
                fixture
                    .store
                    .join("nano/sessions")
                    .join(id)
                    .join("events.jsonl"),
            )
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
            if predicate(&records) {
                return;
            }
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "Nano exited before expected records: {}",
                String::from_utf8_lossy(&self.output)
            );
            assert!(
                Instant::now() < deadline,
                "Timed out waiting for records: {records:?}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
    fn finish(self) {
        self.finish_status(true);
    }
    fn finish_status(mut self, success: bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            self.drain();
            if let Some(status) = self.child.try_wait().unwrap() {
                assert_eq!(status.success(), success, "{status:?}");
                break;
            }
            assert!(Instant::now() < deadline, "Nano did not stop");
            thread::sleep(Duration::from_millis(20));
        }
        self.drain();
        let display = String::from_utf8_lossy(&self.output);
        assert!(display.contains("\x1b[?1049h"));
        assert!(display.contains("\x1b[?1049l"));
        assert!(display.contains("\x1b[?2004l"));
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn waiting_after(records: &[Value], turns: usize) -> bool {
    records
        .iter()
        .filter(|record| record["event"]["event"] == "model_completed")
        .count()
        == turns
        && records
            .last()
            .is_some_and(|record| record["event"]["event"] == "input_requested")
}

#[test]
fn terminal_conversation_steers_runs_tools_cancels_and_resumes_without_registration() {
    let fixture = Fixture::new();
    fs::write(fixture.root.join("lib.rs"), "pub fn example() {}\n").unwrap();
    let provider = Provider::new(vec![
        calls(&[("read_file", json!({"path":"lib.rs"}))]),
        answer("First answer"),
        answer("Second answer"),
        answer("Resumed answer"),
    ]);
    fixture.configure(&provider.url, "");
    let mut terminal = Terminal::start(fixture.command("terminal").args([
        "--interactive",
        "--prompt",
        "Read lib.rs",
    ]));
    terminal.wait_for(&fixture, "terminal", |records| waiting_after(records, 2));
    terminal.resize(40, 12);
    // Paste is one editable multiline draft; Enter sends it as one request.
    terminal.send(b"\x1b[200~Explain the result\nwith detail\x1b[201~\r");
    terminal.wait_for(&fixture, "terminal", |records| waiting_after(records, 3));
    terminal.send(b"\x03");
    terminal.finish();
    assert_eq!(provider.requests.lock().unwrap().len(), 3);
    let records = fixture.records("terminal");
    let inputs: Vec<_> = records
        .iter()
        .filter(|record| record["event"]["event"] == "user_input")
        .collect();
    assert_eq!(inputs.len(), 2);
    assert_eq!(
        inputs[1]["event"]["text"],
        "Explain the result\nwith detail"
    );
    assert_eq!(
        records.last().unwrap()["event"]["reason"]["reason"],
        "cancelled"
    );
    let before = fs::read(fixture.store.join("nano/sessions/terminal/events.jsonl")).unwrap();

    let mut terminal = Terminal::start(fixture.command("terminal-resume").args([
        "--interactive",
        "--resume",
        "terminal",
    ]));
    terminal.wait_for(&fixture, "terminal-resume", |records| {
        waiting_after(records, 0)
    });
    // Resumed context is visible before the first new request.
    terminal.drain();
    assert!(String::from_utf8_lossy(&terminal.output).contains("Second answer"));
    terminal.send(b"Continue\r");
    terminal.wait_for(&fixture, "terminal-resume", |records| {
        waiting_after(records, 1)
    });
    terminal.send(b"/quit\r");
    terminal.finish();
    let requests = provider.finish();
    let request = serde_json::to_string(&requests[3]).unwrap();
    assert!(request.contains("Second answer"));
    assert!(request.contains("Continue"));
    assert_eq!(
        before,
        fs::read(fixture.store.join("nano/sessions/terminal/events.jsonl")).unwrap()
    );
    fixture.assert_unregistered();
}

#[test]
fn terminal_cancellation_joins_owned_writers_before_exit() {
    let fixture = Fixture::new();
    let provider = Provider::new(vec![
        calls(&[(
            "exec",
            json!({"command":"sleep 60", "cwd":".", "timeout_ms":60000}),
        )]),
        answer("Command is running"),
    ]);
    fixture.configure(&provider.url, "");
    let mut terminal = Terminal::start(fixture.command("cancel-writer").args([
        "--interactive",
        "--prompt",
        "Start a command",
    ]));
    terminal.wait_for(&fixture, "cancel-writer", |records| {
        waiting_after(records, 2)
    });
    terminal.send(b"\x03");
    terminal.finish();
    provider.finish();
    let records = fixture.records("cancel-writer");
    let process = records
        .iter()
        .find(|record| record["event"]["event"] == "tool_result")
        .unwrap()["event"]["outcome"]["content"]["process_id"]
        .as_str()
        .unwrap();
    let state: Value = serde_json::from_slice(
        &fs::read(
            fixture
                .store
                .join("nano/sessions/cancel-writer/commands")
                .join(format!("{process}.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(state["completion"]["state"], "cancelled");
    assert_eq!(state["output_complete"], true);
    fixture.assert_unregistered();
}

#[test]
fn terminal_provider_failure_restores_modes_and_returns_nonzero() {
    let fixture = Fixture::new();
    let provider = Provider::new(vec![json!({"choices":[]})]);
    fixture.configure(&provider.url, "");
    let mut terminal = Terminal::start(fixture.command("provider-error").args([
        "--interactive",
        "--prompt",
        "Inspect workspace",
    ]));
    terminal.wait_for(&fixture, "provider-error", |records| {
        records
            .last()
            .is_some_and(|record| record["event"]["event"] == "ended")
    });
    terminal.send(b"/quit\r");
    terminal.finish_status(false);
    provider.finish();
    fixture.assert_unregistered();
}
