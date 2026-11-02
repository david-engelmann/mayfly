#!/usr/bin/env python3
"""The mayfly run janitor.

One janitor is spawned per run. It holds the read end of a pipe the
runner holds the write end of, and it watches for the runner to go
away. While the runner lives, the pipe stays open and the janitor
sleeps on it. When the runner finishes a run it removes the sandbox
itself, leaves a clean marker beside the run root, and closes the
pipe, and the janitor sees the marker and exits. When the runner
dies without removing the sandbox, the pipe closes with no marker
left, and the janitor removes the sandbox directory itself, so a
killed supervisor leaves no sandbox directory behind.

This file imports nothing from the mayfly package, because it runs
by path beside the runner and must stay usable even when the runner
died mid import.
"""

import os
import shutil
import sys
import time

RETRY_SECONDS = 30.0


def main(argv):
    opts = {}
    index = 0
    while index + 1 < len(argv):
        opts[argv[index]] = argv[index + 1]
        index += 2
    sandbox = opts["--sandbox"]
    marker = opts["--marker"]
    watch_fd = int(opts["--fd"])
    try:
        while True:
            chunk = os.read(watch_fd, 4096)
            if not chunk:
                break
    except OSError:
        pass
    try:
        os.close(watch_fd)
    except OSError:
        pass
    if os.path.exists(marker):
        try:
            os.unlink(marker)
        except OSError:
            pass
        return 0
    deadline = time.monotonic() + RETRY_SECONDS
    while True:
        shutil.rmtree(sandbox, ignore_errors=True)
        if not os.path.exists(sandbox):
            return 0
        if time.monotonic() >= deadline:
            return 0
        time.sleep(0.1)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
