# ferrus

[![Ferrus version](https://img.shields.io/badge/ferrus-0.4.1--alpha.1-orange)](https://crates.io/crates/ferrus)
[![Rust version](https://img.shields.io/badge/rustc-1.95+-964B00)](https://releases.rs/docs/1.95.0/)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](https://github.com/ferrus-dev/ferrus/blob/main/LICENSE)
[![Rust](https://github.com/ferrus-dev/ferrus/actions/workflows/rust.yml/badge.svg)](https://github.com/ferrus-dev/ferrus/actions/workflows/rust.yml)
[![Publish](https://github.com/ferrus-dev/ferrus/actions/workflows/publish.yml/badge.svg)](https://github.com/ferrus-dev/ferrus/actions/workflows/publish.yml)

**Deterministic orchestration of AI agents for real software work.**

Ferrus turns coding agents into controlled, repeatable workers.

It runs a Supervisor -> Executor -> Reviewer loop over your repository -- not as a chat, but as a **state machine**.
Tasks are planned, implemented, checked, and reviewed in a structured, restart-safe flow. Unlike chat-based agents, ferrus enforces structure and lifecycle.

Everything is explicit:
- Runtime state lives in SQLite; task context lives in scoped Markdown artifacts
- Optional repository graph facts live in a separate machine-local SQLite sidecar
- Agents are stateless between runs
- Crashes are recoverable
- No hidden context

## Supported agents

Ferrus works with existing coding agents:

- **Codex**
- **Claude Code**
- **Qwen Code** (experimental)
- **goose** (experimental)
- **opencode** (experimental; Supervisor and Reviewer only)

Agents are treated as interchangeable workers -- ferrus provides the runtime, coordination, and state.
See the [agent adapter notes](docs/cli-and-runtime.md#agent-adapters) for limitations and configuration details.

>  **Status**: ferrus is currently in alpha and not ready for production.

[Tutorial](https://ferrus.dev) | [Roadmap](https://github.com/ferrus-dev/ferrus/blob/main/docs/milestones.md)

---

## How it works

```
  you
   |
   +-> ferrus HQ
         |
         +-> Supervisor (Claude Code or Codex) -- plans tasks
         |         | exits after task created;
         |
         +-> Executor (Claude Code or Codex)   -- implements, checks, submits
         |         | runs headlessly
         |
         +-> Reviewer (Claude Code or Codex)   -- spawned automatically on submission
                   | exits after approve/reject; runs headlessly
```

HQ watches state transitions and spawns the right agent at the right time.

State is coordinated through `ferrus.db`, with human-readable task context under `.ferrus/tasks/` and `.ferrus/runs/`. If an agent crashes and restarts, Ferrus can recover its run and task lease without reconstructing state from Markdown files.

---

## Quick start

Install:

```sh
cargo install ferrus
# or on Linux/macOS:
curl -fsSL https://ferrus.dev/cli/install.sh | sh
```

```powershell
# or on Windows:
iwr https://ferrus.dev/cli/install.ps1 -useb | iex
```

For a smaller Cargo installation, use the same size profile as release assets:

```sh
cargo install ferrus --locked --profile dist
```

HQ update notifications are enabled by default. To omit the update-check HTTP/TLS client:

```sh
cargo install ferrus --locked --profile dist --no-default-features
```

See [build profiles and size measurements](docs/binary-size.md) for the tradeoffs.

Run:

```sh
ferrus init                                                # scaffold ferrus.toml, .ferrus/, and ~/.ferrus project state
ferrus register --supervisor claude-code --executor codex  # write agent configs and tool permissions
ferrus                                                     # enter HQ
```

Then type `/task` -- a supervisor spawns, you describe what you want, and the full loop runs automatically.

On Linux and macOS for `x86_64` and `aarch64`/`arm64`, `install.sh` downloads the matching release binary into `~/.local/bin` by default. On Windows, `install.ps1` installs `ferrus.exe` into `%LOCALAPPDATA%\ferrus\bin` by default. Release archives are verified with published SHA-256 checksums before installation. Set `FERRUS_INSTALL_DIR` to override the destination, or `FERRUS_INSTALL_VERSION=vX.Y.Z` to install a specific release tag.

---

## Core workflow

`ferrus` opens HQ. The main path is deliberately small:

```text
/task -> Supervisor defines work -> Executor implements and checks -> Reviewer approves or rejects
```

Tasks advance independently through SQLite-backed states. Git projects use isolated Executor worktrees;
non-Git projects run in the project directory with one Executor. Checks run before submission, and rejected
work resumes with review feedback. `/status`, `/tasks`, `/runs`, and `/events`
provide local inspection; `ferrus doctor` and `ferrus recover` handle consistency and interrupted work.

The full HQ command list, state transitions, CLI reference, configuration example, graph commands, and runtime file
layout are in [docs/cli-and-runtime.md](docs/cli-and-runtime.md).

## Repository intelligence

Ferrus has two optional, independently revisioned local indexes:

- the repository graph for deterministic structural navigation and snapshot-aware snippets;
- project memory for approved outcomes, specification history, and bounded provenance.

Both use separate machine-local SQLite sidecars. They are read-only from agent retrieval tools, never replace
`ferrus.db`, and never become implicit prompt context. Task worktrees receive pinned graph baselines plus mutable
overlays; review uses a frozen submitted view. The same contracts also define a vendor-neutral path to distributed
storage and workers.

Start with [repository graph architecture](docs/repository-graph-architecture.md),
[project memory architecture](docs/project-memory-architecture.md), and
[distributed indexing architecture](docs/distributed-indexing-architecture.md).

## Documentation

- [CLI, HQ, configuration, and runtime reference](docs/cli-and-runtime.md)
- [Roadmap and milestones](docs/milestones.md)
- [Repository graph retrieval](docs/repository-graph-retrieval.md)
- [Repository graph benchmarks](docs/repository-graph-benchmarks.md)
- [Repository graph evaluation](docs/repository-graph-evaluations.md)
- [Project memory operations](docs/project-memory.md)
- [Project memory evaluation](docs/project-memory-evaluations.md)
- [Semantic retrieval and embeddings](docs/semantic-retrieval.md)
- [Feature specifications](docs/specs/)

## Dogfooding

Ferrus is partially developed using its own orchestration workflow.

This repository is used to validate the Supervisor -> Executor -> Reviewer loop in real development scenarios.

---

## Getting involved

If you're interested in Ferrus:

- Try running it on your project
- Share feedback on the workflow (what breaks, what feels unnatural)
- Open issues with observations or ideas

At this stage, feedback on the model is more valuable than code contributions.

---

## Licence

Licensed under Apache 2.0.
