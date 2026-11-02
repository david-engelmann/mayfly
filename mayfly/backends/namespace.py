"""The namespace backend.

One run is one unshare call. The unshare process creates a user
namespace with the caller mapped to root, plus mount, pid, uts, and
ipc namespaces, and a fresh network namespace unless the run opted
in to the host network. Inside, the mayfly init assembles a
prepared root, pivots into it, and supervises one task. The runner
side here owns the phases outside the sandbox, prepare, start,
task, and teardown, and times the start and task phases from the
monotonic stamps the init puts on its own lifecycle events.

The unshare process is spawned with a parent death signal, so a
supervisor killed outright takes the sandbox tree down with it,
and the init arms the same signal against unshare. A janitor
process watches each run and removes the sandbox directory when
the supervisor dies without removing it, and it trusts only a
clean marker carrying the per run token it was handed over its
standard input at spawn. A run also sweeps orphaned sandbox
directories left under its root by runs whose supervisor and
janitor both died, before it prepares its own. A supervisor
interrupted by SIGTERM or SIGINT still tears the sandbox down
and reports the run as a harness error.

This backend is a Linux namespace sandbox. It is not a micro-VM
and nothing in this repo calls it one.
"""

import ctypes
import json
import os
import secrets
import shutil
import signal
import stat
import subprocess
import sys
import threading
import time
import uuid
from contextlib import contextmanager
from pathlib import Path

from .base import HARNESS_EXIT_CODE, RunReport

TAIL_CHARS = 4000
TAIL_BYTES = 1 << 16
PHASES = ("prepare", "start", "task", "teardown")

__all__ = ["HARNESS_EXIT_CODE", "NamespaceBackend", "TAIL_CHARS"]


def _ms(since):
    return round((time.perf_counter() - since) * 1000, 3)


_LIBC = ctypes.CDLL(None)


def _arm_death_signal():
    """Ask the kernel to kill this child when its parent dies."""
    try:
        _LIBC.prctl(1, 9)
    except Exception:
        pass


def _drain_status(fd, events):
    try:
        with os.fdopen(fd, "r", encoding="utf-8", errors="replace") as stream:
            for line in stream:
                try:
                    events.append((time.perf_counter(), json.loads(line)))
                except ValueError:
                    continue
    except OSError:
        return


class _TailBuffer:
    """A bounded tail of one stream, kept as bytes and decoded late."""

    def __init__(self, limit=TAIL_BYTES):
        self._buf = bytearray()
        self._limit = limit
        self._lock = threading.Lock()

    def feed(self, chunk):
        with self._lock:
            self._buf += chunk
            overflow = len(self._buf) - self._limit
            if overflow > 0:
                del self._buf[:overflow]

    def text(self):
        with self._lock:
            raw = bytes(self._buf)
        return raw.decode("utf-8", errors="replace")[-TAIL_CHARS:]


def _drain_stream(stream, tail):
    try:
        while True:
            chunk = stream.read1(8192)
            if not chunk:
                break
            tail.feed(chunk)
    except (OSError, ValueError):
        return


ORPHAN_GRACE_SECONDS = 5.0
PROVENANCE_NAME = ".mayfly-provenance"


def _pids_referencing(path_text):
    """Host pids whose command line names the given path.

    A live run always has processes that name its sandbox
    directory, the janitor through its sandbox argument and the
    init through its rootfs argument, so a sandbox directory no
    live process names belongs to a run that is over. Returns
    None when the process table cannot be listed at all, and
    also when it lists but not one command line in it can be
    read, because a table that cannot be read in substance says
    nothing about which runs are live and must never read as an
    empty one. A read that fails because the process vanished
    between the listing and the read is ordinary and is passed
    over. A read refused for any other reason also makes the
    answer None, because the one entry withheld may be the one
    that names the path.
    """
    needle = os.fsencode(path_text)
    found = []
    try:
        entries = os.listdir("/proc")
    except OSError:
        return None
    reads = 0
    for entry in entries:
        if not entry.isdigit():
            continue
        pid = int(entry)
        if pid == os.getpid():
            continue
        try:
            with open(os.path.join("/proc", entry, "cmdline"), "rb") as handle:
                cmdline = handle.read()
        except FileNotFoundError:
            continue
        except OSError:
            return None
        reads += 1
        if needle in cmdline:
            found.append(pid)
    if reads == 0:
        return None
    return found


PROVENANCE_READ_LIMIT = 4096


def _read_provenance(entry):
    """The run id a directory's provenance file records, or None.

    Every failure shape reads as no provenance at all, because
    each one means the directory carries nothing the sweep may
    trust. A path that is not a regular file names nothing, and
    neither do bytes that are not text and a read that fails.
    The check follows a symlink to the file it names, so a
    provenance that is a symlink to a regular file reads
    through to that file and the file itself is never touched.
    The open cannot block and the read is bounded, so a pipe
    planted at the provenance path or a file without end can
    neither hang the sweep nor fill its memory, and the strict
    decode lives inside the guard, so bytes that are not text
    raise nothing out of the sweep or the run behind it.
    """
    path = entry / PROVENANCE_NAME
    try:
        if not stat.S_ISREG(path.stat().st_mode):
            return None
    except OSError:
        return None
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NONBLOCK)
    except OSError:
        return None
    try:
        chunks = []
        total = 0
        while total <= PROVENANCE_READ_LIMIT:
            chunk = os.read(fd, PROVENANCE_READ_LIMIT)
            if not chunk:
                break
            chunks.append(chunk)
            total += len(chunk)
        if total > PROVENANCE_READ_LIMIT:
            return None
    except OSError:
        return None
    finally:
        os.close(fd)
    try:
        return b"".join(chunks).decode("utf-8").strip()
    except UnicodeDecodeError:
        return None


def _write_provenance(sandbox_dir, run_id):
    """Write the run provenance inside a fresh sandbox directory.

    The write lands through a temporary file in the same
    directory that is renamed over the provenance path, so a
    reader sees either no provenance at all or the complete
    file, and a death inside the write leaves no truncated
    file behind that names nothing and no sweep would ever
    collect. A failure raises OSError, which the caller
    reports as a prepare failure, and the temporary file is
    removed on the way out.
    """
    target = sandbox_dir / PROVENANCE_NAME
    tmp = sandbox_dir / (PROVENANCE_NAME + ".tmp-" + str(os.getpid()))
    try:
        with open(tmp, "w", encoding="utf-8") as handle:
            handle.write(run_id + "\n")
        os.replace(tmp, target)
    except OSError:
        try:
            tmp.unlink()
        except OSError:
            pass
        raise


def _sweep_orphans(root):
    """Remove sandbox directories under root whose runs are dead.

    A directory is swept only when the provenance file inside
    it names the same run id as the directory itself, so the
    sweep can tell the directories mayfly created from any
    other directory carrying the same name shape, an
    identification that guards against accidental collision
    and name confusion and is not authentication and no
    boundary against a writer to the run root, and only when it is
    older than the grace period, so a run still being prepared
    by a concurrent process is never touched, and only when no
    live process names it, so a live run of any age keeps its
    sandbox. Where the process table cannot be listed, liveness
    is unknown and nothing is swept. A clean marker whose run
    directory is already gone is swept on the same terms. A
    sweep failure never breaks the run that triggered it.
    """
    try:
        entries = list(root.iterdir())
    except OSError:
        return
    now = time.time()
    for entry in entries:
        try:
            if entry.is_dir() and entry.name.startswith("run-"):
                if now - entry.stat().st_mtime < ORPHAN_GRACE_SECONDS:
                    continue
                run_id = entry.name[len("run-"):]
                if _read_provenance(entry) != run_id:
                    continue
                referencing = _pids_referencing(str(entry))
                if referencing is None or referencing:
                    continue
                shutil.rmtree(entry, ignore_errors=True)
                marker = root / (".mayfly-clean-" + run_id)
                try:
                    marker.unlink()
                except OSError:
                    pass
            elif entry.is_file() and entry.name.startswith(".mayfly-clean-"):
                run_dir = root / ("run-" + entry.name[len(".mayfly-clean-"):])
                if run_dir.exists():
                    continue
                if now - entry.stat().st_mtime < ORPHAN_GRACE_SECONDS:
                    continue
                try:
                    entry.unlink()
                except OSError:
                    pass
        except OSError:
            continue


@contextmanager
def _signal_guard(interrupted, interrupt_times=None):
    """Record SIGTERM and SIGINT instead of dying on them mid run.

    The handlers only append the signal number, and the wait loop
    in run polls the list, kills the sandbox, and tears it down.
    Where a times list is given, the handler also stamps the
    moment each signal was received on the same monotonic clock
    the init stamps its events with, so the outcome logic can
    compare the receipt of an interrupt against the task done
    stamp rather than against when the runner happened to drain
    the event. Handlers exist only on the main thread, so a run
    driven from a worker thread gets none and keeps the default
    behaviour.
    """
    installed = {}
    if threading.current_thread() is threading.main_thread():

        def _handler(signum, frame):
            interrupted.append(signum)
            if interrupt_times is not None:
                interrupt_times.append(time.perf_counter())

        for name in (signal.SIGINT, signal.SIGTERM):
            try:
                installed[name] = signal.signal(name, _handler)
            except (ValueError, OSError):
                pass
    try:
        yield
    finally:
        for name, previous in installed.items():
            try:
                signal.signal(name, previous)
            except (ValueError, OSError):
                pass


def _write_clean_marker(marker, token):
    """Leave the clean marker carrying this run's token.

    A non regular object planted at the marker path is
    unlinked first, because opening a planted pipe for writing
    would block the finish past the end of the task. The
    planted object itself is all the unlink ever touches, the
    file a planted symlink names is never opened or removed.
    Where the planted object cannot be removed, the write is
    skipped and the run still finishes cleanly, and the janitor
    completes the cleanup in the runner's place when it sees
    its supervisor gone with no valid marker, removing a
    sandbox that is already gone as a no-op. The write itself
    cannot block either, so a pipe swapped into the path after
    the check fails the open instead of hanging on it.
    """
    try:
        mode = marker.lstat().st_mode
    except OSError:
        mode = None
    if mode is not None and not stat.S_ISREG(mode):
        try:
            marker.unlink()
        except OSError:
            return
    try:
        fd = os.open(
            marker, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NONBLOCK, 0o644
        )
    except OSError:
        return
    try:
        os.write(fd, (token + "\n").encode("utf-8"))
    except OSError:
        pass
    finally:
        os.close(fd)


class NamespaceBackend:
    name = "namespace"

    def __init__(self, unshare_path=None):
        if unshare_path is None:
            unshare_path = shutil.which("unshare")
        self.unshare_path = unshare_path

    def _task_env(self, spec):
        env = {}
        for key in spec.allow_env:
            if key in os.environ:
                env[key] = os.environ[key]
        env["MAYFLY_SANDBOX"] = self.name
        # The init and the task are Python processes, and Python
        # coerces a C locale by injecting LC_CTYPE into its own
        # environment. The coercion guard is a harness key of the
        # shared environment itself, named in the contract, and it
        # keeps every interpreter in the run from adding variables
        # the scrub did not produce. A caller who allowlists the
        # key keeps the caller value in its place.
        if "PYTHONCOERCECLOCALE" not in env:
            env["PYTHONCOERCECLOCALE"] = "0"
        return env

    def _unshare_argv(self, spec, sandbox_dir, status_fd):
        argv = [
            self.unshare_path,
            "--user",
            "--map-root-user",
            "--mount",
            "--pid",
            "--uts",
            "--ipc",
            "--fork",
        ]
        if not spec.network:
            argv.append("--net")
        init_path = Path(__file__).resolve().parent.parent / "_init.py"
        argv += [
            sys.executable,
            str(init_path),
            "--rootfs",
            str(sandbox_dir / "rootfs"),
            "--status-fd",
            str(status_fd),
            "--",
        ]
        argv += list(spec.command)
        return argv

    def _start_janitor(self, sandbox_dir, marker, token):
        read_fd, write_fd = os.pipe()
        janitor_path = Path(__file__).resolve().parent.parent / "_janitor.py"
        try:
            proc = subprocess.Popen(
                [
                    sys.executable,
                    str(janitor_path),
                    "--sandbox",
                    str(sandbox_dir),
                    "--marker",
                    str(marker),
                    "--fd",
                    str(read_fd),
                ],
                stdin=subprocess.PIPE,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                pass_fds=(read_fd,),
                start_new_session=True,
            )
        except (OSError, ValueError):
            os.close(read_fd)
            os.close(write_fd)
            return None
        os.close(read_fd)
        # The token reaches the janitor over its standard input,
        # never over its command line, so neither a directory
        # listing nor a process listing reveals it. A janitor that
        # never receives it authenticates nothing and cleans up.
        try:
            proc.stdin.write((token + "\n").encode("utf-8"))
            proc.stdin.close()
        except (OSError, ValueError):
            pass
        return (proc, write_fd)

    def _teardown(self, sandbox_dir, report, timings, since=None):
        phase = since if since is not None else time.perf_counter()
        shutil.rmtree(sandbox_dir, ignore_errors=True)
        report.sandbox_removed = not sandbox_dir.exists()
        timings["teardown"] = round((time.perf_counter() - phase) * 1000, 3)

    def run(self, spec):
        _sweep_orphans(Path(spec.root))
        timings = {name: None for name in PHASES}
        run_id = uuid.uuid4().hex[:12]
        sandbox_dir = Path(spec.root) / ("run-" + run_id)
        marker = Path(spec.root) / (".mayfly-clean-" + run_id)
        report = RunReport(
            backend=self.name,
            run_id=run_id,
            sandbox_dir=str(sandbox_dir),
            command=list(spec.command),
            outcome="harness_error",
        )
        wall = time.perf_counter()
        interrupted = []
        interrupt_times = []
        try:
            with _signal_guard(interrupted, interrupt_times):
                return self._run_inner(
                    spec,
                    sandbox_dir,
                    report,
                    timings,
                    marker,
                    interrupted,
                    run_id,
                    interrupt_times,
                )
        finally:
            timings["total"] = _ms(wall)
            report.timings_ms = timings
            if report.proc_mounted is None:
                report.proc_mounted = False
            if report.sandbox_removed is None:
                report.sandbox_removed = not sandbox_dir.exists()

    def _run_inner(
        self, spec, sandbox_dir, report, timings, marker, interrupted, run_id, interrupt_times
    ):
        phase = time.perf_counter()
        try:
            sandbox_dir.mkdir(parents=True)
        except OSError as exc:
            # The run directory is created as its own first
            # step, apart from everything prepared inside it,
            # so a failure here ends the run before it owns
            # anything. Where the directory already stands,
            # planted in the run root or met by the rare
            # collision on a run id, it is not this run's to
            # remove, and no teardown runs against it. The run
            # reports the failure and leaves the standing tree
            # exactly as it found it.
            report.error = "prepare failed, " + str(exc)
            timings["prepare"] = _ms(phase)
            return report
        try:
            (sandbox_dir / "rootfs").mkdir()
        except OSError as exc:
            report.error = "prepare failed, " + str(exc)
            timings["prepare"] = _ms(phase)
            self._teardown(sandbox_dir, report, timings)
            return report
        # The janitor is spawned here, as soon as the run
        # directory exists and before the born provenance
        # write, so a supervisor killed inside that write
        # already has a janitor watching the directory it
        # leaves behind. The janitor sleeps on its watch pipe
        # with no deadline of its own, so the earlier spawn
        # only lengthens its wait and its protocol is
        # unchanged, and the cleanup in the finally below now
        # also stands the janitor down on the prepare failure
        # and the missing unshare paths, where no janitor
        # existed before. What no cleanup layer covers is a
        # death before the born provenance write below lands,
        # whether it falls in the segment between the
        # directory's creation above and this spawn or takes
        # the janitor with the supervisor in a double kill
        # after it, and MF-12 names what such a death can
        # leave standing.
        token = secrets.token_hex(16)
        janitor = self._start_janitor(sandbox_dir, marker, token)
        try:
            try:
                _write_provenance(sandbox_dir, run_id)
            except OSError as exc:
                report.error = "prepare failed, " + str(exc)
                timings["prepare"] = _ms(phase)
                self._teardown(sandbox_dir, report, timings)
                return report
            timings["prepare"] = _ms(phase)

            if not self.unshare_path:
                report.error = "unshare is not available on this machine"
                self._teardown(sandbox_dir, report, timings)
                return report

            return self._run_supervised(
                spec, sandbox_dir, report, timings, interrupted, interrupt_times
            )
        finally:
            if janitor is not None:
                janitor_proc, janitor_fd = janitor
                clean = bool(report.sandbox_removed)
                if clean:
                    _write_clean_marker(marker, token)
                try:
                    os.close(janitor_fd)
                except OSError:
                    pass
                if clean:
                    try:
                        janitor_proc.wait(timeout=10)
                    except Exception:
                        pass

    def _run_supervised(self, spec, sandbox_dir, report, timings, interrupted, interrupt_times):
        status_r, status_w = os.pipe()
        events = []
        argv = self._unshare_argv(spec, sandbox_dir, status_w)
        spawn = time.perf_counter()
        try:
            proc = subprocess.Popen(
                argv,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                pass_fds=(status_w, *spec.pass_fds),
                env=self._task_env(spec),
                preexec_fn=_arm_death_signal,
            )
        except (OSError, ValueError, TypeError) as exc:
            os.close(status_r)
            os.close(status_w)
            report.error = "could not launch unshare, " + str(exc)
            self._teardown(sandbox_dir, report, timings)
            return report
        os.close(status_w)
        status_reader = threading.Thread(
            target=_drain_status, args=(status_r, events), daemon=True
        )
        status_reader.start()
        out_tail = _TailBuffer()
        err_tail = _TailBuffer()
        out_reader = threading.Thread(
            target=_drain_stream, args=(proc.stdout, out_tail), daemon=True
        )
        err_reader = threading.Thread(
            target=_drain_stream, args=(proc.stderr, err_tail), daemon=True
        )
        out_reader.start()
        err_reader.start()
        while proc.poll() is None:
            if interrupted:
                proc.kill()
                break
            time.sleep(0.02)
        proc.wait()
        for reader in (out_reader, err_reader):
            reader.join(timeout=5)
        status_reader.join(timeout=5)
        if status_reader.is_alive():
            try:
                os.close(status_r)
            except OSError:
                pass
            status_reader.join(timeout=5)

        report.stdout_tail = out_tail.text()
        report.stderr_tail = err_tail.text()

        by_name = {}
        for receipt_ts, event in events:
            by_name.setdefault(event.get("event"), (receipt_ts, event))
        if "ready" in by_name:
            report.proc_mounted = bool(by_name["ready"][1].get("proc_mounted"))
        started_ts = None
        if "started" in by_name:
            started_receipt, started = by_name["started"]
            started_ts = started.get("ts", started_receipt)
            timings["start"] = round(max(0.0, started_ts - spawn) * 1000, 3)
        ended_ts = None
        if "task_done" in by_name:
            done_receipt, done = by_name["task_done"]
            if started_ts is None:
                started_ts = spawn
            ended_ts = done.get("ts", done_receipt)
            timings["task"] = round(max(0.0, ended_ts - started_ts) * 1000, 3)
            # An interrupt received before the task done stamp
            # makes the run a harness error even where the task
            # done event reaches the runner only after the sandbox
            # has been killed, and an interrupt received after the
            # stamp leaves the task outcome standing. The stamps
            # are compared, not the order the runner drained the
            # events in, and MF-4 carries the rule.
            sig_ts = interrupt_times[0] if interrupt_times else None
            if interrupted and (sig_ts is None or sig_ts <= ended_ts):
                report.error = (
                    "the run was interrupted by signal "
                    + str(interrupted[0])
                    + " and the sandbox was torn down"
                )
            else:
                report.outcome = "task"
                if "signal" in done:
                    report.task_signal = int(done["signal"])
                else:
                    report.task_exit_code = int(done["exit_code"])
        elif "start_error" in by_name:
            error_receipt, error_event = by_name["start_error"]
            report.error = error_event.get("detail", "the sandbox failed to start")
            ended_ts = error_event.get("ts", error_receipt)
        else:
            if interrupted:
                report.error = (
                    "the run was interrupted by signal "
                    + str(interrupted[0])
                    + " and the sandbox was torn down"
                )
            else:
                if "started" in by_name:
                    detail = (
                        "the sandbox exited after the task started, "
                        "and no task end was reported"
                    )
                else:
                    detail = "the sandbox exited without starting the task"
                if proc.returncode:
                    detail += ", unshare exit " + str(proc.returncode)
                if report.stderr_tail.strip():
                    detail += ", sandbox stderr tail, " + report.stderr_tail.strip()[-300:]
                report.error = detail

        self._teardown(sandbox_dir, report, timings, since=ended_ts)
        if not report.sandbox_removed:
            if report.outcome == "task":
                report.outcome = "harness_error"
                report.task_exit_code = None
                report.task_signal = None
                report.error = "the sandbox directory could not be removed after the run"
            elif report.error:
                report.error += ", and the sandbox directory could not be removed"
            else:
                report.error = "the sandbox directory could not be removed after the run"
        return report
