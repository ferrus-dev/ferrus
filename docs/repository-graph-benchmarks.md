# Repository Graph Local Baselines

These measurements are development baselines, not release performance guarantees. They are intended to expose
accidental full re-extraction, stale incremental behavior, or unbounded query regressions. Compare relative
behavior and fact counts before comparing wall-clock values across machines.

## Reproducing the medium fixture

The ignored test generates a non-Git Cargo repository with 300 Rust modules plus `Cargo.toml` and `src/lib.rs`.
It measures a cold build, a no-op build, a one-file change, and indexed symbol search:

```sh
cargo test repository_graph::benchmarks::medium_fixture_baseline -- --ignored --nocapture --test-threads=1
```

The harness asserts the important invariants: the no-op build parses zero files, the changed build parses exactly
one file, every other fragment is reused, and search returns the changed symbol.

## Baseline recorded 2026-07-14

Environment: macOS arm64, Rust 1.96.0, debug test/build profile. Timings include coordinator and SQLite work but
exclude compilation.

| Corpus / operation | Time | Parsed | Reused | Result size |
|---|---:|---:|---:|---:|
| Medium fixture cold build (302 files) | 378 ms | 302 | 0 | 1,816 nodes / 2,115 edges |
| Medium fixture no-op build | 203 ms | 0 | 302 | same snapshot |
| Medium fixture one-file change | 293 ms | 1 | 301 | new snapshot |
| Medium fixture indexed symbol search | 1.12 ms | — | — | 1 hit |
| Ferrus dogfood cold build (134 files) | 880 ms | 134 | 0 | 4,015 nodes / 4,803 edges |
| Ferrus dogfood no-op build | 303 ms | 0 | 134 | same snapshot and publication generation |

The Ferrus run used the dirty canonical workspace, included non-ignored untracked files, processed 2,054,529
source bytes on the cold build and zero source bytes on the no-op build, and persisted 351 bounded diagnostics.
`graph status`, `search`, `show`, and `neighbors` were then exercised against the published snapshot; search found
`RuntimeTaskContext` in `src/project.rs` with its exact evidence span and provenance.
