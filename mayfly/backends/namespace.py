"""The namespace backend.

One run is one unshare call. The unshare process creates a user
namespace with the caller mapped to root, plus mount, pid, uts, and
ipc namespaces, and a fresh network namespace unless the run opted
in to the host network. Inside, the mayfly init assembles a
prepared root, chroots into it, and supervises one task. The runner
side here owns the phases outside the sandbox, prepare, start,
task, and teardown, and times each one from the lifecycle events
the init reports on a status pipe.

This backend is a Linux namespace sandbox. It is not a micro-VM
and nothing in this repo calls it one.
"""

import json
import os
import shutil
import subprocess
import sys
import threading
import time
import uuid
from pathlib import Path

from .base import RunReport

TAIL_CHARS = 4000
HARNESS_EXIT_CODE = 125
PHASES = ("prepare", "start", "task", "teardown")


def _ms(since):
    return round((time.perf_counter() - since) * 1000, 3)


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
        # The init is a Python process, and Python coerces a C locale
        # by injecting LC_CTYPE into its own environment, which the
        # task would then inherit. Coercion is disabled for the
        # supervisor processes, and the init removes this key before
        # it forks the task, so the task environment stays exact.
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

    def _teardown(self, sandbox_dir, report, timings):
        phase = time.perf_counter()
        shutil.rmtree(sandbox_dir, ignore_errors=True)
        report.sandbox_removed = not sandbox_dir.exists()
        timings["teardown"] = _ms(phase)

    def run(self, spec):
        timings = {name: 0.0 for name in PHASES}
        run_id = uuid.uuid4().hex[:12]
        sandbox_dir = Path(spec.root) / ("run-" + run_id)
        report = RunReport(
            backend=self.name,
            run_id=run_id,
            sandbox_dir=str(sandbox_dir),
            command=list(spec.command),
            outcome="harness_error",
        )
        wall = time.perf_counter()

        phase = time.perf_counter()
        try:
            (sandbox_dir / "rootfs").mkdir(parents=True)
        except OSError as exc:
            report.error = "prepare failed, " + str(exc)
            timings["prepare"] = _ms(phase)
            report.timings_ms = timings
            return report
        timings["prepare"] = _ms(phase)

        if not self.unshare_path:
            report.error = "unshare is not available on this machine"
            self._teardown(sandbox_dir, report, timings)
            report.timings_ms = timings
            return report

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
            )
        except OSError as exc:
            os.close(status_r)
            os.close(status_w)
            report.error = "could not launch unshare, " + str(exc)
            timings["start"] = _ms(spawn)
            self._teardown(sandbox_dir, report, timings)
            report.timings_ms = timings
            return report
        os.close(status_w)
        reader = threading.Thread(target=_drain_status, args=(status_r, events), daemon=True)
        reader.start()
        out_b, err_b = proc.communicate()
        reader.join(timeout=5)
        if reader.is_alive():
            os.close(status_r)
        exited = time.perf_counter()

        report.stdout_tail = out_b.decode("utf-8", errors="replace")[-TAIL_CHARS:]
        report.stderr_tail = err_b.decode("utf-8", errors="replace")[-TAIL_CHARS:]

        by_name = {}
        for ts, event in events:
            by_name.setdefault(event.get("event"), (ts, event))
        if "ready" in by_name:
            report.proc_mounted = bool(by_name["ready"][1].get("proc_mounted"))
        if "task_done" in by_name:
            done = by_name["task_done"][1]
            report.outcome = "task"
            if "signal" in done:
                report.task_signal = int(done["signal"])
            else:
                report.task_exit_code = int(done["exit_code"])
            started_ts = by_name["started"][0] if "started" in by_name else spawn
            timings["start"] = round((started_ts - spawn) * 1000, 3)
            timings["task"] = round((exited - started_ts) * 1000, 3)
        elif "start_error" in by_name:
            report.error = by_name["start_error"][1].get("detail", "the sandbox failed to start")
            timings["start"] = round((exited - spawn) * 1000, 3)
        else:
            detail = "the sandbox exited without starting the task"
            if proc.returncode:
                detail += ", unshare exit " + str(proc.returncode)
            if report.stderr_tail.strip():
                detail += ", sandbox stderr tail, " + report.stderr_tail.strip()[-300:]
            report.error = detail
            timings["start"] = round((exited - spawn) * 1000, 3)

        self._teardown(sandbox_dir, report, timings)
        timings["total"] = _ms(wall)
        report.timings_ms = timings
        return report
