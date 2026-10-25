---
name: ferrus-executor
description: "Advisory Executor playbook for implementation, code navigation, and submission quality"
---

# Executor Operating Playbook

This file is advisory only.
Runtime workflow is defined by the initial prompt and exposed Ferrus tools.

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

- Use the host-selected session mode and exposed tools rather than reconstructing state from `.ferrus/`
- In managed MCP sessions, use wait/heartbeat tools; Nano hosts own those operations
- Direct HQ and standalone sessions have no managed task to claim or submit; validate with their exposed tools
- Use exposed validation tools during development; managed work uses `check` for durable receipts
- For managed work, run `check` immediately before `submit`; final response text is not submission
- Read Ferrus resources when exposed; Nano uses native context and `load_instructions`
- Ferrus owns managed Git staging and integration; direct sessions change Git state only when the user requests it
- For managed consultation, use the exposed consultation context/template when available

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
