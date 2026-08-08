# Contributing to ferrus

Thanks for contributing to `ferrus`.

`ferrus` is an AI agent orchestrator for software projects. The project is still evolving,
so the main goal for contributions is not just to add features, but to keep the core runtime
predictable, debuggable, and easy to extend.

## Contributor License Agreement

All contributors must accept the Ferrus Individual Contributor
License Agreement before their pull request can be merged.

The agreement applies to both `ferrus-dev/ferrus` and
`ferrus-dev/ferrus-platform`. You only need to accept each version
of the agreement once.

The CLA Assistant will provide a signing link in your first pull
request.

If your employer or another legal entity owns or controls your
contribution, do not sign the Individual CLA. Contact the maintainer
regarding a Corporate Contributor License Agreement.

## Before you start

- Read [README.md](README.md) for the current project model and CLI behavior.
- Keep changes focused. Small, reviewable pull requests are much easier to merge than broad refactors.
- If the change is architectural or likely to affect orchestration semantics, open an issue or discussion first.

## Development setup

You need a working Rust toolchain.

Build the project:

```sh
cargo build
```

Run the full local verification suite before submitting changes:

```sh
cargo fmt --check
cargo clippy -- -D warnings
cargo test
```

All three checks are expected to pass.

## What to contribute

Good contributions include:

- bug fixes with a clear behavioral scope;
- tests that lock in expected runtime behavior;
- improvements to HQ, orchestration reliability, and state handling;
- documentation updates that clarify how `ferrus` works;
- platform compatibility improvements, especially where behavior is currently Unix-centric.

Contributions are less likely to be accepted when they:

- add complexity without a clear payoff in reliability or usability;
- hard-code assumptions that make future storage or orchestration work harder;
- expand the public surface area without enough tests or documentation.

## Coding expectations

- Follow the existing code style and structure.
- Prefer explicit, boring solutions over clever ones in orchestration code.
- Preserve backwards-compatible behavior where practical, or document intentional breakage clearly.
- Add or update tests when changing behavior.
- Keep logs, errors, and state transitions understandable to a human operator.

## Pull requests

When opening a pull request:

- explain the problem being solved;
- describe the approach and any important tradeoffs;
- mention any follow-up work or known limitations;
- include tests, or explain why tests were not added.

If your change affects user-facing behavior, update documentation in the same pull request.

## Issues

When filing an issue, include:

- what you expected to happen;
- what happened instead;
- how to reproduce it;
- environment details when relevant, especially OS and Rust version.

## Code of conduct

By participating in this project, you agree to follow the rules in [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).

## Security

For security issues, do not open a public issue first. Follow the reporting guidance in [SECURITY.md](SECURITY.md).

## Licensing

By contributing to `ferrus`, you agree that your contributions will be licensed under the Apache License 2.0.
