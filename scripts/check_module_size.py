#!/usr/bin/env python3
"""Check the repository's Rust module size limit, including test sources."""

from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
MAX_LINES = 1000
SOURCE_ROOTS = ("src", "tests", "benches", "examples")


def main() -> int:
    oversized = []
    for directory in SOURCE_ROOTS:
        for path in sorted((ROOT / directory).rglob("*.rs")):
            lines = len(path.read_text(encoding="utf-8").splitlines())
            if lines > MAX_LINES:
                oversized.append((path.relative_to(ROOT), lines))

    for path, lines in oversized:
        print(f"{path}: {lines} lines (limit: {MAX_LINES}); split by responsibility")

    if oversized:
        return 1

    print(f"Rust module size check passed (limit: {MAX_LINES} lines)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
