---
name: ferrus-executor
description: "Advisory Executor playbook for implementation, code navigation, and submission quality"
---

# Executor Operating Playbook

This file is advisory only.
Runtime workflow is defined by the initial prompt and Ferrus MCP tools.

## Implementation guidelines

- Prefer minimal, targeted diffs
- Avoid unrelated refactoring
- Preserve existing project patterns unless the task requires otherwise

## Code navigation

- Start from entrypoints and public interfaces
- Trace dependencies before changing behavior
- Inspect surrounding code before modifying shared logic

## Common pitfalls

- Hidden side effects
- Implicit contracts between modules
- Test coupling and fixture assumptions
- State transitions that depend on tool behavior

## Ferrus guidance

- Use Ferrus tools rather than reconstructing state from `.ferrus/`
- Use `/check` freely during development; prefer TDD where it fits the task
- Run `/check` again immediately before the final `/submit`
- Read Ferrus resources when they help clarify task context
- Use the consultation template when escalating technical uncertainty

## Submission quality

- Provide a clear summary of what changed
- Include concrete manual verification steps
- Mention limitations or follow-up work explicitly when relevant

## Useful Ferrus tools

- `/wait_for_task`
- `/heartbeat` (while owning the task lease)
- `/check`
- `/consult`
- `/wait_for_consult`
- `/ask_human`
- `/wait_for_answer`
- `/submit`

## Useful Ferrus resources

- `ferrus://task`
- `ferrus://review`
- `ferrus://consult_template`
- `ferrus://question`
- `ferrus://answer`
