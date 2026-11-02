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
the supervisor dies without removing it. A supervisor interrupted
by SIGTERM or SIGINT still tears the sandbox down and reports the
run as a harness error.

This backend is a Linux namespace sandbox. It is not a micro-VM
and nothing in this repo calls it one.
"""

import ctypes
import json
import os
import shutil
import signal
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


@contextmanager
def _signal_guard(interrupted):
    """Record SIGTERM and SIGINT instead of dying on them mid run.

    The handlers only append the signal number, and the wait loop
    in run polls the list, kills the sandbox, and tears it down.
    Handlers exist only on the main thread, so a run driven from a
    worker thread gets none and keeps the default behaviour.
    """
    installed = {}
    if threading.current_thread() is threading.main_thread():

        def _handler(signum, frame):
            interrupted.append(signum)

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

    def _start_janitor(self, sandbox_dir, marker):
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
                stdin=subprocess.DEVNULL,
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
        return (proc, write_fd)

    def _teardown(self, sandbox_dir, report, timings, since=None):
        phase = since if since is not None else time.perf_counter()
        shutil.rmtree(sandbox_dir, ignore_errors=True)
        report.sandbox_removed = not sandbox_dir.exists()
        timings["teardown"] = round((time.perf_counter() - phase) * 1000, 3)

    def run(self, spec):
        timings = {name: 0.0 for name in PHASES}
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
        try:
            with _signal_guard(interrupted):
                return self._run_inner(spec, sandbox_dir, report, timings, marker, interrupted)
        finally:
            timings["total"] = _ms(wall)
            report.timings_ms = timings
            if report.proc_mounted is None:
                report.proc_mounted = False
            if report.sandbox_removed is None:
                report.sandbox_removed = not sandbox_dir.exists()

    def _run_inner(self, spec, sandbox_dir, report, timings, marker, interrupted):
        phase = time.perf_counter()
        try:
            (sandbox_dir / "rootfs").mkdir(parents=True)
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

        janitor = self._start_janitor(sandbox_dir, marker)
        try:
            return self._run_supervised(spec, sandbox_dir, report, timings, interrupted)
        finally:
            if janitor is not None:
                janitor_proc, janitor_fd = janitor
                clean = bool(report.sandbox_removed)
                if clean:
                    try:
                        marker.write_text("clean", encoding="utf-8")
                    except OSError:
                        pass
                try:
                    os.close(janitor_fd)
                except OSError:
                    pass
                if clean:
                    try:
                        janitor_proc.wait(timeout=10)
                    except Exception:
                        pass

    def _run_supervised(self, spec, sandbox_dir, report, timings, interrupted):
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
        except (OSError, ValueError) as exc:
            os.close(status_r)
            os.close(status_w)
            report.error = "could not launch unshare, " + str(exc)
            timings["start"] = _ms(spawn)
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
        exited = time.perf_counter()

        report.stdout_tail = out_tail.text()
        report.stderr_tail = err_tail.text()

        by_name = {}
        for receipt_ts, event in events:
            by_name.setdefault(event.get("event"), (receipt_ts, event))
        if "ready" in by_name:
            report.proc_mounted = bool(by_name["ready"][1].get("proc_mounted"))
        ended_ts = None
        if "task_done" in by_name:
            done_receipt, done = by_name["task_done"]
            report.outcome = "task"
            if "signal" in done:
                report.task_signal = int(done["signal"])
            else:
                report.task_exit_code = int(done["exit_code"])
            started_receipt, started = by_name.get("started", (spawn, {}))
            started_ts = started.get("ts", started_receipt)
            ended_ts = done.get("ts", done_receipt)
            timings["start"] = round(max(0.0, started_ts - spawn) * 1000, 3)
            timings["task"] = round(max(0.0, ended_ts - started_ts) * 1000, 3)
        elif "start_error" in by_name:
            error_receipt, error_event = by_name["start_error"]
            report.error = error_event.get("detail", "the sandbox failed to start")
            ended_ts = error_event.get("ts", error_receipt)
            timings["start"] = round((exited - spawn) * 1000, 3)
        else:
            if interrupted:
                report.error = (
                    "the run was interrupted by signal "
                    + str(interrupted[0])
                    + " and the sandbox was torn down"
                )
            else:
                detail = "the sandbox exited without starting the task"
                if proc.returncode:
                    detail += ", unshare exit " + str(proc.returncode)
                if report.stderr_tail.strip():
                    detail += ", sandbox stderr tail, " + report.stderr_tail.strip()[-300:]
                report.error = detail
            timings["start"] = round((exited - spawn) * 1000, 3)

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
