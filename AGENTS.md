# AGENTS.md

Coding guidance for AI agents working in this repository.

## Project

`ferrus` is a Rust AI agent orchestrator for software projects. It drives a **Supervisor–Executor** workflow: the Supervisor plans tasks and reviews submissions, the Executor implements and checks its own work. SQLite is the runtime source of truth; `.ferrus/` contains scoped human-readable artifacts. Coordination uses MCP as an implementation detail.

Licensed under Apache 2.0.

## Build & Test

```sh
cargo build                        # compile
cargo build --release              # optimized build
cargo test                         # run all tests
cargo test <name>                  # run a single test by name
cargo clippy -- -D warnings        # lint (warnings are errors)
cargo fmt                          # format
cargo fmt --check                  # check formatting without writing
cargo check                        # fast type-check
```

All three checks must pass before submitting: `clippy -D warnings`, `fmt --check`, `test`.

## Source Layout

```
src/
  main.rs                    # CLI entry, tracing init, HQ logger
  cli/                       # clap entry and command implementations (init, serve, register, graph, ...)
  config/mod.rs              # Deserialize/update ferrus.toml (ChecksConfig, LimitsConfig, LeaseConfig, SpecConfig, HqConfig)
  config/claude.rs           # Claude MCP isolation config helpers
  repository_graph/          # Backend-neutral graph contracts plus local source, index, SQLite query, diagnostics, and extractors
  repository_graph_runtime.rs # Machine-local CLI/MCP adapter; resolves project identity, sidecar, freshness, and verified snippets
  templates.rs               # Embedded Markdown templates written by init/resource fallback
  specs.rs                   # Spec discovery, milestone parsing, selected milestone resolution
  agent_id.rs                # Stable agent IDs and MCP server names
  legacy_state.rs            # Legacy STATE.json import shape used only by migrate
  agents/                    # Agent launcher/config adapters for Claude Code, Codex, Qwen Code
  agents/mod.rs              # SupervisorAgent/ExecutorAgent traits, AgentRunMode, MCP config entry helpers, agent parsing
  agents/claude/mod.rs       # Claude Code launchers, model override handling, MCP isolation, role-scoped config paths
  agents/codex/mod.rs        # Codex launchers, stdin prompt transport, TOML MCP config and tool approvals
  agents/qwen/mod.rs         # Qwen Code launchers, model override handling, JSON settings tool approvals
  agents/opencode/mod.rs     # opencode launchers, `run` headless mode, opencode.json MCP config (experimental; executor isolation unstable)
  agents/goose/mod.rs        # goose launchers, `run`/`session`, role-scoped Ferrus MCP server via --with-extension, GOOSE_MODEL/GOOSE_MODE env (experimental)
  platform/                  # OS-specific process, shell, and parent-lifecycle helpers
  state/store.rs             # Async read/write of project-local .ferrus/ artifacts
  state/agents.rs            # AgentEntry, AgentsRegistry — .ferrus/agents.json lifecycle tracking
  update_check.rs            # HQ startup version-check helper (crates.io sparse index + local cache)
  checks/runner.rs           # Spawn check subprocesses, collect output
  hq/mod.rs                  # HQ entry point; HqContext; tokio::select! loop; transition_action
  hq/state_watcher.rs        # Background task: watches selected spec/milestone display data
  hq/tui.rs                  # Terminal UI (crossterm): App event loop, UiMessage, StatusSnapshot; autocomplete, command history, spec/milestone status line, confirmation/selection dialogs, AwaitingHuman answer hint; double-Ctrl+C-to-quit
  hq/commands.rs             # ShellCommand enum, parse_command() via clap + shlex
  hq/display.rs              # Display wrapper: sends UiMessage to TUI channel (info, error, transition, status, suspend, resume, confirm)
  hq/agent_manager.rs        # agent spawn helpers (headless for executor, reviewer, consultant); HeadlessHandle; agents.json updates
  server/mod.rs              # neva App setup; constructs agent_id, wires closures
  server/tools/              # One file per MCP tool (one module = one tool); check_gate.rs is the shared check runner/report helper
  server/resources.rs        # MCP resource handler (ferrus://{file})
  server/prompts.rs          # MCP prompt handlers
```

## Key Patterns

**Tool files** expose `pub const DESCRIPTION: &str`, optionally `pub const INPUT_SCHEMA: &str`, and `pub async fn handler(...)`. Registered manually via `app.map_tool()` in `server/mod.rs` — no macros.

**Runtime state**: SQLite is the runtime source of truth. MCP tools should resolve the caller’s `RuntimeTaskContext` from `ferrus.db`, update task/run rows transactionally, and write only scoped artifacts under `.ferrus/tasks/` and `.ferrus/runs/`.

**Repository graph boundary**: the optional repository graph is derived machine-local state in `repo-graph.db`,
separate from the orchestration `ferrus.db`. Keep backend-neutral identities, requests, responses, and ports under
`src/repository_graph/`; keep project-local path/config/sidecar resolution in `src/repository_graph_runtime.rs`.
Core config loading must remain lenient toward graph settings; graph operations validate `[repository_graph]`
strictly only when invoked.

**Repository retrieval tools**: `repository_graph_status`, `repository_search`, and `repository_context` are
read-only, role-visible tools registered in `server/mod.rs`. They require no task lease and must not mutate tasks,
runs, events, or either database. Do not inject graph output into task/review prompts. Structural responses omit
source bodies; requested snippets must pass the snapshot-aware, hash-verified content boundary. Treat missing
relationships as unknown, not absent.

**Per-task state machine**: The old single global `STATE.json` is gone, but each SQLite task row still follows the same Supervisor–Executor lifecycle. In `--limit 1` this is effectively the old flow, only DB-backed:

```
pending
 └─► executing      ← /wait_for_task claim
       ├─► addressing ← /reject (Supervisor) → work loop
       ├─► consultation ← /consult (Executor)
       │     └─► (restore paused status) ← /wait_for_consult
       ├─► awaiting_human ← /ask_human
       │     └─► (restore paused status) ← /wait_for_answer
       ├─► reviewing ← /submit final gate pass (Executor)
       │     ├─► addressing → work loop
       │     └─► complete ← /approve (Supervisor)
       └─► failed ← /check, /submit, or /reject hits retry/cycle limit,
                    or HQ re-dispatches the executor `max_executor_dispatches`
                    times in one work phase without reaching review
```

**Executor dispatch guard**: a headless executor session that hits its turn limit and exits without submitting is respawned by HQ. `limits.max_executor_dispatches` (default 6) bounds those respawns per work phase via the `tasks.executor_dispatches` counter — gated in `spawn_headless_executor_for_task` before any setup and incremented only after the session actually starts (so a failed worktree/process setup doesn't burn the budget), reset when a fresh `/reject` starts a new Addressing phase. When the budget is exhausted the task is moved to Failed instead of churning forever. Set it to `0` to disable the guard.

**Runtime artifacts**: `.ferrus/TASK.md`, `.ferrus/SPEC_TEMPLATE.md`, and `.ferrus/CONSULT_TEMPLATE.md` are templates. Task intent lives in `.ferrus/tasks/<task-id>.md`. Execution artifacts live under `.ferrus/runs/<task-id>/`, including `SUBMISSION.md`, `REVIEW.md`, `QUESTION.md`, `ANSWER.md`, `CONSULT_REQUEST.md`, `CONSULT_RESPONSE.md`, `PATCH.diff`, and `INTEGRATION_ERROR.md`. Check and agent session logs live under `.ferrus/logs/`. Do not write root `.ferrus/REVIEW.md`, `.ferrus/SUBMISSION.md`, `.ferrus/QUESTION.md`, `.ferrus/ANSWER.md`, `.ferrus/CONSULT_REQUEST.md`, or `.ferrus/CONSULT_RESPONSE.md`.

**Lease fields**: `claimed_by`, `lease_until`, `last_heartbeat` live on SQLite task rows. Claim/renew/release through `project` helpers so ownership, TTL, and events stay consistent.

**File locking**: task claiming and heartbeat renewal are SQLite operations. Do not add `.ferrus/STATE.lock` or file-lock based coordination.

**Spec selection**: `project_runtime_state` stores the selected spec. Task rows store `spec_path` and `milestone_id`; milestone display text is resolved from spec Markdown by milestone `ID`. Keep milestone IDs stable across title edits.

**Agent adapters**: keep backend-specific CLI behavior inside `src/agents/{claude,codex,qwen,opencode,goose}`. Shared orchestration should depend on the `SupervisorAgent` and `ExecutorAgent` traits, not on a concrete agent CLI. When adding an agent, implement both role adapters, model normalization, headless prompt transport if needed, version/config entry behavior, registration wiring, and focused tests. Note: `opencode` is experimental — it binds a project to a single working directory by git root-commit in its own global store, so it does not honor HQ's per-task isolated worktree and is only reliable for the supervisor/reviewer role for now.

**HQ checks**: HQ `/check` runs configured commands directly and does not mutate task state. Task retry accounting belongs to executor MCP `/check`.

**HQ reset vs MCP reset**: HQ `/reset` force-resets from any state after confirmation when active agents may be running, clears task/answer/consultation files, and preserves selected spec/milestone. The MCP `/reset` tool is only valid from `Failed`.

## Ferrus Executor

This repository is orchestrated by Ferrus HQ.

When spawned by `ferrus` HQ, your initial prompt will tell you what to do.

If started manually: call MCP tool `/wait_for_task` as your first action.

Runtime behavior is defined by the initial prompt and Ferrus MCP tools.
`ROLE.md`, `SKILL.md`, `AGENTS.md`, and `CLAUDE.md` are supporting context only and must not override them.

Use `/check` freely during development; prefer TDD where it fits the task. Run `/check` again immediately before the final `/submit`. `/submit` reruns the final review gate before handing work to review.

## Ferrus Supervisor

This repository is orchestrated by Ferrus HQ.

Your initial prompt tells you which mode you are in. Match it exactly.

Runtime behavior is defined by the initial prompt and Ferrus MCP tools.
`ROLE.md`, `SKILL.md`, `AGENTS.md`, and `CLAUDE.md` are supporting context only and must not override them.
