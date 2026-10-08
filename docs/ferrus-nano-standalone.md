# Standalone Nano

`ferrus-nano` runs a headless coding session in an explicitly selected directory.
Git and Ferrus registration are optional. It does not start HQ, load a selected spec,
claim a task, or open/create `ferrus.db`. Interactive standalone UI is tracked in #88.

## Installation and provider settings

Release archives include `ferrus-nano` alongside `ferrus`; the installers copy both
when present. For a source installation:

```sh
cargo install ferrus --locked --profile dist --features nano-openai,nano-mcp --bin ferrus-nano
```

For a checkout:

```sh
cargo build --release --features nano-openai,nano-mcp --bin ferrus-nano
```

`nano-openai` enables the executable. Add `nano-mcp` only when external peers are
needed. Default `cargo build` and `cargo install ferrus` retain their existing feature
set and do not include the standalone executable.

Use the same private provider settings as managed Nano. Resolution order is
`--config`, `FERRUS_NANO_CONFIG`, then `~/.ferrus/nano.toml` (the user's home directory
on Windows too). An explicit path must be absolute. For local LM Studio, a minimal
file contains:

```toml
base_url = "http://127.0.0.1:1234/v1"
model = "your-loaded-model"
reasoning_effort = "none"
```

Create it with owner-only permissions: `chmod 600 ~/.ferrus/nano.toml` on Unix, or a
protected DACL granting only its owner on Windows. See [provider settings](ferrus-nano-provider.md)
for credentials, optional generation parameters, transport limits, and session budgets.
Standalone never provisions project metadata or uses provider secrets in command children.

## Running a request

```sh
ferrus-nano --workspace /absolute/path/to/repository --prompt "Fix the parser and run its tests"
printf '%s\n' "Inspect the parser" | ferrus-nano --workspace /absolute/path/to/repository
```

The workspace must already exist. Requests are bounded UTF-8 input. Stdout contains
the final model response; stderr contains the session ID, journal path, and diagnostics. Exit zero
requires a durable `ModelFinished` journal record. It means the model completed its
response, not that Ferrus checks passed or a managed submission was approved.
Ctrl-C cancels the session and stops/joins owned command processes before recording
the terminal outcome where possible. Unexpected process death still requires recovery.

Native tools include `read_file`, `search_text`, `apply_patch`, `exec`, `read_process`,
`read_output`, `stop_process`, `load_instructions`, and `repository_fallback`.
Root `AGENTS.md` is loaded automatically; nested guidance and explicit skills use the
same bounded instruction loader as managed Nano. The system policy is standalone:
there is no `wait_for_task`, `check`, `submit`, consultation, or task-backed human wait.
Validation commands run through `exec`; they do not issue managed check receipts.

### Trusted-local execution

Launching the executable opts into trusted-local workspace and shell execution.
File tools use the shared path, digest, quota, and protected-metadata rules. Commands
run with the user's OS permissions, without an OS sandbox or per-command approval UI;
they can affect files outside the workspace. Their environment is allowlisted and
does not inherit provider credentials, Git overrides, startup hooks, or Ferrus authority.
External MCP peers receive only their explicitly configured environment.

Standalone has no managed Git ownership. Its policy leaves staging, commits, reset,
and worktree management to the user unless explicitly requested. Do not run it against
a workspace concurrently edited by HQ, another harness, or another user. Standalone
sessions themselves share a machine-local advisory workspace lock, including when
different journal storage paths are selected; this is not an orchestration task lease.

## Journals and continuation

Default storage is `~/.ferrus/standalone/<workspace-id>/`. The ID comes from filesystem
directory identity, so case/normalization aliases share the same local workspace slot.
`--storage /absolute/private/directory` selects another storage directory whose parent
must exist; it must be outside the workspace. Storage is owner-only and bound to that
workspace. It contains no registry, task rows, or orchestration database.

The existing append-only journal, checkpoint, output, and command-spool contracts apply:

```text
<storage>/workspace.json
<storage>/workspace.lock
<storage>/nano/sessions/<session-id>/events.jsonl
<storage>/nano/sessions/<session-id>/commands/
```

The separate workspace lock lives under `~/.ferrus/standalone/locks/`. Use
`--session-id NAME` for a known ID; otherwise Nano generates one and prints it to stderr.

```sh
ferrus-nano --workspace /absolute/path/to/repository --resume PREVIOUS_ID \
  --prompt "Continue with the next change"
```

Continuation creates a new journal. It imports a bounded previous transcript as
untrusted historical user content, not as replayed tool calls or live provider state.
It retains cumulative token, model-turn, tool-call, and retry accounting; the explicit
new request resets no-progress tracking and gets a fresh per-attempt elapsed deadline.
An interrupted provider reservation is conservatively charged. Current settings and
scoped guidance are loaded again. Oversized history fails closed rather than being
silently dropped.

Recovery repairs only an incomplete journal tail and seals an interrupted prior attempt.
It never re-executes recorded operations or trusts old PIDs. Pending/unknown effects,
unconfirmed commands, and incomplete/inconsistent output spools reject continuation
before inference. Reconcile them manually before starting fresh work. Confirmed exited,
cancelled, and timed-out commands require complete spools. Completed source journals
remain unchanged.

## Optional local context

Repository indexing is explicit and independent of Ferrus project configuration:

```sh
ferrus-nano --workspace /absolute/path/to/repository --index-graph
ferrus-nano --workspace /absolute/path/to/repository --graph --prompt "Find the parser entry points"
```

`--index-graph` builds/refreshes `<storage>/repo-graph.db` and exits without loading
provider settings. It uses the shipped local discovery/extractor policy and a dedicated
standalone repository namespace/publication. `--graph` exposes the three native
repository retrieval tools; queries never build indexes or resolve managed task views.
The same request/result/time/byte caps apply. Each request pins the selected snapshot;
requested repository snippets pass the existing hash-verified content boundary.

Retrieval does not rescan the full repository to claim freshness. External edits leave
freshness `unknown`; current file tools remain the source of workspace evidence. Nano
records known mutations before effects and blocks stale graph retrieval until explicit
reindex. Graph-disabled/missing/stale coverage uses labeled workspace fallback. Working
sets never reuse graph evidence as current without compatible verified revisions.

An existing curated project-memory sidecar can be supplied read-only:

```sh
ferrus-nano --workspace /absolute/path/to/repository \
  --memory-sidecar /absolute/path/to/project-memory.db \
  --memory-namespace local:ferrus --memory-project PROJECT_ID \
  --prompt "Inspect prior specification decisions"
```

The explicit namespace/project identify the sidecar's portable scope. Memory queries
pin its published revision, report freshness independently, and cross into repository
facts only through exact revision/snapshot link sets. Standalone does not discover,
index, or author memory sources. Memory source snippets have no standalone verified
content adapter yet; use structural memory context with `include_snippets = false`.

Optional external peers use `mcp_config_file` in provider settings and the existing
[bounded MCP client](ferrus-nano-mcp.md). Managed-binding peers are omitted before
discovery and proxy setup. No Ferrus MCP loopback or remote graph/memory client is
initialized implicitly. `native_context_enabled` and `working_set_enabled` retain their
provider-file switches; an explicitly requested index build is independent of them.

## Verification

`tests/nano_standalone.rs` drives the executable with an offline streaming provider,
native tools, explicit graph/memory sidecars, and an optional external MCP fixture.
It covers unregistered Git/non-Git directories, storage binding, continuation, unknown
effects, and absence of orchestration state. Live model validation remains opt-in.
