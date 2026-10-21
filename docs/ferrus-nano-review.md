# Native Nano review and consultation

With `nano-openai`, `ferrus register --supervisor nano` selects Nano for HQ review and
consultation as well as [taskless authoring](ferrus-nano-supervisor.md). Both roles use
the same private provider settings and optional `--supervisor-model` override. HQ `/review`
and normal scheduling use the existing task-scoped Supervisor launch and JSONL protocol.
SQLite selects the profile from the exact bound task, run, agent, and workspace; launch
prompt text cannot change it.

| Profile | Source and graph | Lifecycle tools |
| --- | --- | --- |
| Reviewer | Submitted Git tree and the run's frozen graph view | `review_pending`, `approve`, `reject` |
| Consultant | Attached Executor workspace and mutable task view | `consultation_context`, `respond_consult` |

Both profiles expose native file/search, scoped instructions, repository/memory reads,
`read_artifact`, and `ask_human`. Taskless definition/archive tools, code edits, shell,
Executor submit/check, and external MCP peers are unavailable. The Executor cannot approve
itself. Review decisions require reading `review_pending`; consultation responses require
`consultation_context`. Large context fields report truncation and can be paged through
`read_artifact`, whose names select only the bound scoped artifacts, never arbitrary paths.
Individual artifacts are limited to 1 MiB and pages to 4 KiB.

The Reviewer reads a private inspection workspace built from submitted Git blobs without
checkout filters. Subsequent canonical/worktree edits cannot alter those reads. Inspection
is bounded to 20,000 entries, 8 MiB per blob, and 128 MiB total. Omitted symlinks/gitlinks and
host-incompatible paths are reported separately in context metadata.
Reviewer startup reclaims inspection directories without a live Supervisor run.
Missing frozen submission evidence or an invalid binding fails
before inference, with no canonical fallback. Arrange a successful graph freeze at submit;
a graph-disabled or failed freeze does not provide this evidence. This requires a submitted Git tree;
non-Git managed review has no equivalent immutable source and fails explicitly.

Only `approve` integrates code, using the existing canonical lock, checks, merge/rollback,
source observation, and graph refresh. Rejection retains review-cycle limits, retry/dispatch
resets, and the mutable successor without rewriting the old frozen run. Successful decisions
stop the engine before later calls or another inference.
Failed approval rollbacks stop with an unknown effect requiring manual reconciliation.

Reviewer leases renew independently of inference. Consultants attach exclusively to the exact
run and never claim or renew the Executor lease. A human question pauses the existing task
with its requester; the host waits without inference and restores the original review or
consultation phase. HQ can relaunch an answered waiter using that same native binding.

Owned lifecycle operations are joined on cancellation. A restart does not replay an unknown
approve/reject/response effect: durable journals are matched to the role and submission or
consultation phase and ambiguous effects require manual reconciliation. Permanent provider
or budget stops and sessions that finish without a decision fail the still-active task with
a diagnostic; paused tasks and already committed decisions are preserved. A recorded consultation
response remains available to the existing Executor wait path. Relaunch retains the same phase's
token/turn/tool budgets and durably delivered human answer; it does not replay an unfinished write.

Automated tests use scripted inference and temporary projects. Live model and cross-platform
workflow validation remain separate from local test results.
