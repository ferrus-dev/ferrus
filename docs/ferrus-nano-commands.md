# Nano command sessions

Status: implemented for #77. These are native tool adapters, not a new CLI entry point.
Lifecycle/check/submit integration is implemented in #79; [HQ launch](ferrus-nano-launch.md)
is wired in #80.

`CodingTools` composes the existing workspace tools with `Commands`. The host binds a
workspace, session ID, private journal directory, execution backend, and limits before
advertising tools. Model arguments cannot choose the backend, environment, artifact
directory, runtime identity, or a host PID.

## Tools

| Tool | Arguments | Result |
| --- | --- | --- |
| `exec` | `command`, workspace-relative `cwd`, `timeout_ms` | Running process ID and separate stdout/stderr handles |
| `read_process` | `process_id`, optional `wait_ms` | Current state and available output byte counts; wait is capped at one second |
| `stop_process` | `process_id` | Cancels the owned tree, then briefly waits; poll if it is still running |
| `read_output` | `handle`, optional byte `offset` and `max_bytes` | Bounded excerpt, next byte offset, available bytes, and capture completeness |

For example, `exec` accepts:

```json
{"command":"rg --files src","cwd":".","timeout_ms":10000}
```

Commands start without waiting for exit. Supervisors drain stdout/stderr independently
of the model loop. Stdin is EOF, both output streams are piped, and no command inherits
Nano's protocol stdout. Short process waits yield to heartbeat/control work. Output is
untrusted data; its contents do not authorize additional tools or lifecycle actions.

Completion distinguishes `running`, `exited` (exit code and success flag), `timed_out`,
`cancelled`, `output_limit`, and `unknown`. A signal exit can have no numeric exit code.
An unknown outcome must be reconciled, not automatically re-executed. Starting a command
invalidates the composed workspace generation, and its mutation scope remains `unknown`.
The host can query potentially active writers, including unknown outcomes, before a
check/freeze boundary. #79 must enforce that gate when wiring managed lifecycle tools.

Managed build/test validation must use Ferrus `check`. Successful arbitrary shell
execution does not issue a check receipt, consume a check retry, or submit a task.
The model-facing command policy reserves staging, commits, reset, worktree management,
and integration for Ferrus. Arbitrary shell syntax is not parsed into a security policy.

## Storage and bounds

Artifacts are exclusive, owner-only files under the bound journal's `commands/` directory.
Each command has stdout, stderr, and a small state file. The initial state is persisted
as unknown before spawn. Successful cleanup flushes the output and persists terminal
state before publishing that state to readers. Storage errors or lost supervision leave
an unknown result. A crash can leave an unfinished/invalid state file; it is never evidence
that a process completed successfully.

Default command limits:

- Four concurrent commands and 64 lifetime attempts, including failed setup attempts.
- Ten minutes per command; requests may shorten this but cannot extend the host cap.
- Four MiB of combined stdout/stderr per command and 32 MiB for the command store.
- Two KiB reserved per attempt for state, charged to the same command-store allowance.
- At most three files per attempt. Output reads retain at most two KiB of raw source bytes.
- Up to two seconds of supervisor cleanup, with a bounded total shutdown grace period.

Output reservations are shared across concurrent commands and made before disk writes.
Quota exhaustion stops the tree and preserves the captured prefix with an explicit
incomplete result. Output spooling uses async file I/O and fixed-size pipe buffers.
The check runner's unlimited full-log spools are intentionally not used for this store.
Journal quotas and command-store quotas are independent; a host must budget for both.

Handles resolve only through the current session's registry and retained file objects;
`read_output` never opens a model-supplied path. Cursors are raw byte offsets, including
for binary or invalid UTF-8 output. Display uses lossy UTF-8 conversion; a page boundary
can split a code point. JSON expansion remains bounded separately from raw bytes.
`complete` means that capture finished and the page reached its end, not that the command
succeeded. A quota-limited capture remains truncated even at its last available byte.

This slice does not reopen command stores. Reopening an existing store fails closed;
#84 will reconcile saved output and interrupted effects. Commands are never resumed by
blind re-execution, and persisted data never supplies a PID to stop or attach.

## Trusted-local execution

The only shipped backend is `trusted_local`. Initial cwd validation reuses the native
no-follow workspace traversal. The shell can subsequently access the host: a worktree,
cwd check, environment filter, and command instructions are not an OS sandbox.

The host clears the inherited environment and copies only execution necessities:
PATH, HOME/USERPROFILE, Windows system/shell paths, temporary-directory paths, LANG,
and LC_ALL. TERM is `dumb` and NO_COLOR is enabled. Provider keys, MCP secrets, shell
startup hooks, Git overrides, and Ferrus launch authority are not copied. This prevents
credential inheritance; it does not prevent trusted-local code from reading host files.

Unix reuses Ferrus process-group setup and observes leader exit without reaping it.
Cleanup kills the group before releasing the leader PID. Windows starts the shell
suspended, assigns Ferrus's kill-on-close Job Object, and only then resumes it; cmd AutoRun
is disabled. Natural leader exit also triggers descendant cleanup. Explicit cancellation,
manager drop, and engine termination stop owned processes. Unconfirmed cleanup changes
the engine's terminal reason to `effect_unknown`.

Deliberate Unix session/group escape is outside this backend's containment guarantees.
OS-enforced sandboxing, interactive PTYs, external MCP processes, and live process resume
remain separate slices. The `ExecutionBackend` boundary allows a future enforced backend
without changing the model tools or session protocol.
