#!/usr/bin/env python3
"""The mayfly in-sandbox init.

This file runs as pid 1 inside the namespaces unshare created. It
assembles the prepared root, pivots into it with the old root
detached behind it, supervises one task, and reports lifecycle
events as json lines on the status descriptor it was handed. It
imports nothing from the mayfly package, because it runs by path
before any package context exists.

The init forks the task itself and waits on it, so the exact exit
code and the exact signal come from its own wait. Nothing here
trusts unshare to report how the task died, because a probe showed
unshare reports 0 when pid 1 dies by a signal.

The init keeps its own capabilities for the assembly. The forked
task child drops every capability before it execs, so the task
holds no CAP_SYS_CHROOT and no CAP_SYS_ADMIN and cannot chroot or
mount or remount its way out of the prepared root. The init also
arms a parent death signal and watches the status pipe, so when
the supervisor on the host goes away the sandbox dies with it.
"""

import ctypes
import json
import os
import platform
import signal
import socket
import sys
import threading
import time

MS_BIND = 4096
MS_REC = 16384
MS_PRIVATE = 1 << 18
MS_REMOUNT = 32
MS_RDONLY = 1
MNT_DETACH = 2

PR_SET_PDEATHSIG = 1
PR_CAPBSET_DROP = 24
CAPSET_VERSION_3 = 0x20080522
AT_FDCWD = -100
AT_RECURSIVE = 0x8000
MOUNT_ATTR_RDONLY = 0x0001
SYS_PIVOT_ROOT = {"x86_64": 155, "aarch64": 41}
SYS_MOUNT_SETATTR = {"x86_64": 442, "aarch64": 442}

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


class _MountAttr(ctypes.Structure):
    _fields_ = [
        ("attr_set", ctypes.c_uint64),
        ("attr_clr", ctypes.c_uint64),
        ("propagation", ctypes.c_uint64),
        ("mission_mask", ctypes.c_uint64),
        ("userns_fd", ctypes.c_uint64),
    ]


def _remount_read_only_tree(target):
    """Set the read only flag on a bound tree and every submount.

    The recursive mount_setattr call carries the claim where the
    kernel offers it. The walk behind it remounts each mount point
    under the target one by one, deepest first, for kernels where
    the call is refused.
    """
    number = SYS_MOUNT_SETATTR.get(platform.machine())
    if number is not None:
        attr = _MountAttr()
        attr.attr_set = MOUNT_ATTR_RDONLY
        result = _libc.syscall(
            number,
            AT_FDCWD,
            target.encode(),
            AT_RECURSIVE,
            ctypes.byref(attr),
            ctypes.sizeof(attr),
        )
        if result == 0:
            return
    points = []
    with open("/proc/self/mounts", encoding="utf-8") as handle:
        for line in handle:
            fields = line.split()
            if len(fields) >= 2:
                points.append(fields[1])
    under = [p for p in points if p == target or p.startswith(target + "/")]
    for point in sorted(under, key=len, reverse=True):
        _mount(point, point, None, MS_BIND | MS_REMOUNT | MS_RDONLY)


def assemble_root(rootfs):
    """Build the prepared root and return whether proc was mounted."""
    _write_provenance(rootfs)
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
        _remount_read_only_tree(target)
    for name in DEVICE_FILES:
        target = os.path.join(rootfs, "dev", name)
        if not os.path.exists(target):
            open(target, "a").close()
        _mount("/dev/" + name, target, None, MS_BIND)
    _mount("tmpfs", os.path.join(rootfs, "work"), "tmpfs", 0)
    proc_mounted = True
    try:
        _mount("proc", os.path.join(rootfs, "proc"), "proc", 0)
    except OSError:
        proc_mounted = False
    _write_provenance(rootfs)
    return proc_mounted


def _write_provenance(rootfs):
    """Write the run provenance beside the prepared root.

    The runner writes the provenance file when it creates the
    sandbox directory, and that copy is what the orphan sweep
    trusts. A janitor whose supervisor was killed can remove
    the whole directory while this assembly is still under
    way, and the assembly then recreates the rootfs shape on
    the same path, so a skeleton left that way carried no
    provenance and no sweep would ever collect it. The init
    therefore writes the provenance when its creation work
    begins and again when that work is done, naming the run
    the directory name already carries, so any residue this
    assembly leaves stands under the provenance it was born
    with, and a removal that lands before the assembly starts
    meets the write at the assembly's first step rather than
    only at its last. The write lands through
    a temporary file in the same directory that is renamed
    over the provenance path, so a death inside the write
    leaves the runner copy standing whole and never a
    truncated file that names nothing. A failure to write it
    is swallowed, because the runner copy may still stand and
    a missing provenance must never fail a run whose sandbox
    is otherwise sound.
    """
    sandbox = os.path.dirname(os.path.abspath(rootfs))
    name = os.path.basename(sandbox)
    if not name.startswith("run-"):
        return
    target = os.path.join(sandbox, ".mayfly-provenance")
    tmp = target + ".tmp-" + str(os.getpid())
    try:
        try:
            with open(tmp, "w", encoding="utf-8") as handle:
                handle.write(name[len("run-"):] + "\n")
            os.replace(tmp, target)
        except OSError:
            try:
                os.unlink(tmp)
            except OSError:
                pass
            raise
    except OSError:
        pass


def enter_root(rootfs):
    """Pivot into the prepared root and detach the old root."""
    number = SYS_PIVOT_ROOT.get(platform.machine())
    if number is None:
        raise OSError(38, "pivot_root is not available on this architecture")
    _mount(rootfs, rootfs, None, MS_BIND | MS_REC)
    oldroot = os.path.join(rootfs, ".oldroot")
    os.makedirs(oldroot, exist_ok=True)
    result = _libc.syscall(number, rootfs.encode(), oldroot.encode())
    if result != 0:
        err = ctypes.get_errno()
        raise OSError(err, os.strerror(err), rootfs)
    os.chdir("/")
    result = _libc.umount2(b"/.oldroot", MNT_DETACH)
    if result != 0:
        err = ctypes.get_errno()
        raise OSError(err, os.strerror(err), "/.oldroot")
    os.rmdir("/.oldroot")


class _CapHeader(ctypes.Structure):
    _fields_ = [("version", ctypes.c_uint32), ("pid", ctypes.c_int)]


class _CapData(ctypes.Structure):
    _fields_ = [
        ("effective", ctypes.c_uint32),
        ("permitted", ctypes.c_uint32),
        ("inheritable", ctypes.c_uint32),
    ]


def _drop_capabilities():
    """Empty the capability sets of the calling process.

    Every capability leaves the bounding set first, so no exec can
    bring one back, and the effective and permitted and inheritable
    sets are then set empty in one capset call.
    """
    for cap in range(64):
        _libc.prctl(PR_CAPBSET_DROP, cap, 0, 0, 0)
    header = _CapHeader(CAPSET_VERSION_3, 0)
    data = (_CapData * 2)()
    if _libc.capset(ctypes.byref(header), data) != 0:
        err = ctypes.get_errno()
        raise OSError(err, os.strerror(err), "capset")


class Init:
    def __init__(self, status_fd, command):
        self.status_fd = status_fd
        self.command = command
        self.child = None

    def status(self, event):
        try:
            payload = dict(event)
            payload.setdefault("ts", time.monotonic())
            os.write(self.status_fd, (json.dumps(payload) + "\n").encode())
        except OSError:
            pass

    def fail(self, detail):
        self.status({"event": "start_error", "detail": detail})
        os._exit(RESERVED_EXIT)

    def _watch_supervisor(self):
        """Die with the task when the host supervisor goes away.

        The runner holds the read end of the status pipe for the
        whole run. When every read end is gone the poll on this
        write end reports an error, and the sandbox has no
        supervisor left to report to, so the task is killed and
        the namespace dies with the init.
        """
        poller = None
        try:
            import select

            poller = select.poll()
            poller.register(self.status_fd, 0)
        except (ImportError, OSError):
            return
        while self.child is not None:
            try:
                ready = poller.poll(200)
            except OSError:
                return
            for _, mask in ready:
                if mask & (select.POLLERR | select.POLLHUP):
                    try:
                        os.kill(self.child, signal.SIGKILL)
                    except OSError:
                        pass
                    os._exit(128 + signal.SIGKILL)

    def supervise(self):
        read_fd, write_fd = os.pipe2(os.O_CLOEXEC)
        child = os.fork()
        if child == 0:
            os.close(read_fd)
            os.close(self.status_fd)
            try:
                _drop_capabilities()
                os.execvpe(self.command[0], self.command, os.environ)
            except OSError as exc:
                payload = (str(exc.errno) + " " + (exc.strerror or str(exc))).encode()
                try:
                    os.write(write_fd, payload)
                except OSError:
                    pass
            os._exit(127)
        self.child = child
        watcher = threading.Thread(target=self._watch_supervisor, daemon=True)
        watcher.start()
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
        self.child = None
        if signal_no is not None:
            self.status({"event": "task_done", "signal": signal_no})
            return 128 + signal_no
        if exit_code is None:
            self.fail("the task vanished before its wait status was read")
        self.status({"event": "task_done", "exit_code": exit_code})
        return exit_code


def main(argv):
    # The harness sets PYTHONCOERCECLOCALE to 0 in the environment
    # every supervisor and the task share, and the contract names
    # that key as a harness key of the task environment itself, so
    # it stays. Where the caller allowlisted the key, the caller
    # value stands in its place and stays on the same terms.
    try:
        _libc.prctl(PR_SET_PDEATHSIG, signal.SIGKILL, 0, 0, 0)
    except Exception:
        pass
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
        enter_root(rootfs)
        os.chdir("/work")
    except OSError as exc:
        init.fail("the prepared root failed, " + str(exc))
    init.status({"event": "ready", "proc_mounted": proc_mounted})
    return init.supervise()


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
