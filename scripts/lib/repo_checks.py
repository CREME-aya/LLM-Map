#!/usr/bin/env python3
"""Repository hygiene and cross-component integration checks.

Subcommands:
  check-case      Detect case-insensitive path collisions (critical on Case-sensitive APFS + macOS/Windows).
  check-json      Validate JSON syntax across repository JSON files.
  check-md-links  Validate relative Markdown file links.
  check-secrets   Lightweight scan for accidentally committed secrets or private keys.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
from collections import defaultdict
from pathlib import Path
from urllib.parse import unquote

IGNORED_DIRS = {
    ".git",
    "node_modules",
    "target",
    "build",
    ".gradle",
    "dist",
    ".venv",
    "venv",
    "__pycache__",
}

MD_LINK_RE = re.compile(r"!?\[[^\]]*\]\(([^)]+)\)")

SECRET_PATTERNS: list[tuple[str, re.Pattern[str]]] = [
    ("private_key_block", re.compile(r"-----BEGIN (?:RSA |EC |OPENSSH |DSA )?PRIVATE KEY-----")),
    ("openai_api_key", re.compile(r"\bsk-[A-Za-z0-9]{20,}\b")),
    ("google_api_key", re.compile(r"\bAIza[0-9A-Za-z_-]{35}\b")),
    ("aws_access_key", re.compile(r"\bAKIA[0-9A-Z]{16}\b")),
]


def walk_repo_files(root: Path) -> list[Path]:
    files: list[Path] = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(d for d in dirnames if d not in IGNORED_DIRS)
        for name in sorted(filenames):
            files.append(Path(dirpath) / name)
    return files


def check_case_collisions(root: Path) -> list[str]:
    buckets: dict[str, list[str]] = defaultdict(list)
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(d for d in dirnames if d not in IGNORED_DIRS)
        rel_dir = Path(dirpath).relative_to(root)
        for name in list(dirnames) + list(filenames):
            rel = (rel_dir / name).as_posix()
            buckets[rel.casefold()].append(rel)

    errors: list[str] = []
    for _, paths in sorted(buckets.items()):
        unique_paths = sorted(set(paths))
        if len(unique_paths) > 1:
            errors.append(f"Case collision: {' <-> '.join(unique_paths)}")
    return errors


def check_json_files(root: Path) -> tuple[int, list[str]]:
    checked = 0
    errors: list[str] = []
    for path in walk_repo_files(root):
        if path.suffix.lower() != ".json":
            continue
        checked += 1
        try:
            json.loads(path.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
            rel = path.relative_to(root).as_posix()
            errors.append(f"{rel}: {exc}")
    return checked, errors


def _strip_fenced_code(text: str) -> str:
    """Remove fenced code blocks so example markdown links inside code blocks are ignored."""
    lines = text.splitlines()
    out: list[str] = []
    in_fence = False
    fence_marker = ""
    for line in lines:
        stripped = line.lstrip()
        if not in_fence and (stripped.startswith("```") or stripped.startswith("~~~")):
            in_fence = True
            fence_marker = stripped[:3]
            continue
        if in_fence:
            if stripped.startswith(fence_marker):
                in_fence = False
            continue
        out.append(line)
    return "\n".join(out)


def check_markdown_links(root: Path) -> tuple[int, list[str]]:
    checked = 0
    errors: list[str] = []
    for path in walk_repo_files(root):
        if path.suffix.lower() != ".md":
            continue
        text = _strip_fenced_code(path.read_text(encoding="utf-8"))
        for match in MD_LINK_RE.finditer(text):
            raw_target = match.group(1).strip()
            if not raw_target:
                continue
            # Strip optional title part: path "title"
            target = raw_target.split()[0].strip("<>")
            if target.startswith(("#", "http://", "https://", "mailto:", "file://", "data:")):
                continue
            target_no_anchor = unquote(target.split("#", 1)[0])
            if not target_no_anchor:
                continue
            checked += 1
            resolved = (path.parent / target_no_anchor).resolve()
            if not resolved.exists():
                rel_src = path.relative_to(root).as_posix()
                errors.append(f"{rel_src}: broken relative link -> {target}")
    return checked, errors


def check_secrets(root: Path) -> tuple[int, list[str]]:
    checked = 0
    errors: list[str] = []
    for path in walk_repo_files(root):
        # Skip binary files and our own pattern definitions / unit tests
        rel = path.relative_to(root).as_posix()
        if rel in {"scripts/lib/repo_checks.py", "scripts/tests/test_scripts.py"}:
            continue
        if path.suffix.lower() in {".png", ".jpg", ".jpeg", ".gif", ".webp", ".so", ".apk", ".aab", ".zip"}:
            continue
        try:
            content = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        checked += 1
        for rule_name, pattern in SECRET_PATTERNS:
            if pattern.search(content):
                errors.append(f"{rel}: matched secret pattern '{rule_name}'")
    return checked, errors


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Repository hygiene checks")
    parser.add_argument("command", choices=["check-case", "check-json", "check-md-links", "check-secrets"])
    parser.add_argument("--root", type=Path, default=Path("."))
    args = parser.parse_args(argv)
    root = args.root.resolve()

    if args.command == "check-case":
        errors = check_case_collisions(root)
        if errors:
            for e in errors:
                print(f"[FAIL] {e}", file=sys.stderr)
            return 1
        print("[PASS] No case-insensitive path collisions detected.")
        return 0

    if args.command == "check-json":
        checked, errors = check_json_files(root)
        if errors:
            for e in errors:
                print(f"[FAIL] {e}", file=sys.stderr)
            return 1
        print(f"[PASS] Validated {checked} JSON file(s).")
        return 0

    if args.command == "check-md-links":
        checked, errors = check_markdown_links(root)
        if errors:
            for e in errors:
                print(f"[FAIL] {e}", file=sys.stderr)
            return 1
        print(f"[PASS] Verified {checked} relative Markdown link(s).")
        return 0

    if args.command == "check-secrets":
        checked, errors = check_secrets(root)
        if errors:
            for e in errors:
                print(f"[FAIL] {e}", file=sys.stderr)
            return 1
        print(f"[PASS] Secret scan clean across {checked} text file(s).")
        return 0

    return 2


if __name__ == "__main__":
    sys.exit(main())
