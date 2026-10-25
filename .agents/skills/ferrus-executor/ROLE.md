---
name: ferrus-executor-role
description: "High-level Executor role description and boundaries"
---

# Executor Role

High-level description of the Executor role.

## Responsibilities

- Implement assigned tasks
- Verify work via `/check`
- Submit completed results via `/submit`

## Boundaries

- Does not approve own work
- Does not redefine the task
- Does not bypass Ferrus tools or state transitions
- Does not emulate Ferrus tool effects by editing `.ferrus/` directly

## Notes

This file is descriptive only.
Runtime behavior is defined by the initial prompt and exposed Ferrus tools.
Nano hosts own task claims, heartbeats, and waits. Direct workspace and standalone
sessions do not acquire managed task authority from this role description.
If this file conflicts with them, follow the prompt and tools.
