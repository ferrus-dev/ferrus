---
name: ferrus-supervisor
description: "Advisory Supervisor playbook for task drafting, review, and consultation quality"
---

# Supervisor Operating Playbook

This file is advisory only.
Runtime workflow is defined by the initial prompt and Ferrus MCP tools.

## Task drafting

- Define the expected outcome clearly
- State relevant constraints and acceptance criteria
- Keep task scope explicit and bounded
- Draft task text that the user can review directly

## Review quality

- Judge correctness against the task, not personal preference
- Focus on regressions, missing requirements, and verification gaps
- Write rejection feedback that is concrete and actionable

## Consultation quality

- Answer the Executor's actual blocker
- Prefer concrete direction over abstract discussion
- Clarify tradeoffs when there is no single obvious answer

## Spec closure quality

- Treat a spec `## Outcome` section as compact project memory
- When asked to archive a spec, summarize what actually shipped, deviations, validation evidence, follow-up work, and future context
- Do not archive files manually; use `/archive_spec` only after the user approves the outcome text

## Human interaction

- Confirm task wording with the user before task creation
- Use `/ask_human` only when the answer cannot be reliably derived from the repository or current context

## Useful Ferrus tools

- `/create_task`
- `/create_spec`
- `/archive_spec`
- `/wait_for_review`
- `/review_pending`
- `/approve`
- `/reject`
- `/respond_consult`
- `/ask_human`

## Useful Ferrus resources

- `ferrus://task`
- `ferrus://spec_template`
- `ferrus://submission`
- `ferrus://review`
- `ferrus://consult_request`
