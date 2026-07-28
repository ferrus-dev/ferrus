# Repository Graph Local Baselines

These measurements are development baselines, not release performance guarantees. They are intended to expose
accidental full re-extraction, stale incremental behavior, or unbounded query regressions. Compare relative
behavior and fact counts before comparing wall-clock values across machines.

## Reproducing the medium fixture

The Criterion target in `benches/repository_graph.rs` generates a non-Git Cargo repository with 300 Rust modules
plus `Cargo.toml` and `src/lib.rs`. It measures a cold build, a no-op build, a one-file change, and indexed symbol
search using the optimized benchmark profile:

```sh
cargo bench --bench repository_graph
```

Use `cargo bench --bench repository_graph -- --quick` for a fast smoke run. Criterion setup prepares a fresh
fixture and sidecar outside each measured indexing iteration, so filesystem generation, source discovery, and any
prerequisite build are excluded from the reported operation. Before measuring, the harness asserts the important
invariants: the no-op build parses zero files, the changed build parses exactly one file, every other fragment is
reused, and search returns the changed symbol.

## Criterion baseline recorded 2026-07-15

Environment: macOS arm64, Rust 1.96.0, optimized benchmark profile, Criterion 0.8.2 `--quick`. Values are Criterion
point estimates; compilation and per-iteration setup are excluded.

| Corpus / operation | Time | Parsed | Reused | Result size |
|---|---:|---:|---:|---:|
| Medium fixture cold build (302 files) | 97.0 ms | 302 | 0 | 1,816 nodes / 2,115 edges |
| Medium fixture no-op build | 49.1 ms | 0 | 302 | same snapshot |
| Medium fixture one-file change | 84.0 ms | 1 | 301 | new snapshot |
| Medium fixture indexed symbol search | 238 us | -- | -- | 1 hit |

## Ferrus dogfood recorded 2026-07-14

These dogfood timings used the debug CLI build and are retained as functional evidence, not as values to compare
directly with the Criterion release-profile baseline.

| Corpus / operation | Time | Parsed | Reused | Result size |
|---|---:|---:|---:|---:|
| Ferrus dogfood cold build (134 files) | 880 ms | 134 | 0 | 4,015 nodes / 4,803 edges |
| Ferrus dogfood no-op build | 303 ms | 0 | 134 | same snapshot and publication generation |

The Ferrus run used the dirty canonical workspace, included non-ignored untracked files, processed 2,054,529
source bytes on the cold build and zero source bytes on the no-op build, and persisted 351 bounded diagnostics.
`graph status`, `search`, `show`, and `neighbors` were then exercised against the published snapshot; search found
`RuntimeTaskContext` in `src/project.rs` with its exact evidence span and provenance.
