#!/usr/bin/env python3
"""Scan repo text files for em dash and en dash codepoints.

Counts U+2014 and U+2013 with a real Python codepoint scan and
exits 1 when either count is nonzero.
"""

import sys
from pathlib import Path

EM_DASH = "\u2014"
EN_DASH = "\u2013"
SUFFIXES = {".md", ".txt", ".py", ".yml", ".toml"}
SKIP_DIRS = {".git", "__pycache__"}


def iter_files(root):
    for path in sorted(Path(root).rglob("*")):
        if any(part in SKIP_DIRS for part in path.parts):
            continue
        if path.is_file() and path.suffix in SUFFIXES:
            yield path


def scan(root):
    em_count = 0
    en_count = 0
    for path in iter_files(root):
        text = path.read_text(encoding="utf-8", errors="ignore")
        em_count += text.count(EM_DASH)
        en_count += text.count(EN_DASH)
    return em_count, en_count


def main():
    root = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).resolve().parent.parent
    em_count, en_count = scan(root)
    print(f"em dash U+2014 count {em_count}")
    print(f"en dash U+2013 count {en_count}")
    return 1 if em_count or en_count else 0


if __name__ == "__main__":
    raise SystemExit(main())
