# Nano conversation in Ferrus HQ

Status: #86 adds a native interactive frontend for the managed Executor. It uses the same
provider, engine, journal, workspace, tools, and lifecycle as headless Nano. Standalone delivery
and Supervisor, Reviewer, and Consultant profiles remain separate work.

## Open a conversation

Configure Nano as the Executor as described in [Nano launch](ferrus-nano-launch.md), then open HQ.

- `/executor` connects to the running Nano Executor. With multiple running Executors, HQ asks
  which conversation to open. With no running Executor, it starts a ready managed task within
  the normal parallelism and dispatch limits.
- `/attach executor:nano:<task-id>` selects that task's active run, or displays its latest
  persisted Executor run when no process remains.
- Type plain text to queue steering for the next model turn. Nano commits the input after the
  current inference and its complete tool-call/result group, before another inference starts.
  It never interrupts a patch or inserts an orphan tool result into provider history.
- A final textual response waits for input in interactive mode. `submit`, failure, cancellation,
  and configured session limits still end the managed attempt normally.
- PageUp/PageDown scroll the bounded conversation history. Input editing, multiline paste,
  terminal resize, completion, and other HQ commands use the existing HQ implementation.
- `/detach` returns to the dashboard without stopping the Executor or changing task state.
- `/cancel` cancels the selected attempt, stops owned writers, and journals its terminal outcome.
  HQ pauses automatic Executor dispatch for that task, including answered consultations and
  human questions. Use `/executor` to resume ready work or `/resume` to resume all paused work.
  Answering the task's human question also permits its Executor to resume.
  Starting another HQ does not retain this frontend pause; task status, leases, dispatch counts,
  and recovery remain authoritative.

Attaching a live headless Nano enables interaction through its existing command pipe, at a safe
engine boundary. The process is not replaced and no effects are replayed. Its final textual
response waits only after the activation has been accepted. A completed run is read-only.
Steering and cancellation bind to a run ID; HQ rejects input when the selected run has changed.

## Questions and authority

`ask_human` displays the existing scoped question panel. Plain text typed for that question is
recorded as its answer, ahead of ordinary steering. The task ID captured when typing begins
prevents a newly arriving question from stealing an answer. Consultation still follows the
managed Supervisor workflow. Nano currently has no separate tool-approval workflow; opening a
conversation does not expand its trusted-local workspace authority or expose review/archive tools.

The managed host continues to own task claims, heartbeat, worktrees, baseline bindings, checks,
submission, recovery, and dispatch accounting. The frontend cannot complete a task by rendering
an assistant message. Only the normal checked submission path produces a Reviewing handoff.

## Display and transport bounds

The private append-only journal is the durable conversation. HQ reads only complete validated
records, incrementally, without acquiring its writer lock or repairing partial tails. Reconnecting
reconstructs display state from those records. Dropping the view stops its reader, not persistence.

The display retains at most 128 entries and 32 KiB of text, with 2 KiB per entry. Tool arguments,
results, status, token usage, and completed assistant responses come from typed journal events.
Long results are marked as display-truncated; full records and command spools remain subject to
normal journal/output quotas. Terminal control characters are sanitized before rendering.

Streaming text is a temporary 512-byte preview associated with its model turn. It is coalesced
and replaced by the completed durable response; it never becomes replay or recovery evidence.
The frontend polls coalesced snapshots at most once per 100 ms. Its rendering speed cannot delay
journal commits, provider/tool effects, or heartbeat renewal.

The existing v1 JSONL protocol adds `interact` and `steer` commands and text-preview events.
Frames remain limited to 4096 bytes, including JSON encoding and the terminating newline.
Both the pipe writer and the engine input queue hold at most eight commands. Full or disconnected
queues report an error; queued input is authoritative only once `UserInput` has been committed.
Cancellation bypasses queued steering. Stdout remains structured protocol; stderr remains diagnostics.
`ferrus nano run --interactive` is the managed launch mode used by HQ, not a standalone terminal UI.

## Validation

Offline tests cover inference-time steering, intact tool groups, input commit failure, journal
reconnect and incomplete tails, bounded long output, blocked input pipes, stream coalescing,
resize/scroll/prompt geometry, scoped question input, detach, cancellation/dispatch suppression,
and a real managed process submitting through a mock OpenAI-compatible HTTP provider.
Interactive attempts are excluded from the headless comparative evaluation reporter, including
runs activated by attaching HQ. Live LM Studio checks remain opt-in. Local validation does not
replace Windows runtime CI.
