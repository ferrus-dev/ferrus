# Binary size measurements

These are local development measurements for issue #71, not cross-platform size or performance guarantees.
Measurements use macOS arm64 and Rust 1.98.0. File sizes include the linked executable, without compression.

## Measurements recorded 2026-09-05

| Build | Bytes | MiB |
|---|---:|---:|
| Original release | 22,972,720 | 21.91 |
| Release with ThinLTO, one codegen unit, stripping, and unified sha2 | 14,411,904 | 13.74 |
| Original dist (`z`) | 10,918,704 | 10.41 |
| Dist (`z`) with unified sha2, default features | 10,920,208 | 10.41 |
| Dist settings with `s`, unified sha2, default features | 12,992,608 | 12.39 |
| Dist (`z`) with unified sha2, no default features | 9,576,720 | 9.13 |

The final release build is 37.3% smaller than the original release build. Disabling update notifications reduces
the final `dist` executable by another 12.3%. Standard builds continue to include update notifications.
The `s` variant is 19.0% larger than `z`, so `dist` retains `z` for compact release assets and installations.

| Graph operation, 302-file fixture | `z` | `s` |
|---|---:|---:|
| Cold indexing | 116.57 ms | 102.28 ms |
| No-op indexing | 31.098 ms | 28.501 ms |
| One-file change | 92.077 ms | 79.320 ms |
| Exact symbol search | 351.58 us | 284.53 us |

These quick runs show the tradeoff: `s` was faster on this fixture but added almost 2 MiB. Both profiles passed
the benchmark's graph reuse and search invariants. Keep `release` at optimization level 3 for ordinary Cargo
installations and reserve `dist` for users and release assets that prioritize file size.

## Cargo installation profile

The `release` profile uses ThinLTO, one codegen unit, and symbol stripping, including ordinary `cargo install`.
It retains Cargo's default `opt-level = 3`. The `dist` profile inherits these settings and optimizes for size.
Panic unwinding remains enabled. ThinLTO and one codegen unit trade additional build/link work for smaller
executables; stripped release files omit symbol names used in native debugging.

## Reproduce

```sh
cargo build --locked --release
cargo build --locked --profile dist
cargo build --locked --profile dist --no-default-features
cargo build --locked --profile dist --config 'profile.dist.opt-level="s"'

cargo bench --locked --profile dist --bench repository_graph -- --quick
cargo bench --locked --profile dist --config 'profile.dist.opt-level="s"' --bench repository_graph -- --quick
```

Copy each executable before the next build overwrites it. Compare `s` and `z` with the same dependency lockfile,
features, LTO, stripping, and codegen-unit settings. The graph benchmark creates the same 302-file fixture for
each profile; fixture setup and initial source discovery are outside measured indexing iterations. Quick runs
are smoke measurements with too few samples to establish small performance differences.

## Optional update notifications

The default `update-check` feature enables the existing HQ update notification. Disabling default features
omits its task, cache access, `ureq` client, and TLS dependency tree. CLI commands, orchestration, and graph/memory
operations remain available. This option does not disable networking performed by agent executables.

```sh
cargo install ferrus --locked --profile dist --no-default-features
```

CI runs clippy and tests with both default features and `--no-default-features` on Linux, macOS, and Windows.

## SHA-256 dependency consolidation

Ferrus now uses `sha2` 0.11, matching `neva`. This removes the old `sha2`, `digest`, `block-buffer`,
`crypto-common`, `cpufeatures`, `generic-array`, and `version_check` packages from the lockfile. No unrelated
package versions were updated. On identical `dist --no-default-features` builds, the executable changed from
9,576,336 to 9,576,720 bytes (+384 bytes), so this cleanup provides no meaningful linked-size saving.
