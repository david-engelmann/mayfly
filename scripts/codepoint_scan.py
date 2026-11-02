#!/usr/bin/env python3
"""Scan repo text files for em dash and en dash codepoints.

Counts U+2014 and U+2013 with a real Python codepoint scan and
exits 1 when either count is nonzero. Directories are excluded by
name during the walk below the scan root, so the name of an
ancestor of the root can never exclude the root itself, and the
excluded names match the generated and dependency directories in
the repo gitignore plus the tool caches. Every other file in the
tree is scanned, whatever its name or suffix, apart from a short
list of known binary suffixes, and a file that does not decode as
UTF-8 fails the scan loudly instead of passing unseen.
"""
import os
import sys
from pathlib import Path

EM_DASH = "\u2014"
EN_DASH = "\u2013"
BINARY_SUFFIXES = {".png", ".jpg", ".jpeg", ".gif", ".ico", ".webp", ".pdf", ".zip", ".gz", ".tar", ".whl", ".so", ".dylib"}
EXCLUDED_DIRS = {".git", "__pycache__", ".pytest_cache", ".venv", "venv", "node_modules", "build", "dist"}


def iter_files(root):
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(d for d in dirnames if d not in EXCLUDED_DIRS)
        for name in sorted(filenames):
            path = Path(dirpath) / name
            if path.suffix.lower() not in BINARY_SUFFIXES:
                yield path


class ScanError(Exception):
    pass


def scan(root):
    em_count = 0
    en_count = 0
    for path in iter_files(root):
        try:
            text = path.read_text(encoding="utf-8")
        except UnicodeDecodeError as exc:
            raise ScanError(f"cannot decode {path} as UTF-8, {exc}") from exc
        file_em = text.count(EM_DASH)
        file_en = text.count(EN_DASH)
        if file_em or file_en:
            print(f"{path}: em dash {file_em}, en dash {file_en}")
        em_count += file_em
        en_count += file_en
    return em_count, en_count


def main():
    root = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).resolve().parent.parent
    try:
        em_count, en_count = scan(root)
    except ScanError as exc:
        print(f"scan error, {exc}", file=sys.stderr)
        return 2
    print(f"em dash U+2014 count {em_count}")
    print(f"en dash U+2013 count {en_count}")
    return 1 if em_count or en_count else 0


if __name__ == "__main__":
    raise SystemExit(main())
