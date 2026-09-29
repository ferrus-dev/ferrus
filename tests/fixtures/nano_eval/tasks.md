# Nano headless evaluation tasks

Each `cases/<id>/` directory is the entire starting Git tree. `suite.json` pins its
`git write-tree` object ID. Use one fresh Ferrus project and database per sample.
Do not put this task file in the starting tree. `suite.json` is the
machine-readable workload. The task artifact must contain only the task text
below plus one final newline. Keep the configured check command identical
across variants for a given case.

| Case | Task intent | Check command |
| --- | --- | --- |
| `local_bug_fix` | Fix `clamp` so values within the interval stay unchanged and out-of-range values reach the nearest bound. | `cargo test` |
| `cross_file_rust` | Make `greeting` trim surrounding whitespace and use `guest` for an empty or whitespace-only name. Preserve the module boundary. | `cargo test` |
| `refactor` | Rename the public capacity accessor to `capacity_hint` across the crate, preserving its behavior. | `cargo test` |
| `config_docs` | Change the default retry count to three and update the documented policy to match. | `cargo test` |
| `unsupported_language` | Fix the Python average function so it preserves fractional results. Use workspace evidence when repository graph coverage is unavailable. | `python3 -m unittest` (Windows: `python -m unittest`) |
| `graph_disabled` | Fix the parity predicate while repository graph and native context retrieval are disabled. | `cargo test` |
| `stale_context` | Inspect the existing status implementation, then update it to recognize 201 as `created`. After editing, re-read or refresh before relying on earlier repository evidence. | `cargo test` |

The initial checks fail by design. A successful attempt needs the configured
Ferrus submit check gate, independent Reviewer approval, and a `complete` task
state. Record rejected and failed attempts too. The checked-in tests verify
the pinned starting trees; provider-dependent runs are opt-in.
