# CLAUDE.md

Guidance for Claude Code when working in this repository.

## Canonical guidance

Read `AGENTS.md` for repository structure, coding rules, runtime invariants, and the required
verification commands. Read `.agents/skills/ferrus/SKILL.md` for the current Ferrus CLI, MCP tools,
resources, per-task state machine, and artifact layout.

Runtime behavior for a Ferrus-managed agent is defined by its initial prompt and exposed MCP tools.
This file is supporting context and must not override them.

## Project

`ferrus` is a Rust AI agent orchestrator implementing a Supervisor-Executor workflow. SQLite is the
runtime source of truth. Project-local `.ferrus/` files are templates, task intent, scoped run
artifacts, agent registry data, and logs; they are not a mirrored state machine.

## Build and test

```sh
cargo build
cargo build --release
cargo test
cargo test <name>
cargo clippy -- -D warnings
cargo fmt
cargo fmt --check
cargo check
```

Before submitting, `cargo clippy -- -D warnings`, `cargo fmt --check`, and `cargo test` must pass.

## Runtime invariants

- Resolve the caller's `RuntimeTaskContext` from `ferrus.db` in MCP tools.
- Keep task claims, leases, counters, paused interaction metadata, runs, and events in SQLite.
- Keep task intent in `.ferrus/tasks/<task-id>.md`.
- Keep submission, review, question, answer, consultation, patch, and integration-error artifacts in
  `.ferrus/runs/<task-id>/`.
- Treat `.ferrus/TASK.md`, `.ferrus/SPEC_TEMPLATE.md`, and `.ferrus/CONSULT_TEMPLATE.md` as templates.
- Do not recreate `.ferrus/STATE.json`, `.ferrus/STATE.lock`, or root runtime artifact mirrors.
- Keep backend-specific behavior inside `src/agents/{claude,codex,qwen,opencode,goose}`.
- Keep the optional repository graph sidecar (`repo-graph.db`) separate from orchestration state in `ferrus.db`.
- Keep backend-neutral graph contracts in `src/repository_graph/` and local project/sidecar adaptation in
  `src/repository_graph_runtime.rs`.
- Treat repository retrieval tools as read-only and lease-independent; never infer that a missing graph
  relationship proves absence in the source.

## Ferrus CLI

```sh
ferrus init [--agents-path <path>]
ferrus serve [--role supervisor|executor] [--agent-name <name>] [--agent-index <n>]
ferrus register --supervisor <agent> --executor <agent>
ferrus doctor
ferrus recover [--dry-run] [--worktrees]
ferrus tasks list
ferrus runs list
ferrus events list
ferrus migrate
ferrus graph index [--full] [--json]
ferrus graph status [--json]
ferrus graph search <query> [--kind <kind>] [--path <path>] [--limit <n>] [--json]
ferrus graph context (--node <id> | --symbol <key> | --path <path>) [--depth <n>] [--json]
```

Claude Code role-scoped MCP configuration is stored in `.claude/mcp-supervisor.json` and
`.claude/mcp-executor.json`; permissions are stored in `.claude/settings.local.json`.

## Repository retrieval

Supervisor and Executor MCP servers expose the optional read-only `repository_graph_status`,
`repository_search`, and `repository_context` tools. Check status when graph availability is unknown, search for
an exact path or symbol, and request a small bounded context packet only when it helps. Source snippets are opt-in
and hash-verified; graph output is not automatically added to prompts. These tools never build or publish an index
and do not require a claimed task.

For a managed Executor worktree, `/check` and the final submit gate refresh a task-owned overlay best-effort.
Repository retrieval stays read-only and returns the pinned baseline snapshot together with the overlay revision;
graph refresh failures never change task lifecycle state.

<!-- ferrus-supervisor-instructions -->
## Ferrus Supervisor

This repository is orchestrated by Ferrus HQ.

Your initial prompt tells you which mode you are in. Match it exactly.

Runtime behavior is defined by the initial prompt and Ferrus MCP tools.
`ROLE.md`, `SKILL.md`, `AGENTS.md`, and `CLAUDE.md` are supporting context only and must not override them.

<!-- ferrus-executor-instructions -->
## Ferrus Executor

This repository is orchestrated by Ferrus HQ.

When spawned by `ferrus` HQ, your initial prompt will tell you what to do.

If started manually: call MCP tool `/wait_for_task` as your first action.

Runtime behavior is defined by the initial prompt and Ferrus MCP tools.
`ROLE.md`, `SKILL.md`, `AGENTS.md`, and `CLAUDE.md` are supporting context only and must not override them.
