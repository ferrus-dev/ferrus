# Nano Session Engine and Journal

Status: implemented foundation for #74. #75 adds an opt-in
[Chat Completions provider](ferrus-nano-provider.md); #80 adds
[CLI/HQ launch](ferrus-nano-launch.md). Automatic effect resume remains later work.
See the [architecture and PR index](ferrus-nano-architecture.md).

## Boundaries

`src/nano/engine.rs` owns one sequential attempt. It accepts `SessionCommand::Start` or `Cancel`,
uses a clonable cancellation handle during an active attempt, and returns a typed `SessionEnd`.
A final model message ends the attempt with `ModelFinished`; only existing Ferrus operations can
change task/run lifecycle. One engine cannot be started twice.

The engine imports transport-neutral provider, tool, host, and journal interfaces:

- `Provider` starts a request and emits fragments followed by one completed response. Fragments
  never execute tools. Adapters own wire parsing and bounded response assembly; completed responses
  preserve provider call IDs and opaque continuation data, including signed blocks. Truncated
  model responses and duplicate call IDs end the attempt before any tool executes.
- `Tools` describes the enabled catalog, validates parsed argument objects against each tool's
  schema, and executes validated calls sequentially. Unknown tools and malformed arguments produce
  typed feedback. The catalog is captured once per model turn. Before the terminal record,
  `shutdown` joins/stops owned effects; unconfirmed cleanup changes the reason to `EffectUnknown`.
- `Host` revalidates authority immediately before an effect and receives committed records.
  Managed Ferrus binding remains in `nano/ferrus.rs`; the engine imports no HQ, MCP, or project types.
- `Journal` commits versioned records and complete-group checkpoints. The file implementation is
  independent of orchestration, repository graph, and project memory databases.

Tool adapters must clean up owned processes when execution is interrupted. Dropping an effect
future does not prove rollback: cancellation or deadline during execution records `Unknown` and
ends the attempt. No subsequent effect runs. Native critical sections and later resume adapters
must reconcile the outcome against the effect's authority; #79 and #84 provide that integration.
The #77 [command adapter](ferrus-nano-commands.md) implements bounded background supervision
and cleanup without blocking provider or host control work.

## Durable order

```text
Started -> ModelStarted -> ModelCompleted
  -> ToolIntent -> ToolResult
  -> ... remaining calls in model order ...
  -> checkpoint -> next model turn or Ended
```

`ModelFailed` terminates a failed model attempt before a bounded retry. Unknown/malformed calls
also receive an intent/result pair, but never reach execution. The journal flushes intent before
calling the tool, and flushes the result before notifying the host. A write, quota, or checkpoint
failure stops the engine. If an effect ran but its result could not be committed, the previous
intent remains pending and no completion notification is emitted. `SessionEnd.durable = false`
means the returned ending itself could not be persisted.

## Budgets

Defaults are initial safety limits, not calibrated performance targets:

| Resource | Default |
| --- | --- |
| Model attempts, including retries | 64 |
| Total reported/estimated/reserved tokens | 200,000 |
| Tool calls, including rejected calls | 256 |
| Provider retries | 3 |
| Consecutive failed or repeated calls / empty model turns | 3 |
| Elapsed time | 30 minutes |
| Serialized context | 256 KiB |
| Completed response and streamed fragments | 64 KiB |
| Tool result | 32 KiB |

Empty streaming fragments consume a minimum byte of the streaming allowance; buffered fragments
also yield so cancellation can run. The host checks context, call, and token budgets before new
work. Provider and tool waits observe cancellation and the attempt deadline. Synchronous durable
storage and adapter cleanup still depend on the underlying OS and implementation; these are not
hard real-time guarantees.

Before contacting a provider, `ModelStarted` records input and output reservations. The local
input estimate uses one token per serialized byte; the request receives an explicit output cap.
A complete response replaces the reservation with reported usage, or a separately marked local
estimate when usage is absent. An interrupted/failed request conservatively charges its entire
reservation as estimated usage, since actual billing is unknown. Counters, reservations,
no-progress count, and elapsed time are persisted with records and checkpoints. Recovery does not
reset budgets or treat estimates as provider billing data.

The active attempt uses a process-local monotonic clock. After a crash, time since the last
committed record is unknown. Recovery conservatively consumes the remaining elapsed allowance
and durably ends an unfinished attempt with `Limit(Elapsed)`, including one at a complete
checkpoint. This charge is a recovery bound, not a measured duration. Pending effects and token
reservations remain available for reconciliation. Continuing work requires a new session;
reopening an already ended session preserves its recorded duration and reason.

## Storage and recovery

The host passes the registered machine-local project data directory:

```text
<project-data>/nano/sessions/<session-id>/
  writer.lock
  events.jsonl
  outputs/<output-id>
  checkpoints/<sequence>.json
```

Session identity contains the project and optional task/run pair. IDs are bounded path-safe
ASCII tokens. Directories and files are owner-only: Unix uses 0700/0600 and verifies effective
ownership; Windows uses a protected owner-rights DACL without inherited grants. Reopen rejects
symlinks, unexpected file types, and broad permissions. The writer holds an OS file lock.

Per-session defaults are 512 KiB per JSON record, 16 MiB journal, 1 MiB per artifact, 32 MiB total
storage, and 256 files. Artifacts and checkpoints share the byte/file quota. Serialization is
bounded while writing into memory. Artifact writes are immutable; this slice performs no automatic
retention/deletion or raw-session ingestion into project memory.

Checkpoints identify a complete journal prefix by sequence, SHA-256 digest, and budget snapshot.
They cannot split a model response's tool-call/result group. Files are flushed before atomic
publication; Unix also syncs directories, and Windows uses write-through rename. Storage durability
ultimately depends on the filesystem. Checkpoints are prefix markers, not compacted conversation
copies; #82 and #84 add compaction and live recovery.

`FileJournal::recover` acquires the writer lock, enforces quotas, validates version/identity/order,
and removes only an unterminated final line. A malformed newline-terminated record is an error.
It returns recorded state, pending intents, and unknown effects without invoking tools or providers.
For an unfinished attempt it appends the elapsed-budget charge and ending before returning; if
that write fails, recovery fails. Repeated recovery preserves the same ending and budget.
`Replay::from_records` is pure and reconstructs messages, ordering, and budget state from recorded
inputs; it has no external effect ports.

An oversized initial command is rejected before its body is journaled. Other limits produce typed
end reasons when the journal remains writable. Session files contain operational context and must
remain outside default project-memory ingestion.
