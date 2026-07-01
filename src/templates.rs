pub const SPEC_TEMPLATE: &str = r#"# <Feature Name> Specification

## Summary
Briefly describe the feature and the user or system outcome it should create.

## Goals
- ...

## Non-Goals
- ...

## Context
Relevant repository, product, architectural, or workflow context.

## Requirements
- ...

## Milestones
Milestones must be ordered for execution:
- prerequisites first
- simpler enabling work before dependent work
- later milestones may depend on earlier completed milestones
- independent milestones should be marked as such

Each milestone must declare dependencies:
- Depends on: none
- Depends on: #1.0, #1.1

- [ ] #1.0 ...
  - ID: m1.0
  - Depends on: none

- [ ] #1.1 ...
  - ID: m1.1
  - Depends on: #1.0

- [ ] #1.2 ...
  - ID: m1.2
  - Depends on: #1.0

## Acceptance Criteria
- ...

## Risks and Open Questions
- ...
"#;

pub const TASK_TEMPLATE: &str = r#"# <Task Title>

## Context
Relevant repository, feature, spec, milestone, or prior review context.

## Objective
Describe the concrete outcome the Executor should produce.

## Requirements
- ...

## Acceptance Criteria
- ...

## Notes
Anything the Executor should know before starting.
"#;
