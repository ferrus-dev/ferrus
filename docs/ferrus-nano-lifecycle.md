# Nano managed Executor lifecycle

Status: implemented for #79. CLI/HQ launch and structured UI delivery remain in #80;
live crash recovery remains in #84. This module is exercised with scripted providers and
temporary runtime projects. It does not enable a new CLI command or register an agent.

## Host and authority

`managed::run` receives a bound `FerrusSession`, session identity, provider, native tools,
limits, and journal. It validates matching project/task/run authority, claims or attaches to
the prepared work phase before inference, and loads the current task instructions. Model
arguments cannot select a database, project, task, run, agent, workspace, or lease owner.

A separate Tokio task renews the caller-owned lease during inference, command execution,
checks, and native waits. Renewal runs at the configured heartbeat interval, capped at one
third of the lease TTL. A missing, expired, or foreign lease cancels the engine and its owned
processes. The host revalidates authority before each tool; lifecycle mutations validate the
exact run and live owner again inside the SQLite transaction. They never reclaim a lost lease.

The engine remains independent of the provider and frontend. Existing MCP handlers retain
their entry points. Nano calls shared transaction helpers directly, with explicit registered
paths, and does not call a local MCP server or parse MCP result strings.

## Tools and task states

| Tool | Native behavior |
| --- | --- |
| `check` | Quiesce owned writers, run configured checks in the bound workspace, account for retries, refresh the task overlay best-effort, and return bounded feedback |
| `submit` | Quiesce writers, run `/check` and then the existing final review gate, persist scoped submission artifacts and the Reviewing transition |
| `consult` | Validate the existing consultation template, pause the task, immediately poll for the Supervisor response, and return its actual text |
| `ask_human` | Pause with requester ownership, immediately poll for the human answer, and return its actual text |

Claim, heartbeat, and response polling are host operations and consume no model turns. Polls
run every 250 ms, bounded by cancellation and the session elapsed budget. Successful response
consumption restores the previous task state once. A cancelled wait leaves the task paused;
it does not fabricate a reply or advance the task.

Questions and submission notes are limited to 16 KiB. Replies are read through the no-follow
runtime artifact boundary with a 16 KiB input cap and a 24 KiB encoded delivery cap. Oversized
replies remain unconsumed. Approval, task creation, reset, and Supervisor tools are not exposed.

## Checks and submit

Checks use the same configured commands, retry limits, failure metadata, log formatting,
and task transitions as Ferrus. Native check processes use the trusted-local command backend
and its restricted child environment. This is process ownership, not an OS sandbox. Each
check stream is capped at 8 MiB; exceeding the cap interrupts the check rather than reporting
success. Normal failures retain a full scoped log under `.ferrus/logs/`; model feedback is
limited to 4,096 characters with a separate log path. Successful logs are removed.

Before either managed check or submit, Nano stops and joins its command writers. A failed
check can be followed by further coding commands. Cancellation or authority loss stops and
reaps check processes before a terminal session result is recorded.

Successful overlay refreshes run graph recovery and retention best-effort against the bound
project's sidecar and runtime references. Maintenance protects active task/run snapshots and
does not change the check result or task lifecycle on failure.

Submit preserves both required gates and the existing retry accounting. It compares source
identities before, between, and after the gates, including at the SQLite handoff. Git workspaces
use captured Git trees without changing the real index. Non-Git workspaces use a bounded
no-follow fingerprint of source files, excluding `.ferrus/` and `.git/`, capped at 10,000 entries
and 64 MiB. Unsupported or over-budget source inspection fails closed. These checks detect
observed concurrent changes; trusted-local execution does not lock out arbitrary external
writers after the final source observation.

When graph freezing succeeds, the exact snapshot and submitted tree are persisted with
Reviewing and the submitted tree remains pinned. Graph failure is diagnostic-only and cannot
turn an otherwise passing submission into a failed task. Abandoned pins are released. Isolated
workspaces produce a baseline-relative patch from the checked source tree. Ferrus retains
ownership of staging, integration, review, and approval.

The host owns an in-flight lifecycle operation until it settles. If cancellation races a
committed submit, the host joins the operation, records its actual result, and stops without
repeating the handoff or changing the frozen view. Replay accepts a `submitted` end only after
a successful native submit result. This is in-process reconciliation; restart recovery is not
part of this change.

## Session outcomes

Session outcomes do not add task states:

- `submitted`: the native operation confirmed the SQLite Reviewing handoff;
- `task_failed`: the existing task state became Failed, for example after retry exhaustion;
- `paused`: consultation or a human question remains pending;
- `authority_lost`: the bound run or caller-owned lease is no longer valid;
- `model_finished`: inference ended normally, but the task remains incomplete;
- existing cancellation, budget, provider, journal, and unknown-effect outcomes remain available.

A final model sentence never marks a task Complete. Only the Supervisor approval workflow does.
