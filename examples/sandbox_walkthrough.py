#!/usr/bin/env python3
"""Standalone standard-library CLI walkthrough for mayfly.

Exercises version inspection, pre-prepare bound refusal (exit 2), pre-prepare
read-only bind validation (exit 125 with JSON RunReport), and on Linux hosts
permitting unprivileged user namespaces a full isolated sandbox task with
read-only input binding and environment scrubbing.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


def resolve_mayfly_bin(explicit: str | None) -> str:
    if explicit:
        return explicit
    env_bin = os.environ.get("CARGO_BIN_EXE_mayfly")
    if env_bin:
        return env_bin
    repo_root = Path(__file__).resolve().parent.parent
    for profile in ("debug", "release"):
        candidate = repo_root / "target" / profile / "mayfly"
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return str(candidate)
    found = shutil.which("mayfly")
    if found:
        return found
    raise RuntimeError(
        "mayfly binary not found; build with `cargo build --bin mayfly` or pass --mayfly-bin"
    )


def is_host_namespace_refusal(report: dict[str, object]) -> bool:
    err = str(report.get("error") or "")
    stderr_tail = str(report.get("stderr_tail") or "")
    combined = f"{err}\n{stderr_tail}".lower()
    return "uid_map" in combined or (
        "unshare" in combined
        and ("not permitted" in combined or "permission denied" in combined)
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="sandbox_walkthrough.py",
        description="Run a standalone CLI disposable-sandbox walkthrough with mayfly.",
    )
    parser.add_argument(
        "--mayfly-bin",
        default=None,
        help="Path to the mayfly binary (defaults to CARGO_BIN_EXE_mayfly, target/debug, or PATH)",
    )
    ns = parser.parse_args(argv)
    mayfly_bin = resolve_mayfly_bin(ns.mayfly_bin)

    # 1. Verify --version
    ver = subprocess.run(
        [mayfly_bin, "--version"],
        stdin=subprocess.DEVNULL,
        capture_output=True,
        text=True,
        check=False,
    )
    if ver.returncode != 0 or not ver.stdout.startswith("mayfly "):
        raise RuntimeError(f"unexpected --version output: {ver.stdout!r} {ver.stderr!r}")
    print("1.", ver.stdout.strip())

    with tempfile.TemporaryDirectory(prefix="mayfly-walkthrough-") as tmp:
        tmp_dir = Path(tmp)
        run_root = tmp_dir / "runs"
        run_root.mkdir()

        # 2. Pre-prepare CLI bound floor refusal (--timeout 0 -> exit 2, no report)
        bad_timeout = subprocess.run(
            [
                mayfly_bin,
                "run",
                "--root",
                str(run_root),
                "--timeout",
                "0",
                "--",
                "/bin/echo",
                "unreachable",
            ],
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            check=False,
        )
        assert bad_timeout.returncode == 2, bad_timeout
        assert bad_timeout.stdout == "", bad_timeout.stdout
        assert list(run_root.glob("run-*")) == []
        print("2. --timeout 0 refused at CLI parse (exit 2)")

        # 3. Pre-prepare --ro-bind missing source refusal (exit 125 with JSON RunReport)
        missing_src = tmp_dir / "does-not-exist.txt"
        bad_bind = subprocess.run(
            [
                mayfly_bin,
                "run",
                "--root",
                str(run_root),
                "--ro-bind",
                f"{missing_src}:/inputs/item.txt",
                "--",
                "/bin/cat",
                "/inputs/item.txt",
            ],
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            check=False,
        )
        assert bad_bind.returncode == 125, bad_bind
        bind_report = json.loads(bad_bind.stdout)
        assert bind_report["outcome"] == "harness_error", bind_report
        assert list(run_root.glob("run-*")) == []
        print("3. missing --ro-bind source refused with harness_error (exit 125)")

        # 4. On Linux, run an isolated task with --ro-bind and --allow-env
        if sys.platform.startswith("linux"):
            host_input = tmp_dir / "greeting.txt"
            host_input.write_text("hello from host ro-bind\n", encoding="utf-8")
            env = dict(os.environ)
            env["DEMO_ALLOWED"] = "SYNTHETIC-TEST-VALUE-allowed"
            env["DEMO_SCRUBBED"] = "SYNTHETIC-TEST-VALUE-scrubbed"
            live = subprocess.run(
                [
                    mayfly_bin,
                    "run",
                    "--root",
                    str(run_root),
                    "--timeout",
                    "10",
                    "--ro-bind",
                    f"{host_input}:/inputs/greeting.txt",
                    "--allow-env",
                    "DEMO_ALLOWED",
                    "--allow-env",
                    "PATH",
                    "--",
                    "/bin/sh",
                    "-c",
                    'test -n "$DEMO_ALLOWED" && test -z "$DEMO_SCRUBBED" && cat /inputs/greeting.txt',
                ],
                stdin=subprocess.DEVNULL,
                capture_output=True,
                text=True,
                env=env,
                check=False,
            )
            live_report = json.loads(live.stdout)
            if live_report["outcome"] == "task":
                assert live.returncode == 0, live_report
                assert live_report["task_exit_code"] == 0, live_report
                assert live_report["sandbox_removed"] is True, live_report
                assert live_report["stdout_tail"] == "hello from host ro-bind\n", live_report
                print("4. Linux sandbox task succeeded and incinerated directory")
            elif is_host_namespace_refusal(live_report):
                print("4. host restricted unprivileged user namespaces:", live_report["error"])
            else:
                raise RuntimeError(f"unexpected Linux sandbox failure: {live_report!r}")
        else:
            print("4. non-Linux host detected; pre-prepare CLI checks verified")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
