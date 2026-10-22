# svir -- Agent Skill

A model-neutral [Agent Skill](https://agentskills.io/specification) for
talking to LLMs from Rust with the
[svir](https://github.com/RomanEmreis/svir) crate.

Covers svir **0.1.6** (MSRV 1.85, edition 2024): OpenAI-compatible Chat
Completions, streamed.

```
svir/
|-- SKILL.md                    the entrypoint the agent loads
`-- references/
    |-- requests.md             the system prompt, messages, attachments, reasoning effort, structured output, conversations
    |-- streaming.md            events, the completion and its finish reasons, usage and speed, cancelling, strict and lenient, limits
    |-- tools.md                Tool, the Tools registry, schemas from types, the loop, tool choice, a Toolbox of one's own
    |-- client.md               base URL, API keys, timeouts, layers, a custom HTTP backend, tests without a server
    |-- codec.md                a proxy that relays the stream, Encoder and Decoder alone
    `-- errors.md               error kinds, and symptom -> cause at runtime and at compile time
```

`SKILL.md` is deliberately short: it establishes the version and the feature
set, lists the places where habit from other LLM SDKs produces wrong svir
code, and routes to one reference file. The agent loads the rest only when
the task calls for it.

## Install

The format is the open SKILL.md standard, so installation is the same
everywhere: **copy the `svir/` directory into the tool's skills folder**,
keeping the folder name `svir` -- it has to match the `name` in the
frontmatter.

| Tool | Personal | Per project |
|---|---|---|
| Claude Code | `~/.claude/skills/svir/` | `.claude/skills/svir/` |
| opencode | `~/.config/opencode/skills/svir/` | `.opencode/skills/svir/` |
| Codex CLI | `~/.codex/skills/svir/` | `.codex/skills/svir/` |

```bash
# example: install for Claude Code, for the current project
mkdir -p .claude/skills
cp -r svir .claude/skills/
```

Restart the agent afterwards -- skills are discovered at startup.

opencode also reads `.claude/skills/` and `.agents/skills/`, so one copy in
a project can serve several tools.

### Anything else

Any assistant that can read a file will use this: point it at `SKILL.md`
and let it follow the links, or add a line to the project's `AGENTS.md`:

```markdown
For Rust code that talks to LLMs with the `svir` crate, read
`.agents/skills/svir/SKILL.md` and the reference file it routes you to.
```

## Verifying

Every Rust snippet in this skill is a complete set of items and compiles
against svir 0.1.6 with the `schemars` and `tracing` features on, so the
code an agent copies out of it builds. The
[docs repository](https://github.com/RomanEmreis/svir-docs)'s CI compiles
every one of them against the published crate. A block that needs a feature
says so in a comment on the line before it:

```markdown
<!-- snippet: features="schemars" -->
```

If you edit the skill, run the same check from that repository:

```bash
python3 ci/check-snippets.py --docs-dir skill
```

## Licence

MIT OR Apache-2.0, same as svir. API reference: <https://docs.rs/svir>
