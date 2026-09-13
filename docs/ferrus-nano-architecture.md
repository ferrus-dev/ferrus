# Ferrus Nano: Native Agent Harness

Status: accepted implementation plan, updated 2026-09-08. The #73 managed-session binding,
#74 bounded engine/journal, and #75 opt-in Chat Completions adapter are implemented. Live model
validation, launcher, and later slices remain planned. No measured performance claim.

Working product name: `ferrus-nano`. Ferrus backend name: `nano`.

Related documents: [session engine and journal](ferrus-nano-sessions.md), [first provider](ferrus-nano-provider.md), [roadmap](milestones.md), [repository graph](repository-graph-architecture.md),
[project memory](project-memory-architecture.md).

## 1. Decision

Build a small Rust coding-agent runtime with native access to existing Ferrus operations and
repository context. Ship the first version as a headless Executor launched by HQ. Keep the agent
engine independent of the terminal, scheduler, MCP transport, and LLM provider so that an HQ
conversation view and a standalone frontend can drive the same engine later.

The first launch path is `ferrus nano run`, using the current Ferrus executable as a child process.
Inside that child, Ferrus operations are ordinary Rust calls. There is no loopback `ferrus serve`
process, MCP client, or JSON-RPC hop for native operations. External integrations use a neva MCP
client through a separate tool adapter.

Keep one package and ordinary Rust modules initially. Do not introduce a plugin SDK, independently
versioned crate family, second orchestrator, or model-routing framework. Nano means a small harness,
not a requirement to use a small model.

Keep new harness code under `src/nano/` and preserve the existing Ferrus layout. Reuse operations
where they live, with small callable helpers where needed. An optional `src/shared/` is reserved
for concrete components used by both HQ and nano; introducing nano does not require a general
application-layer extraction or moving the existing runtime into the library.

Use `ferrus-nano` for a future standalone executable; avoid installing a bare `nano` executable
alongside the existing terminal editor.

## 2. Verified starting point

The initial implementation lives in `src/nano/ferrus.rs`: `LaunchContext` captures trusted HQ
environment/cwd, and `FerrusSession` resolves explicit project registration and a task-scoped
Executor run. Its claim/status/heartbeat methods return existing project types, not MCP text.
The scoped project entry points validate the exact run, role, agent, task, and workspace within
the same SQLite transaction as effects. They never select the latest run or another task by agent.
Status remains read-only, including before claim or after lease loss. Claim preserves pending
promotion and retry semantics; heartbeat cannot renew outside an Executor work phase.

The Git baseline is checked against HQ's existing task baseline metadata independently of optional
graph snapshot IDs. The database is opened without create/migrate behavior: HQ must prepare it and
persist the run binding before the child uses the adapter. The later launcher (#80) must account
for run-start ordering; the current external launcher writes the run after spawning. This slice
adds no runnable backend or automatic launch retry. The nano module's temporary dead-code allowance
can be removed when the launcher makes these entry points reachable.

Audit base: Ferrus commit `4f52783d6f5efa64ea1a2adf48b55c8b27c565be`.

| Area | Available now | Work needed for nano |
| --- | --- | --- |
| Graph and memory domains | Public library modules, bounded queries, immutable identities, source verification | Native context projection and session working set |
| Task graph routing | `LocalGraphContext`, task baselines/overlays, submitted snapshots | Explicit session context instead of ambient environment lookup |
| Lifecycle | SQLite claims, leases, checks, submit, consultation, human questions, review | Expose native helpers in existing modules where needed; retain one implementation |
| Workspace lifecycle | HQ worktrees, baseline preparation, process supervision and recovery | Native launch capabilities and structured output |
| Checks | Ordered commands, log spooling, bounded feedback, final submit gate | Reuse the existing check path, with cancellation support |
| Agent adapters | `ExecutorAgent` and `SupervisorAgent`, model overrides, stdin prompts | Executor-only/headless-only capability declarations; native registration |
| UI | Crossterm HQ, `UiMessage`, transcript and question handling | A session-event adapter and, later, a conversation view |
| MCP | neva 0.5.6 with `server`, `di`, `legacy-spec` | Add client features for external tools, preserve protocol compatibility |
| Agent engine | Sequential bounded core, provider/tool/host interfaces, durable journal, pure replay, and scripted tests | Live model validation, managed tool wiring, compaction, and live resume |
| Instructions | Ferrus role prompts, project guidance, and embedded skill templates | A bounded native instruction loader with explicit precedence and provenance |
| Coding tools | Graph source readers, check runner and process primitives | General bounded reads/search, patch editing, command sessions |

Important source locations:

- `src/lib.rs` exports graph, memory, and distributed modules. Project runtime, HQ, and MCP
  composition currently belong to `src/main.rs`; they are not an existing reusable runtime library.
- `src/server/tools/{wait_for_task,check,submit}.rs` contain lifecycle behavior as well as MCP
  wrappers. `handler_for_agent` still returns serialized text and neva errors.
- `src/server/tools/repository_context.rs` adapts JSON inputs and serializes typed graph responses.
- `src/repository_graph_runtime.rs` owns project configuration, routing, freshness, and verified
  content adaptation. Native access must preserve this boundary.
- `src/hq/agent_manager.rs` launches processes, logs stdout/stderr, and closes stdin after sending
  a headless prompt. This is not yet a bidirectional session protocol.
- `src/agents/mod.rs` defaults version discovery to an interactive command and assumes MCP
  registration. These defaults do not fit an Executor-only native backend unchanged.

The current extractor set covers generic files, Cargo, Rust syntax, and relationship resolution.
It is useful structural evidence, not complete compiler semantics or universal language coverage.

## 3. What should improve, and why

Native invocation removes local transport and serialization work around Ferrus operations. The LLM
still receives tool definitions and emits tool arguments; those tokens do not disappear with MCP.
Remote inference, graph refresh, source reads, and checks may dominate elapsed time.

The stronger hypotheses are:

1. A bounded graph query can replace several exploratory searches and whole-file reads.
2. A session working set can avoid sending the same source ranges and facts repeatedly.
3. Host-owned claims, heartbeat, waiting, and completion gates remove mechanical model turns.
4. Stable tool schemas and context ordering can improve provider prompt-cache reuse.
5. Explicit effects, edit preconditions, and recoverable tool records can reduce avoidable retries.

Determinism means reproducible context selection and control flow for recorded model responses,
tool results, configuration, and source identities. It does not mean identical model output on
repeated live requests. Pin model identifiers where supported and record effective settings.

## 4. Boundaries and dependencies

```text
Ferrus HQ scheduler
  |
  +-- process supervisor --> ferrus nano run
                               |
                          Session engine <--> LLM provider
                               |
                     Tool registry and policy
                       /        |         \
               Ferrus host   Workspace   External MCP
                   |           tools       (neva)
          Existing Ferrus operations
             /             \
       Project runtime   Local graph/memory adapters
             |                    |
         ferrus.db          Existing domain APIs

External agents --> Ferrus MCP wrappers --> same existing operations

Session commands --> Session engine --> Session events
                                       |-- JSONL/headless output
                                       |-- HQ conversation adapter (later)
                                       `-- Standalone frontend (later)
```

Use only a few substitution boundaries: model provider, tool execution, session storage, and
command/event delivery. Context selection and managed Executor behavior are concrete modules,
not independently configurable plugin pipelines.

Existing operation implementations retain role/session guards and lifecycle behavior. Nano's
Ferrus adapter supplies validated session identity and calls those implementations. Native tools
must not manipulate SQLite rows or artifact files directly. Graph and memory domains remain
unaware of nano, LLMs, HQ, and network clients. Session history does not become project memory.

The child-process boundary preserves HQ's existing process ownership, worktree cwd, stop/recovery,
and dispatch accounting. It also isolates remaining legacy uses of process environment and cwd.
Do not embed several nano sessions inside the HQ process while those ambient dependencies remain.

### Reusing existing operations

Connect the operations needed by the Executor incrementally:

- claim/attach and scoped context loading;
- status and lease renewal;
- check and final submit;
- consultation and human-question request/wait;
- graph status/search/context and memory/federated retrieval;
- scoped reads of task, rejection, templates, and other currently authorized resources.

Keep the existing files and ownership of these operations. If a handler combines transport
formatting with lifecycle logic, separate a small `pub(crate)` helper in that same module as the
native caller is added. Use typed outcomes for native callers and retain the MCP formatter around
the helper. Reuse existing project/graph/check APIs directly when they already provide the needed
operation; do not create another forwarding layer for every API.

The nano host keeps an explicit validated session context binding project, agent, role, task, run,
workspace, and baseline. It resolves that context from HQ launch data and SQLite; the model cannot
select another task or root. Pass the identity and inputs required by each operation, using
existing types where possible. Revalidate ownership and lifecycle at effect boundaries, rather
than trusting a startup snapshot. Add explicit-context entry points only where ambient lookup
would otherwise make native execution incorrect; no repository-wide signature rewrite is needed.

MCP wrappers retain their current names, input shapes, textual compatibility, and manual
`app.map_tool()` registration. They translate to and from the same operations nano uses.
Role visibility is also enforced inside the operations, since native invocation bypasses MCP
listing. Do not use `handler_for_agent` as nano's permanent API or copy its behavior into nano.

All Ferrus capabilities should ultimately have a native path. The initial Executor tool set
contains only Executor and shared operations. Supervisor, Reviewer, Consultant, archive, and
compatibility-only tools are exposed only when their corresponding host profile is implemented.

## 5. Engine, providers, and execution policy

The engine owns one session and one active model turn. Start with sequential tool execution.
Later, independently authorized read-only calls may run concurrently against the same pinned
view, with results committed in model-call order. Workspace mutations, checks, submit, and
lifecycle transitions remain serial barriers. External tools are not assumed read-only from
untrusted annotations.

Load a compact native role policy, the current task/rejection context, and applicable project
instructions with their paths and content digests. Preserve Ferrus's existing rule that supporting
`AGENTS.md`, `ROLE.md`, and skill documents cannot override runtime authority or the active task.
Apply nested project guidance by workspace path and load selected skill bodies on demand, under
explicit context limits. Do not load every external agent's configuration or every skill into the
prompt. Keep active constraints through compaction and refresh guidance when its source changes.
This needs an instruction loader, not a plugin/hook framework. The native role prompt describes
host-owned claim/heartbeat/wait behavior instead of asking the model to perform it over MCP.

An ordinary turn is:

```text
accept command -> assemble bounded context -> stream model response
  -> validate completed tool calls -> authorize -> persist intent
  -> execute -> persist outcome -> update working set -> next turn
```

Never execute partial streamed arguments. Reject unknown tools and invalid schemas with bounded,
typed feedback. Stop malformed-call loops, repeated no-progress calls, and provider retries with
explicit budgets. A final model message alone does not complete a managed Ferrus task.

The provider interface covers streaming text and tool calls, terminal reasons, cancellation,
usage, context limits, and provider-specific continuation data. Keep provider wire types inside
adapters; preserve opaque signed/reasoning blocks where the provider requires them. Do not invent
a common format that silently loses tool-call IDs or continuation requirements.

The first adapter targets LM Studio through OpenAI-compatible Chat Completions streaming,
with optional Bearer authentication. See [provider configuration and verification](ferrus-nano-provider.md).
A scripted provider remains available for engine tests. A second adapter is a later compatibility
check; compatible endpoints still require explicit validation of tool calling, streaming, usage,
and context limits with the selected model.

Load credentials through host configuration or environment references. Keep credentials out of
project files, command arguments, transcript events, tool children, and MCP child environments.
Separate provider inference transport from workspace command execution and external MCP access.

Budgets include model turns, total input/output tokens, tool calls, command duration, provider
retries, context bytes, and elapsed time. Charge compaction and retries to the same session budget.
Persist consumption across recovery of that session; HQ's existing task dispatch limit still
bounds fresh attempts. Budget exhaustion ends the agent attempt with a typed reason and uses
existing HQ recovery behavior; nano does not invent new task statuses.

## 6. Native coding tools

Prefer familiar, narrow tools with typed results over a single arbitrary `ferrus_call` dispatcher.
Use one authoritative descriptor per tool, with provider-specific schema encoding at the edge.
Expose only schemas relevant to the session; model tokens still matter for native tools.

| Surface | Initial behavior |
| --- | --- |
| `repository_graph_status`, `repository_search`, `repository_context` | Existing bounded graph semantics, native typed results |
| `project_memory_status`, `project_context_search`, `project_context` | Existing curated memory semantics and explicit domains |
| `read_file` | Bounded line/range reads with path, content digest, and source identity |
| `search_text` | Bounded lexical/path search for literals, unsupported syntax, and missing graph coverage |
| `apply_patch` | Create/update/delete with explicit base-content preconditions and conflict results |
| `exec`, `read_process`, `stop_process` | Bounded noninteractive commands, cancellation, persisted output handles |
| `read_output` | Retrieve a bounded range from an authorized session output artifact |
| `check`, `submit`, `consult`, `ask_human`, `status` | Native Ferrus operations, preserving existing lifecycle behavior |

Claims, heartbeat, and wait operations are native host operations, usually absent from the model's
tool catalog. They remain available to the runtime without spending inference turns.

File tools confine paths to authorized roots, handle symlinks safely, preserve unrelated edits,
and check the expected content before writing. Validate all patch hunks before applying a patch;
report any partial filesystem failure precisely rather than promising a multi-file transaction.
The implemented #76 tool contract is documented in [Nano workspace tools](ferrus-nano-workspace.md).
Keep original/new digests sufficient to reconcile an interrupted edit. Do not silently apply a
stale edit with fuzzy matching. Binary and oversized content return explicit limitations.

Use existing process lifecycle helpers and bounded check-output primitives where appropriate.
General command sessions still need duration limits, process-tree cancellation, output cursors,
and disk quotas. Build/test commands in managed mode continue through Ferrus `check`; a generic
shell success is not a check receipt. Ferrus retains ownership of Git staging, history, and final
integration. Native file tools cannot modify runtime databases or `.ferrus/` lifecycle artifacts.

For the first release, declare a trusted-local command mode explicitly. Root-confined file tools
and an isolated Git worktree are not an OS sandbox for arbitrary shell code. A policy can deny
commands, but it cannot make allowed shell code unable to access the host. Keep an execution
backend boundary for future enforced sandboxing; do not label the initial backend sandboxed.

In headless managed mode, missing human input is routed through existing `ask_human`/answer
handling. The tool layer must not wait on an invisible terminal prompt.

## 7. Repository context as a native working set

This is the distinguishing part of nano. Implement it as a concrete context manager using the
existing graph query and verified-content boundaries.

Maintain three separate representations:

1. An append-only session journal with model messages, tool calls, outcomes, and provenance.
2. A working set of evidence handles, source ranges, edits, check outcomes, and active intent.
3. A bounded model-context projection assembled from those records for the next request.

A working-set entry identifies its repository, task view, immutable snapshot/overlay, optional
memory revision/link set, path or node, source digest/span, inclusion reason, and freshness.
Evidence handles are lookup keys, not replacements for source text needed to make an edit.

### Retrieval flow

1. The managed host obtains task intent and rejection feedback through the normal claim path.
2. Check graph availability once. Resolve explicit task paths/symbols into small bounded queries;
   without usable seeds, let the model choose a search instead of building a speculative repo map.
3. Search exact paths/symbols first, then expand relevant structural neighbors under a hard budget.
4. Include verified source ranges when needed for reasoning or editing. Merge overlapping ranges
   only when their content identity and snapshot match.
5. Keep selected evidence in the working set and reuse it while its validity and context presence
   are known. A result evicted by compaction must be materialized again when requested.
6. Fall back to authorized current-workspace reads and lexical search when the graph is disabled,
   missing, stale for the requested file, ambiguous, or lacks coverage. Label this as workspace
   evidence, not evidence from the old graph snapshot.

Graph results never enter persisted task/review prompts or task artifacts. An optional initial
prefetch is a logged native retrieval operation with a separately identified evidence observation
in the session context. It must not fabricate an assistant tool call to satisfy provider message
formatting. The context projection encodes host observations in the provider's supported format
with their origin intact. Prefetch can be disabled for evaluation and is subject to the same
read-only and budget rules as model-requested retrieval.

Start with deterministic ranking: explicit seeds, requested relationships, stable relevance
scores, and a stable path/ID tie-breaker. No embeddings, hidden planner model, speculative full
graph expansion, or automatic project-memory writes are required. Missing edges remain unknown;
they are not proof of no callers, no dependencies, or safe deletion.

### Freshness and invalidation

Graph queries stay read-only. Workspace mutation tracking schedules refresh through the existing
refresh coordinator outside the retrieval operation, under existing sidecar leases.

- Successful native edits advance a session workspace generation and invalidate affected source
  entries and derived context packets. The returned changed-path set drives a debounced overlay
  refresh after an edit batch, with a finite refresh time budget.
- Shell commands and checks may edit arbitrary files. Treat their mutation scope as unknown unless
  verified, invalidate the relevant workspace observation, and reconcile before trusting it again.
- Background processes keep the workspace potentially mutable while they run. Do not run final
  checks/submit until nano-owned writers are stopped or joined. Capture and compare source identity
  around the check/freeze boundary and report a concurrent-change outcome if it changed.
- Other editors and processes are not fully covered by nano's generation counter. File watchers
  are advisory; retain `Freshness::Unknown` without reliable comparison. Verify bytes at reads and
  patch application. A session counter must never make globally unverified data appear fresh.
- A failed refresh preserves the last publication and reports its age/limitations. Existing check
  and submit refresh/freeze semantics remain the final lifecycle integration points.

Resolve a mutable graph publication once for a context assembly, then pin every query in that
assembly to the immutable target. Cache keys include project/repository, task-view identity,
snapshot/overlay, effective query shape, budgets, source-policy version, and content identities
where relevant. Memory caches additionally bind the exact memory revision and link-set pair.
Do not reuse cursors after a view changes. Invalid task bindings and missing Reviewer freezes
are routing errors, never reasons to fall back to canonical data.

### Context budget and compaction

Compute available input space from the provider's context limit minus reserved output, tool
schemas, instructions, protocol overhead, and a safety margin. Admit bounded evidence only within
that space. Track estimated tokens separately from provider-reported usage.

Prioritize active instructions and task intent, recent edits/check failures, directly selected
source, then optional neighbors and historical context. Keep a stable prefix where possible;
changing the tool catalog or rewriting earlier messages can reduce prompt-cache reuse.

First compact deterministically: remove superseded evidence from the projection, replace large
old outputs with retrievable handles, and retain recent complete call/result groups. If necessary,
summarize older reasoning/intent into a checkpoint with evidence references. Preserve unresolved
questions, current constraints, edit preconditions, and verification gaps. A summary cannot grant
tool authority or overwrite SQLite state. Do not compact a partially executed tool group or
rewrite opaque provider data that must remain intact. The original journal remains available.

## 8. Managed Executor lifecycle

The managed host, not the model, carries out the mechanical workflow:

```text
validate launch -> claim task -> start lease renewal -> run model/tool loop
  |                                   |
  |                                   +-- consult/ask_human -> native wait -> resume
  |
  `-- model requests submit
        -> quiesce writes -> check -> submit's final gate and freeze
        -> confirm SQLite Reviewing handoff -> stop session
```

Claim occurs before the first inference request. Renew only the caller-owned lease on the
configured schedule, independently of provider streams, commands, and native waits. On ownership
loss, stop new effects, cancel running work where possible, and re-resolve runtime state; do not
continue writing under a stale claim.

After `consult` or `ask_human`, invoke the corresponding wait operation immediately. The host
handles bounded polling timeouts without model calls and delivers the actual answer once ready.
Resume/recovery must also recognize an already paused task, reconstruct its pending request, and
wait for that request instead of claiming fresh work or issuing a duplicate question.

The public native `submit` tool is a managed sequence: run `check` immediately before invoking
the existing submit operation, which runs the final gate again. This preserves today's required
workflow. Deduplicating those gates is a separate lifecycle change requiring evidence and tests.
Failed checks return bounded feedback to the model while preserving Ferrus retry accounting.

Stop after a confirmed Reviewing handoff; the Executor never approves its own work. If the model
ends without a handoff, report an incomplete attempt and let existing HQ dispatch/recovery rules
handle it. Do not infer task completion from a model sentence, process exit code, or journal event.
After a lost submit response, query SQLite and reconcile artifacts/pins before attempting again.

When a native operation commits a lifecycle effect, cancellation must reconcile its outcome before
ending the session. A stop request cannot simply forget an in-flight submit transaction.

## 9. Session journal and crash recovery

The #74 implementation and current defaults are documented in [session storage](ferrus-nano-sessions.md).
Recorded replay is implemented; live resume and reconciliation remain #84.

Use one versioned, single-writer JSONL journal per nano session plus bounded output artifacts.
Managed location proposal:

```text
<project-data>/nano/sessions/<session-id>/
  events.jsonl
  outputs/<output-id>
  checkpoints/<checkpoint-id>.json
```

Resolve `<project-data>` from project metadata. Link the nano session to its Ferrus task and run;
do not put provider transcripts into `ferrus.db`, `repo-graph.db`, or `project-memory.db`. Existing
`.ferrus/logs/` can contain the normal human-readable session log and a journal locator.
Session bodies are local operational artifacts excluded from default project-memory ingestion.
Define retention/quotas and owner-only file permissions alongside the journal, not after release.

Persist completed messages and tool intent/outcomes with sequence numbers and stable call IDs.
Flush intent before effects and flush the outcome before acknowledging completion. Token deltas
may be ephemeral UI events; persist the completed message. Recover a truncated final journal
record safely and write checkpoints atomically. A journal failure stops new unrecorded effects.

On resume, validate workspace, source changes, role, run/task binding, and current lease. A new
HQ run may reference a prior session checkpoint as context but does not inherit its authority.
Reconcile completed native lifecycle operations against Ferrus state and deterministic edits
against their before/after digests. An interrupted command or external MCP effect has an unknown
outcome unless the tool provides reliable reconciliation; do not replay it automatically.

Recorded replay tests reproduce engine decisions with scripted model/tool inputs. They do not
re-execute real shell commands, external effects, or provider calls. Live resume is a separate
operation that rechecks current authority and source state.

## 10. External MCP through neva

Keep MCP at the extension boundary. The native Ferrus catalog must not contain a second MCP copy
of the same Ferrus operations.

MVP: explicitly configured stdio servers, connect/list/call/disconnect, namespace isolation,
schema validation, bounded results, timeouts, and cancellation. Use a provider-safe deterministic
tool-name encoding with a reversible mapping to server/tool identity. Pin the enabled catalog
and schema hashes for an active turn. Large catalogs can later use explicit search/enable, rather
than sending every external schema on every turn.

Treat server content and tool descriptions as data. Apply local permissions independently of
server annotations. Keep credentials scoped to the configured server. Advertise only supported
client capabilities: sampling is disabled initially; elicitation is either routed through the
host's human-input path or explicitly unsupported, with no headless terminal prompt.

Ferrus currently enables neva's `legacy-spec` and serves MCP `2025-03-26`. Adding client features
to that dependency retains the legacy build profile. Cargo feature unification means putting
a client in another crate of the same dependency graph does not isolate the protocol generation.
Use that compatible profile for the first stdio integration and test actual supported peers.

Supporting a new protocol generation requires a deliberate migration of Ferrus's existing server
surface or a separately built bridge process. Do not silently remove `legacy-spec` while adding
nano. Streamable HTTP, OAuth, resources/prompts, and richer external interactions can follow
without changing the engine/tool boundary. neva manages protocol details, not model inference.

## 11. HQ and standalone evolution

Introduce explicit adapter capabilities covering role, interaction mode, Ferrus integration
(`native` or `mcp`), and output format. For nano v1:

```text
executor/headless: supported
supervisor/reviewer/consultant: unsupported
interactive: unsupported
Ferrus integration: native
output: versioned session events
```

Reject unsupported modes before worktree/process setup. Override version discovery explicitly.
`ferrus register --executor nano` validates native configuration and selects the backend without
writing self-referential MCP configuration. Keep external adapters' current behavior as defaults.
The requested Executor-only rollout is intentional; do not add a fake Supervisor implementation.

Define `SessionCommand` and `SessionEvent` independently of `UiMessage`:

- Commands: start, cancel, resume, and later user input/steering/approval response.
- Events: session/turn started, text delta, tool started/completed, context selected, usage,
  check outcome, input required, error, and session ended.
- Every durable event has a version, sequence, session identity, and optional task/run/call IDs.

Add a native launch/output mode alongside the current raw-output/stdin-prompt modes. In that mode,
stdin carries bounded JSONL commands and remains open; stdout carries JSONL events; stderr carries
diagnostics. Child commands and MCP servers never write to the protocol stdout. Debug mode must
preserve the same protocol. Unknown versions and oversized frames produce explicit errors.

The HQ adapter translates events into its current transcript, status, and question presentation;
it does not parse prose to infer checks, completion, or costs. Start with coarse session/tool
events. Coalesce token deltas and bound UI queues so a slow renderer cannot block lease renewal
or durable tool outcomes. Recovery uses the journal, not the display stream.

Interactive phase: add an HQ conversation view and command producer, with one active inference
turn per session and a defined steering queue. Adapt the current HQ transcript/input/rendering in
place first. Move a component into `shared/` only when both frontends need that implementation.
The current HQ renderer is not an existing reusable chat widget. Test geometry, scrolling,
cancellation, and long tool output.

Standalone phase: compose the same engine with a `StandaloneHost` supplying an explicit workspace,
local tools, configuration, session storage, and optional local graph/memory. It must run without
an HQ daemon, selected spec, task claim, or `ferrus.db`. Reuse graph domains with explicit local
scope; do not call today's `LocalGraphContext::load_for_agent` in an unregistered directory.
Standalone completion is a session result, while managed completion remains Ferrus submit.

A future `ferrus-nano` binary can reuse the nano engine and selected frontend components without
spawning a `ferrus serve` process. Decide library exports when adding that binary; the initial
headless integration does not require them. Supervisor support is another host capability/profile
with its own planning, review, consultation, and archive policies; adding a UI alone does not
grant those operations.

## 12. Initial code organization

All new harness implementation lives under one directory. This is a possible internal split,
not a requirement to create every file before the first working slice:

```text
src/
  nano/
    mod.rs
    cli.rs                 # Headless entry point and JSONL command/event delivery
    agent.rs               # ExecutorAgent implementation used by HQ
    engine.rs              # Bounded turn loop and commands/events
    ferrus.rs              # Managed host and native calls to existing Ferrus operations
    config.rs              # Nano settings and provider selection
    provider.rs            # Provider contract and streaming normalization
    providers/             # First provider plus scripted test provider
    context.rs             # Working set, projection, invalidation, compaction
    instructions.rs        # Scoped guidance and selected skill loading
    tools.rs               # Descriptors, authorization and tool execution contract
    workspace.rs           # Local file, patch, process and output tools
    session.rs             # Journal, checkpoints, recovery
    mcp.rs                 # Optional neva client adapter
  shared/                  # Optional: concrete components shared by HQ and nano
```

Start with `mod nano` in the existing binary, optionally feature-gated. The engine depends on its
provider/tool/host contracts; `nano/ferrus.rs` binds them to the current project, graph, checks,
and tool modules. Workspace adapters can reuse `platform` and output helpers in place. Keep
Ferrus-specific imports out of the engine itself so later standalone composition needs another
host, not a rewrite of the loop. Public library exports and crate splitting can wait for a real
standalone consumer.

Limit changes outside `nano/` to the integration points actually required:

- `src/main.rs`: declare the nano module.
- `src/cli/mod.rs`: add command routing to `nano::cli`.
- `src/agents/mod.rs` and existing registration/config code: select `nano::agent`, validate
  capabilities, and skip self-MCP registration for the native backend.
- `src/hq/agent_manager.rs` and the existing UI path: launch nano and consume its events.
- Existing operation modules: expose small native helpers and the guards needed by their callers.
- `Cargo.toml`: add the necessary optional provider/client dependencies and features.

Keep `project/`, `server/`, `checks/`, graph/memory adapters, and external-agent implementations in
their current locations. There is no new `application/`, root-level `nano_host.rs`, or mandatory
library migration. The conceptual native-operation boundary does not require a new directory.

Create `shared/` only when an implementation has two actual consumers and no suitable existing
owner. A reusable conversation renderer is a possible later example. Keep nano session events in
`nano` initially; HQ can consume that public-to-the-crate contract. Do not move existing utilities
or domain operations into `shared/` merely because nano calls them.

Parse nano-specific settings only when nano is selected/invoked. Unrelated CLI, graph, and existing
external-agent operations must not initialize providers or external MCP clients. Keep provider
dependencies optional and session limits distinct from Ferrus task/review retry limits.

## 13. Delivery slices and acceptance

| Slice | Deliverable | Acceptance evidence |
| --- | --- | --- |
| N0: Native integration seam | Add `nano/ferrus.rs`; expose the first required helpers in place | Native/MCP parity for the touched operations and their role/lease guards; no broad module moves |
| N1: Smallest agent | Scripted and one real provider, core tools, bounded loop, journal, managed host | HQ launches nano; it edits an isolated task, checks, submits and exits; unsupported modes fail early |
| N2: Native context | Working set, verified snippets, invalidation, bounded projection and compaction | Stale source/edit conflicts handled; graph-disabled and unsupported-language tasks still work; benchmark ablations recorded |
| N3: Extensions and recovery | External stdio MCP, cancellation, interrupted-effect reconciliation | Namespaces, timeouts, unsupported capabilities, no duplicate effects, lease loss and process cleanup verified |
| N4: Interactive HQ | Shared command/event flow plus conversation/input UI | Same engine works headlessly and interactively; input wait and terminal geometry tested |
| N5: Standalone and roles | Standalone host/binary, then individual Supervisor role profiles | Works without Ferrus runtime registration; each added role preserves its lifecycle and authority |

N1 connects the remaining Executor operations incrementally and includes parity coverage for
claims, check failures, submit, paused flows, and graph routing. It also includes basic graph
calls; N2 makes graph use efficient and systematic. The first usable
headless MVP is N0-N3. It includes compactable sessions and minimal external MCP support, not just
a demo inference loop. N4/N5 are subsequent releases. Break each slice into reviewable changes;
keep each helper adjustment with the native use case that needs it instead of scheduling a
repository-wide preparatory refactor.

### Planned PRs and GitHub issues

Each row tracks one implementation PR. PRs 01-13 cover the first headless release, including
comparative evaluation; PRs 14-18 cover interactive HQ, standalone, and Supervisor profiles.
Dependencies identify prerequisites and allow independent work to proceed separately.
The [first issue](https://github.com/ferrus-dev/ferrus/issues/73) also contains this index and the architectural baseline.

| PR | Phase | Issue | Depends on |
| --- | --- | --- | --- |
| 01 | N0 | [#73: add native Ferrus session binding and operation entry points](https://github.com/ferrus-dev/ferrus/issues/73) | None |
| 02 | N1 | [#74: implement the bounded session engine and durable event journal](https://github.com/ferrus-dev/ferrus/issues/74) | #73 |
| 03 | N1 | [#75: add the first streaming LLM provider](https://github.com/ferrus-dev/ferrus/issues/75) | #74 |
| 04 | N1 | [#76: add bounded file search, reading, and patch tools](https://github.com/ferrus-dev/ferrus/issues/76) | #74 |
| 05 | N1 | [#77: add cancellable command sessions and bounded output](https://github.com/ferrus-dev/ferrus/issues/77) | #74 |
| 06 | N1 | [#78: load scoped instructions and expose native repository context tools](https://github.com/ferrus-dev/ferrus/issues/78) | #73, #74, #76 |
| 07 | N1 | [#79: implement the managed Executor lifecycle](https://github.com/ferrus-dev/ferrus/issues/79) | #73, #74, #76, #77, #78 |
| 08 | N1 | [#80: integrate headless launch and structured events with HQ](https://github.com/ferrus-dev/ferrus/issues/80) | #75, #79 |
| 09 | N2 | [#81: add a revision-aware repository working set](https://github.com/ferrus-dev/ferrus/issues/81) | #77, #78, #79 |
| 10 | N2 | [#82: add token-budgeted context projection and compaction](https://github.com/ferrus-dev/ferrus/issues/82) | #74, #75, #78, #81 |
| 11 | N3 | [#83: connect external stdio MCP tools through neva](https://github.com/ferrus-dev/ferrus/issues/83) | #74, #77, #79 |
| 12 | N3 | [#84: resume interrupted sessions and reconcile tool effects](https://github.com/ferrus-dev/ferrus/issues/84) | #77, #79, #80, #82, #83 |
| 13 | MVP validation (N0-N3) | [#85: add comparative evaluations and headless release gates](https://github.com/ferrus-dev/ferrus/issues/85) | #80, #81, #82, #83, #84 |
| 14 | N4 | [#86: add an interactive conversation view to Ferrus HQ](https://github.com/ferrus-dev/ferrus/issues/86) | #85 |
| 15 | N5 | [#87: add a standalone host and ferrus-nano executable](https://github.com/ferrus-dev/ferrus/issues/87) | #85 |
| 16 | N5 | [#88: reuse the conversation UI in the standalone executable](https://github.com/ferrus-dev/ferrus/issues/88) | #86, #87 |
| 17 | N5 roles | [#89: support Supervisor planning, specification, and archive sessions](https://github.com/ferrus-dev/ferrus/issues/89) | #86 |
| 18 | N5 roles | [#90: support Reviewer and Consultant session profiles](https://github.com/ferrus-dev/ferrus/issues/90) | #89 |

Minimum failure cases: graph unavailable; stale snippet; changed edit base; invalid task binding;
oversized output; malformed or repeated tool call; provider timeout/rate limit; compaction around
tool results; cancellation during commands/checks/submit; lease loss; consultation/answer recovery;
crash before/after an effect; stdout protocol corruption; and external MCP name/schema changes.

Run focused tests and the repository's required `cargo fmt --check`, `cargo clippy -- -D warnings`,
and `cargo test`. Add feature-specific gates for nano when implemented, including provider protocol
fixtures and a local MCP peer. Test the legacy/default neva profiles intentionally rather than
assuming `--all-features` selects the desired protocol. Keep real provider smoke tests explicit and
separate from deterministic CI; they require credentials and incur inference cost.

## 14. Evaluation before performance claims

Use a small fixed task suite with pinned starting trees: local bug fix, cross-file Rust change,
rename/refactor, configuration/docs change, unsupported-language change, graph-disabled task,
and recovery from stale context. Include independent review and resulting tests, not only whether
the agent invoked submit. Use repeated attempts and report quality and variance alongside cost.

Separate these comparisons:

| Variant | What it isolates |
| --- | --- |
| External Executor with Ferrus graph over MCP | Practical current baseline |
| Nano with the same graph requests/context formatting through a test MCP adapter | Harness behavior while retaining transport |
| Nano with equivalent native graph requests/context formatting | Transport contribution, compared with the previous row |
| Nano with native working-set/context policy | Retrieval/context contribution, compared with the previous row |
| Nano with graph disabled | Whether the graph contributes on the chosen tasks |

Keep model/version/settings, tools, task, checks, permissions, and initial repository state as
similar as each experiment permits. External integrations may differ in hidden prompts, model
availability, caching, or billing; disclose those differences and do not attribute the entire
delta to MCP. Run cold and warm index/cache cases and include indexing/refresh cost.

Record success/review acceptance, review cycles, input/output and cached tokens, provider cost
when available, model turns, tool calls, duplicate source bytes, graph/fallback usage, stale
evidence events, context assembly time, refresh time, tool latency, total time, and peak memory.
Report latency distributions and sample counts rather than a single fastest run.

There is no defensible percentage improvement yet. A useful first result is comparable task
quality with demonstrably lower token use or elapsed time on a stated workload, without weaker
checks or relaxed workspace rules. Keep the native context policy independently disableable so
the claimed benefit remains testable.

## 15. Reference designs

Read on 2026-09-06. These are sources for design ideas, not dependencies or copied implementations.

| Project | Observed idea to reuse | Keep outside nano MVP |
| --- | --- | --- |
| [Pi agent core](https://github.com/earendil-works/pi/blob/9767ba275f3e9a5ee0f5c5342249b629ab1b2282/packages/agent/README.md) | Separate stored agent messages from the model projection; stream session/tool events | Broad extension/package system and runtime customization surface |
| [Codex session protocol](https://github.com/openai/codex/blob/ac192cd7937b0d73edc6dffe009940ae53782dd4/codex-rs/protocol/src/protocol.rs) | Submission and event queues decouple clients from agent execution | Reproducing the full application protocol and product feature set |
| [Goose state-machine module](https://github.com/aaif-goose/goose/blob/5e90925962f05acf8e255032de44d16c4a7768a2/crates/goose/src/agents/state_machine/mod.rs) | Explicit operations/effects over persisted conversation state; the inspected module has a runtime environment flag | A general-purpose configurable operation pipeline |
| [Grok Build layout](https://github.com/xai-org/grok-build/blob/72a61251fcffb464bcc687aeb5a998e5a98ec0c9/README.md#repository-layout) | Separate runtime, tools, workspace, and frontend composition | Its large crate hierarchy and TUI scope |

## 16. Decisions still open

- Validate the first loaded model against the opt-in LM Studio smoke test before claiming N1 complete.
- Concrete token/time/output defaults, calibrated with the task suite rather than guessed savings.
- Which platform gets the first enforced command sandbox after the trusted-local MVP.
- Whether standalone first ships a shared terminal UI or a headless executable before that UI.

The proposed defaults are fixed native Ferrus tools, one production provider, a sequential engine,
local session journals, optional graph acceleration with explicit fallbacks, and the existing
Ferrus lifecycle as the authority.
