# Nano conversation in Ferrus HQ

Status: #86 adds native HQ conversations for direct workspace sessions and managed Executors.
Both use the same provider, engine, journal, workspace, and native tools. Standalone delivery
and Supervisor, Reviewer, and Consultant profiles remain separate work.

## Open a conversation

Configure Nano as the Executor as described in [Nano launch](ferrus-nano-launch.md), then open HQ.

- `/executor` opens a direct conversation in the canonical project workspace, without requiring
  or claiming a queued task. It waits for your first message before calling the model. Repeating
  the command reconnects to the same live direct session; managed Executors continue headlessly.
- `/attach executor:nano:1` reconnects to the direct session, including its persisted history.
- `/attach executor:nano:<task-id>` selects that task's active run, or displays its latest
  persisted Executor run when no process remains.
- Type plain text to queue steering for the next model turn. Nano commits the input after the
  current inference and its complete tool-call/result group, before another inference starts.
  It never interrupts a patch or inserts an orphan tool result into provider history.
  HQ keeps the submitted text until its input ID appears in the durable journal, and allows
  one unconfirmed steering request at a time. Queue failures preserve the request for retry.
- A final textual response waits for input. Failure, cancellation, and configured session limits
  end the attempt; managed `submit` ends a task-bound attempt normally.
- PageUp/PageDown scroll the bounded conversation history. Input editing, multiline paste,
  terminal resize, completion, and other HQ commands use the existing HQ implementation.
- `/detach` returns to the dashboard without stopping the Executor or changing task state.
- `/cancel` cancels the selected attempt, stops owned writers, and journals its terminal outcome.
  For a managed conversation, HQ pauses automatic dispatch for that task, including answered
  consultations and human questions. Use `/resume` to resume paused managed work.
  Answering the task's human question also permits its Executor to resume.
  Starting another HQ does not retain this frontend pause; task status, leases, dispatch counts,
  and recovery remain authoritative.

Direct sessions expose native file, patch, command, instruction, repository graph, project memory,
and configured external MCP tools. Their `check` runs configured workspace checks without changing
task retry counters. They read canonical graph context and refresh it best-effort after mutations;
they do not use a task overlay. They ask questions in ordinary responses and do not expose managed
`submit`, `consult`, or `ask_human`. `/cancel` stops the direct session; the next `/executor` opens
a fresh one. No task dispatch budget or lease is consumed.
MCP peers with `inherit_managed_binding = true` are omitted in direct sessions; native repository
tools continue to use canonical context. Other configured MCP peers remain available.
Without Git, direct and managed Executors share one workspace slot. In Git projects, direct
mutations and checks share the canonical approval lock with integration. A running command keeps
that lock until its writer stops; an idle direct conversation does not block approval.

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
A missing journal is shown as unavailable once the persisted run is terminal, rather than
waiting indefinitely for records that will not arrive.

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
HQ supplies an optional `input_id` in steering frames and the journal records that ID on acceptance.
Identified queue rejections survive progress/preview coalescing. Frames and old journals without
input IDs remain readable.
Cancellation bypasses queued steering. Stdout remains structured protocol; stderr remains diagnostics.
HQ uses `ferrus nano run --interactive --taskless` for direct conversations. Task-bound launches
use `--interactive` alone. Both use the private HQ protocol, not a standalone terminal UI.

## Validation

Offline tests cover inference-time steering, intact tool groups, input commit failure, journal
reconnect and incomplete tails, bounded long output, blocked input pipes, stream coalescing,
resize/scroll/prompt geometry, scoped question input, detach, cancellation/dispatch suppression,
and real processes submitting managed work or editing/checking a direct workspace through a mock
OpenAI-compatible HTTP provider. Direct-session tests verify that pending tasks remain unclaimed.
Interactive attempts are excluded from the headless comparative evaluation reporter, including
runs activated by attaching HQ. Live LM Studio checks remain opt-in. Local validation does not
replace Windows runtime CI.
