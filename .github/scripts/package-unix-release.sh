#!/usr/bin/env bash
set -euo pipefail

: "${TARGET:?release target is required}"
: "${ARCHIVE:?release archive name is required}"

mkdir -p dist
staging="dist/ferrus-${TARGET}"
mkdir -p "${staging}"
cp "target/${TARGET}/dist/ferrus" "${staging}/ferrus"
cp "target/${TARGET}/dist/ferrus-nano" "${staging}/ferrus-nano"
cp README.md LICENSE NOTICE "${staging}/"
tar -C dist -czf "dist/${ARCHIVE}" "ferrus-${TARGET}"
(
  cd dist
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "${ARCHIVE}" > "${ARCHIVE}.sha256"
  else
    shasum -a 256 "${ARCHIVE}" > "${ARCHIVE}.sha256"
  fi
)
