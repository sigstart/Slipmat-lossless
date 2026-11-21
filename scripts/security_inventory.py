#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Miguel Rincon
# SPDX-License-Identifier: GPL-3.0-or-later
"""Collect a path and metadata inventory without opening file payloads."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys


SKIP_DIRS = {
    ".git", ".agents", ".codex", ".aws", "node_modules", "target",
    "dist", "build", "out", "vendor", ".venv", "venv", "__pycache__",
}
SOURCE_SUFFIXES = {
    ".rs", ".js", ".mjs", ".cjs", ".ts", ".tsx", ".jsx", ".py",
    ".c", ".h", ".cc", ".cpp", ".hpp", ".go",
    ".sh", ".bash", ".ps1", ".html", ".css", ".scss", ".xml", ".yml", ".yaml",
}
SOURCE_NAMES = {"makefile", "pkgbuild", "slipmat-launcher", ".npmrc"}
BINARY_SUFFIXES = {
    ".so", ".dll", ".dylib", ".bin", ".exe", ".wasm", ".deb", ".rpm",
    ".flatpak", ".appimage", ".png", ".jpg", ".jpeg", ".webp", ".gif",
    ".ico", ".woff", ".woff2", ".ttf", ".otf", ".mp3", ".flac", ".m4a", ".run",
}
MANIFEST_NAMES = {
    "cargo.toml", "cargo.lock", "package.json", "package-lock.json",
    "npm-shrinkwrap.json", "yarn.lock", "pnpm-lock.yaml", "pyproject.toml",
    "requirements.txt", "go.mod", "go.sum", "gemfile", "gemfile.lock",
    "composer.json", "composer.lock", "flake.lock", "poetry.lock", "go.work",
    "cargo-sources.json",
}
SECRET_NAMES = {
    ".env", ".env.local", ".env.production", "id_rsa", "id_ed25519",
    "credentials", "credentials.json", "secrets.json", "secrets.yaml",
    "secret.key", "private.key",
}
SECRET_SUFFIXES = {".pem", ".key", ".p12", ".pfx", ".credentials"}
LARGE_BYTES = 1024 * 1024


def is_secret_name(name: str) -> bool:
    lowered = name.lower()
    return (
        lowered in SECRET_NAMES
        or (lowered.startswith(".env") and lowered != ".env.example")
        or lowered.endswith(tuple(SECRET_SUFFIXES))
        or any(word in lowered for word in ("credential", "private_key", "secret", "token", "password"))
    )


def excluded_parts(parts: tuple[str, ...]) -> bool:
    return any(part in SKIP_DIRS or is_secret_name(part) for part in parts)


def relative(path: Path, root: Path) -> str:
    return path.relative_to(root).as_posix() or "."


def git_result(repo: Path, args: list[str]) -> subprocess.CompletedProcess[bytes]:
    env = os.environ.copy()
    env.update({"GIT_OPTIONAL_LOCKS": "0", "GIT_TERMINAL_PROMPT": "0"})
    return subprocess.run(
        ["git", "-c", "core.fsmonitor=false", *args], cwd=repo, env=env,
        stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=8, check=False,
    )


def git_info(repo: Path, root: Path, errors: list[str]) -> dict[str, object]:
    info: dict[str, object] = {
        "path": relative(repo, root), "revision": None, "dirty_paths": None,
        "worktree_status": "not_checked",
        "worktree_status_reason": "git status may invoke configured filters; payload-safe mode skips it",
    }
    try:
        rev = git_result(repo, ["rev-parse", "--verify", "HEAD"])
        if rev.returncode:
            errors.append(f"git revision unavailable for {relative(repo, root)}")
        else:
            info["revision"] = rev.stdout.decode("ascii", errors="replace").strip()
    except (OSError, subprocess.TimeoutExpired):
        errors.append(f"git inspection failed for {relative(repo, root)}")
    return info


def collect(root: Path, output: Path | None) -> dict[str, object]:
    errors: list[str] = []
    symlink_count = 0
    invalid_git_markers = 0
    repos: set[Path] = set()
    entries: list[tuple[Path, int, int]] = []
    excluded_counts: dict[str, int] = {}
    output_absolute = Path(os.path.abspath(output)) if output else None

    def walk(directory: Path) -> None:
        nonlocal symlink_count, invalid_git_markers
        try:
            children = sorted(os.scandir(directory), key=lambda item: item.name)
        except OSError:
            errors.append(f"directory could not be read: {relative(directory, root)}")
            return
        for child in children:
            path = Path(child.path)
            if child.is_symlink():
                symlink_count += 1
                continue
            if child.name == ".git" and child.is_dir(follow_symlinks=False):
                head = path / "HEAD"
                if not head.is_symlink() and head.is_file():
                    repos.add(directory)
                else:
                    invalid_git_markers += 1
                continue
            if child.name == ".git" and child.is_file(follow_symlinks=False):
                repos.add(directory)
                continue
            if is_secret_name(child.name):
                excluded_counts["credentials and secret-like names"] = (
                    excluded_counts.get("credentials and secret-like names", 0) + 1
                )
                continue
            if child.is_dir(follow_symlinks=False):
                if child.name in SKIP_DIRS:
                    excluded_counts[child.name] = excluded_counts.get(child.name, 0) + 1
                    continue
                walk(path)
                continue
            if not child.is_file(follow_symlinks=False):
                continue
            if output_absolute is not None and path.absolute() == output_absolute:
                continue
            try:
                stat = child.stat(follow_symlinks=False)
            except OSError:
                errors.append(f"file metadata unavailable: {relative(path, root)}")
                continue
            entries.append((path, stat.st_size, stat.st_mode))

    # A worktree may have a .git file rather than a directory.
    try:
        if (root / ".git").is_file() and not (root / ".git").is_symlink():
            repos.add(root)
    except OSError:
        errors.append("root git metadata could not be checked")
    walk(root)

    repo_list = sorted(repos, key=lambda path: (len(path.parts), path.as_posix()))
    groups: dict[Path | None, dict[str, list[dict[str, object]]]] = {}
    for path, size, mode in entries:
        owner = max((repo for repo in repo_list if path.is_relative_to(repo)), key=lambda p: len(p.parts), default=None)
        group = groups.setdefault(owner, {"files": [], "source_files": [], "metadata_manifests": [], "candidates": []})
        record = {"path": relative(path, root), "size_bytes": size}
        group["files"].append(record)
        if path.suffix.lower() in SOURCE_SUFFIXES or path.name.lower() in SOURCE_NAMES:
            group["source_files"].append(record)
        lowered = path.name.lower()
        if lowered in MANIFEST_NAMES or lowered.endswith((".lock", ".sum")):
            group["metadata_manifests"].append(record)
        reasons = []
        if mode & 0o111:
            reasons.append("executable_bit")
        if size >= LARGE_BYTES:
            reasons.append("at_least_1_mib")
        if path.suffix.lower() in BINARY_SUFFIXES:
            reasons.append("binary_or_media_extension")
        if reasons:
            group["candidates"].append({**record, "reasons": reasons})

    repository_output = []
    for repo in repo_list:
        group = groups.get(repo, {"files": [], "source_files": [], "metadata_manifests": [], "candidates": []})
        for records in group.values():
            records.sort(key=lambda record: str(record["path"]))
        repository_output.append({**git_info(repo, root, errors), **group})
    outside = groups.get(None, {"files": [], "source_files": [], "metadata_manifests": [], "candidates": []})
    for records in outside.values():
        records.sort(key=lambda record: str(record["path"]))

    return {
        "schema_version": 1,
        "root": str(root),
        "complete": not errors,
        "repositories": repository_output,
        "files_outside_repositories": outside,
        "exclusions": {
            "policy": [
                "Never follows symlinks; symlinks are counted and skipped.",
                "Skips VCS/agent/config, dependency, virtualenv, and build output directories.",
                "Skips credential and secret-like names; excluded names are not emitted.",
                "No file payloads are read; binary classification uses names, size, and mode only.",
                "Git LFS pointer detection was not performed because contents are not read.",
            ],
            "directory_counts": excluded_counts,
            "symlinks_skipped": symlink_count,
            "invalid_git_markers_skipped": invalid_git_markers,
        },
        "errors": errors,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[2])
    parser.add_argument("--output", type=Path, help="write JSON here; refuses to overwrite an existing file")
    args = parser.parse_args()
    root = args.root.resolve()
    if not root.is_dir():
        parser.error("--root must name an existing directory")
    output = Path(os.path.abspath(args.output)) if args.output else None
    if output and os.path.lexists(output):
        parser.error("--output already exists; choose a new path")
    if output:
        for parent in (output.parent, *output.parent.parents):
            if parent.is_symlink():
                parser.error("--output cannot use a symlinked parent directory")
    result = collect(root, output)
    rendered = json.dumps(result, indent=2, ensure_ascii=True) + "\n"
    if output:
        try:
            output.parent.mkdir(parents=True, exist_ok=True)
            with output.open("x", encoding="utf-8") as stream:
                stream.write(rendered)
        except OSError:
            print("could not write requested output", file=sys.stderr)
            return 2
    else:
        sys.stdout.write(rendered)
    return 0 if result["complete"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
