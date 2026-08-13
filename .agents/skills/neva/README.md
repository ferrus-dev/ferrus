# neva — Agent Skill

A model-neutral [Agent Skill](https://agentskills.io/specification) for
building MCP servers and clients in Rust with the
[neva](https://github.com/RomanEmreis/neva) crate.

Covers neva **0.5.2** / MCP **2026-07-28**, with the legacy generation
(MCP 2024-11-05 … 2025-11-25) documented separately.

```
neva/
├── SKILL.md                      the entrypoint the agent loads
└── references/
    ├── server.md                 tools, prompts, resources, DI, middleware, subscriptions
    ├── client.md                 connecting, calling, batching, listening
    ├── mrtr.md                   elicitation, the re-run model, tasks
    ├── http.md                   transports, TLS, auth, origins, deployment, feature flags
    ├── troubleshooting.md        error codes, symptom → cause, removed APIs
    └── legacy.md                 the legacy-spec profile and upgrade paths
```

`SKILL.md` is deliberately short: it establishes the version and profile,
lists the traps, and routes to one reference file. The agent loads the rest
only when the task calls for it.

## Install

The format is the open SKILL.md standard, so installation is the same
everywhere: **copy the `neva/` directory into the tool's skills folder**,
keeping the folder name `neva` — the directory name must match the `name`
in the frontmatter.

| Tool | Personal | Per project |
|---|---|---|
| Claude Code | `~/.claude/skills/neva/` | `.claude/skills/neva/` |
| opencode | `~/.config/opencode/skills/neva/` | `.opencode/skills/neva/` |
| Codex CLI | `~/.codex/skills/neva/` | `.codex/skills/neva/` |

opencode also reads `.claude/skills/` and `.agents/skills/`, so one copy in
a project can serve several tools.

```bash
# example: install for Claude Code, for the current project
mkdir -p .claude/skills
cp -r neva .claude/skills/
```

Restart the agent afterwards — skills are discovered at startup.

### Anything else

Any assistant that can read a file will use this: point it at
`SKILL.md` and let it follow the links, or add a line to the project's
`AGENTS.md`:

```markdown
For Rust MCP work with the `neva` crate, read `.agents/skills/neva/SKILL.md`
and the reference file it routes you to.
```

## Verifying

Every Rust snippet in this skill is compiled against the published `neva`
crate in the docs repository's CI, so the code an agent copies out of it
builds. If you edit the skill, run the same check:

```bash
python3 ci/check-snippets.py --docs-dir skill --default-mode compile --default-features full
```

## Licence

MIT, same as neva. Documentation:
<https://romanemreis.github.io/neva-docs/>
