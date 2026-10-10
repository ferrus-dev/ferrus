#!/usr/bin/env bash
set -euo pipefail

# Keep these identical to the release matrix and asset upload paths.
test "${TARGET:?release target is required}" = aarch64-apple-darwin
test "${ARCHIVE:?release archive name is required}" = ferrus-aarch64-apple-darwin.tar.gz

# Preserve Rust's current arm64 deployment target across runner/SDK upgrades.
export MACOSX_DEPLOYMENT_TARGET=11.0
cargo build --locked --profile dist --features nano-openai,nano-mcp --target "$TARGET"
bash .github/scripts/package-unix-release.sh

(
  cd dist
  shasum -a 256 -c "${ARCHIVE}.sha256"
)

smoke_root="$(mktemp -d)"
trap 'rm -rf "$smoke_root"' EXIT
mkdir -p "$smoke_root/unpacked" "$smoke_root/home" "$smoke_root/project"
tar -xzf "dist/${ARCHIVE}" -C "$smoke_root/unpacked"
package_dir="$smoke_root/unpacked/ferrus-${TARGET}"
bin_path="$package_dir/ferrus"
test -x "$bin_path"
test -x "$package_dir/ferrus-nano"
env HOME="$smoke_root/home" "$package_dir/ferrus-nano" --version
test -f "$package_dir/README.md"
test -f "$package_dir/LICENSE"
test -f "$package_dir/NOTICE"
for executable in "$bin_path" "$package_dir/ferrus-nano"; do
  file "$executable"
  test "$(lipo -archs "$executable")" = arm64
  minos="$(otool -l "$executable" | awk '$1 == "cmd" && $2 == "LC_BUILD_VERSION" { build_version = 1; next } build_version && $1 == "minos" { print $2; build_version = 0 }')"
  echo "Packaged macOS deployment target: $minos"
  test "$minos" = "$MACOSX_DEPLOYMENT_TARGET"
done

env HOME="$smoke_root/home" "$bin_path" --version
(
  cd "$smoke_root/project"
  env HOME="$smoke_root/home" "$bin_path" init
  env HOME="$smoke_root/home" "$bin_path" doctor
)
test -f "$smoke_root/project/ferrus.toml"
test -f "$smoke_root/project/.ferrus/project.toml"
test -d "$smoke_root/project/.agents/skills/ferrus"
# Confirm that init registered its runtime under the isolated home directory.
find "$smoke_root/home/.ferrus/projects" -name ferrus.db -type f | awk 'END { exit NR != 1 }'
