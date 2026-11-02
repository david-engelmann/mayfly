#!/usr/bin/env python3
"""The mayfly in-sandbox init.

This file runs as pid 1 inside the namespaces unshare created. It
assembles the prepared root, chroots into it, supervises one task,
and reports lifecycle events as json lines on the status descriptor
it was handed. It imports nothing from the mayfly package, because
it runs by path before any package context exists.

The init forks the task itself and waits on it, so the exact exit
code and the exact signal come from its own wait. Nothing here
trusts unshare to report how the task died, because a probe showed
unshare reports 0 when pid 1 dies by a signal.
"""

import ctypes
import json
import os
import socket
import sys

MS_BIND = 4096
MS_REC = 16384
MS_PRIVATE = 1 << 18
MS_REMOUNT = 32
MS_RDONLY = 1

RESERVED_EXIT = 125
HOSTNAME = "mayfly"
DEVICE_FILES = ("null", "zero", "full", "random", "urandom")
ROOT_DIRS = ("usr", "etc", "dev", "proc", "sys", "work", "tmp")
LINKED_DIRS = ("bin", "sbin", "lib", "lib64")

_libc = ctypes.CDLL(None, use_errno=True)


def _mount(source, target, fstype, flags):
    src = source.encode() if source else None
    fs = fstype.encode() if fstype else None
    result = _libc.mount(src, target.encode(), fs, flags, None)
    if result != 0:
        err = ctypes.get_errno()
        raise OSError(err, os.strerror(err), target)


def assemble_root(rootfs):
    """Build the prepared root and return whether proc was mounted."""
    _mount(None, "/", None, MS_REC | MS_PRIVATE)
    for name in ROOT_DIRS:
        os.makedirs(os.path.join(rootfs, name), exist_ok=True)
    bind_sources = ["/usr", "/etc"]
    for name in LINKED_DIRS:
        host = "/" + name
        target = os.path.join(rootfs, name)
        if os.path.islink(host):
            if not os.path.lexists(target):
                os.symlink(os.readlink(host), target)
        elif os.path.isdir(host):
            os.makedirs(target, exist_ok=True)
            bind_sources.append(host)
    targets = []
    for host in bind_sources:
        target = os.path.join(rootfs, host.lstrip("/"))
        _mount(host, target, None, MS_BIND | MS_REC)
        targets.append(target)
    for target in targets:
        _mount(target, target, None, MS_BIND | MS_REC | MS_REMOUNT | MS_RDONLY)
    for name in DEVICE_FILES:
        target = os.path.join(rootfs, "dev", name)
        if not os.path.exists(target):
            open(target, "a").close()
        _mount("/dev/" + name, target, None, MS_BIND)
    _mount("tmpfs", os.path.join(rootfs, "work"), "tmpfs", 0)
    try:
        _mount("proc", os.path.join(rootfs, "proc"), "proc", 0)
    except OSError:
        return False
    return True


class Init:
    def __init__(self, status_fd, command):
        self.status_fd = status_fd
        self.command = command

    def status(self, event):
        try:
            os.write(self.status_fd, (json.dumps(event) + "\n").encode())
        except OSError:
            pass

    def fail(self, detail):
        self.status({"event": "start_error", "detail": detail})
        os._exit(RESERVED_EXIT)

    def supervise(self):
        read_fd, write_fd = os.pipe2(os.O_CLOEXEC)
        child = os.fork()
        if child == 0:
            os.close(read_fd)
            os.close(self.status_fd)
            try:
                os.execvpe(self.command[0], self.command, os.environ)
            except OSError as exc:
                payload = (str(exc.errno) + " " + (exc.strerror or str(exc))).encode()
                try:
                    os.write(write_fd, payload)
                except OSError:
                    pass
            os._exit(127)
        os.close(write_fd)
        report = os.read(read_fd, 4096)
        os.close(read_fd)
        if report:
            try:
                os.waitpid(child, 0)
            except ChildProcessError:
                pass
            self.fail("could not start the task, " + report.decode(errors="replace"))
        self.status({"event": "started"})
        exit_code = None
        signal_no = None
        while True:
            try:
                pid, status = os.waitpid(-1, 0)
            except ChildProcessError:
                break
            except InterruptedError:
                continue
            if pid == child:
                if os.WIFEXITED(status):
                    exit_code = os.WEXITSTATUS(status)
                elif os.WIFSIGNALED(status):
                    signal_no = os.WTERMSIG(status)
                break
        while True:
            try:
                pid, _ = os.waitpid(-1, os.WNOHANG)
            except ChildProcessError:
                break
            if pid <= 0:
                break
        if signal_no is not None:
            self.status({"event": "task_done", "signal": signal_no})
            return 128 + signal_no
        if exit_code is None:
            self.fail("the task vanished before its wait status was read")
        self.status({"event": "task_done", "exit_code": exit_code})
        return exit_code


def main(argv):
    # The runner disables locale coercion for the supervisor
    # processes with this key. Its work is done by the time this
    # process starts, so remove it and the task never sees it.
    os.environ.pop("PYTHONCOERCECLOCALE", None)
    sep = argv.index("--")
    opts = argv[:sep]
    command = argv[sep + 1 :]
    rootfs = opts[opts.index("--rootfs") + 1]
    status_fd = int(opts[opts.index("--status-fd") + 1])
    init = Init(status_fd, command)
    if not command:
        init.fail("no command was given")
    try:
        proc_mounted = assemble_root(rootfs)
        socket.sethostname(HOSTNAME)
        os.chroot(rootfs)
        os.chdir("/work")
    except OSError as exc:
        init.fail("the prepared root failed, " + str(exc))
    init.status({"event": "ready", "proc_mounted": proc_mounted})
    return init.supervise()


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
