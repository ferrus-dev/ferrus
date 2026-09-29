"""Copy one pinned Nano evaluation case into a fresh Git repository."""

import json
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path


def git(directory, *args):
    return subprocess.check_output(
        ["git", *args], cwd=directory, text=True, stderr=subprocess.PIPE
    ).strip()


def main():
    if len(sys.argv) != 3:
        raise SystemExit("usage: prepare.py CASE NEW_DIRECTORY")
    fixture = Path(__file__).resolve().parent
    case, destination = sys.argv[1], Path(sys.argv[2]).resolve()
    suite = json.loads((fixture / "suite.json").read_text(encoding="utf-8"))
    if case not in suite["cases"]:
        raise SystemExit(f"unknown case: {case}")
    if destination.exists():
        raise SystemExit(f"destination already exists: {destination}")
    shutil.copytree(fixture / "cases" / case, destination)
    git(destination, "init", "-q", "--object-format=sha1")
    git(destination, "config", "core.autocrlf", "false")
    git(destination, "add", "-A")
    tree = git(destination, "write-tree")
    if tree != suite["cases"][case]:
        raise SystemExit(f"fixture tree changed: {tree}")
    # An empty hooks path disables pre- and post-commit hooks from host config.
    with tempfile.TemporaryDirectory(prefix="nano-eval-hooks-") as hooks:
        git(
            destination,
            "-c", f"core.hooksPath={hooks}",
            "-c", "user.name=Nano Eval",
            "-c", "user.email=nano-eval@example.invalid",
            "-c", "commit.gpgsign=false",
            "commit", "-q", "-m", f"nano eval {case}",
        )
    if git(destination, "rev-parse", "HEAD^{tree}") != tree:
        raise SystemExit("committed fixture tree changed")
    if git(destination, "status", "--porcelain=v1", "--untracked-files=all"):
        raise SystemExit("prepared fixture workspace is not clean")
    print(json.dumps({
        "case_id": case,
        "start_tree": tree,
        "directory": str(destination),
        "task_artifact": suite["workloads"][case]["task"] + "\n",
        "checks": suite["workloads"][case]["checks"],
        "windows_checks": suite["workloads"][case].get("windows_checks"),
    }))


if __name__ == "__main__":
    main()
