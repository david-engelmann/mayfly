#!/usr/bin/env python3
"""The mayfly run janitor.

One janitor is spawned per run. It holds the read end of a pipe the
runner holds the write end of, and it watches for the runner to go
away. The runner hands the janitor a per run token over its
standard input at spawn, a token that appears in no directory
listing and in no command line. While the runner lives, the pipe
stays open and the janitor sleeps on it. When the runner finishes
a run it removes the sandbox itself, leaves a clean marker beside
the run root carrying that token, and closes the pipe, and the
janitor sees the marker carries its token and exits. A marker
without the token was planted by someone else and stands nothing
down. When the runner dies without removing the sandbox, the pipe
closes with no genuine marker left, and the janitor removes the
sandbox directory itself, so a killed supervisor leaves no sandbox
directory behind. The removal can land while the run's init is
still dying, and the init then recreates the sandbox shape behind
the removal, so the janitor watches the removed path for a few
seconds afterwards and removes the directory again if it
reappears.

This file imports nothing from the mayfly package, because it runs
by path beside the runner and must stay usable even when the runner
died mid import.
"""

import os
import shutil
import sys
import time

RETRY_SECONDS = 30.0
WATCH_SECONDS = 5.0


def _read_token():
    """Read the per run token the runner wrote to standard input.

    The runner writes the token once at spawn and closes the
    stream, so this returns as soon as the line or the end of the
    stream arrives. A janitor whose runner died before writing
    reads an empty token, and an empty token authenticates nothing.
    """
    chunks = []
    try:
        while True:
            chunk = os.read(0, 4096)
            if not chunk:
                break
            chunks.append(chunk)
            if b"\n" in chunk:
                break
    except OSError:
        pass
    return b"".join(chunks).decode("utf-8", errors="replace").strip()


def _marker_carries_token(marker, token):
    """Whether the marker carries exactly this run's token.

    The marker is opened only where the marker path is a
    regular file, so a pipe or any other planted object at the
    path reads as a marker without the token instead of
    blocking the janitor inside its open while the sandbox
    stands. The check follows a symlink to the file it names,
    and a symlink is read through and never removed. The read
    itself cannot block and is bounded by a limit derived from
    the token length, and a marker that runs past the limit is
    not the clean marker whatever its first bytes say. Bytes
    that are not valid text decode to replacement characters
    and fail the comparison. A planted marker that is oversized
    or undecodable therefore reads as a marker without the
    token, and the sandbox is removed.
    """
    limit = len(token.encode("utf-8")) + 64
    try:
        if not os.path.isfile(marker):
            return False
    except OSError:
        return False
    try:
        fd = os.open(marker, os.O_RDONLY | os.O_NONBLOCK)
    except OSError:
        return False
    try:
        raw = os.read(fd, limit + 1)
    except OSError:
        return False
    finally:
        os.close(fd)
    if len(raw) > limit:
        return False
    return raw.decode("utf-8", errors="replace").strip() == token


def _watch_for_recreation(sandbox):
    """Remove the sandbox again if a dying init recreates it.

    The removal in main can land while the run's init is still
    dying, and the init then recreates the sandbox shape on the
    same path behind the removal, carrying no provenance the
    orphan sweep could trust. A run id is never reused, so a
    directory that reappears at this path inside the watch
    window can only be that dying init recreating its own
    tree, and never a directory a later live run created at a
    reused path. The watch is bounded, and the janitor leaves
    when the window ends whether or not the path stands clear
    at that moment.
    """
    deadline = time.monotonic() + WATCH_SECONDS
    while time.monotonic() < deadline:
        if os.path.exists(sandbox):
            shutil.rmtree(sandbox, ignore_errors=True)
        time.sleep(0.1)
    if os.path.exists(sandbox):
        shutil.rmtree(sandbox, ignore_errors=True)


def main(argv):
    opts = {}
    index = 0
    while index + 1 < len(argv):
        opts[argv[index]] = argv[index + 1]
        index += 2
    sandbox = opts["--sandbox"]
    marker = opts["--marker"]
    watch_fd = int(opts["--fd"])
    token = _read_token()
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
    if token and _marker_carries_token(marker, token):
        try:
            os.unlink(marker)
        except OSError:
            pass
        return 0
    deadline = time.monotonic() + RETRY_SECONDS
    while True:
        shutil.rmtree(sandbox, ignore_errors=True)
        if not os.path.exists(sandbox):
            break
        if time.monotonic() >= deadline:
            break
        time.sleep(0.1)
    try:
        os.unlink(marker)
    except OSError:
        pass
    if not os.path.exists(sandbox):
        _watch_for_recreation(sandbox)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
