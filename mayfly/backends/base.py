"""Shared types for mayfly backends.

A backend is anything that takes a RunSpec, runs one task in a
disposable sandbox, and returns a RunReport. The runner and the
command line talk only to this interface, so a new backend joins by
registering a name and implementing run. The lifecycle phases every
backend reports timings for are prepare, start, task, and teardown.
"""

from dataclasses import dataclass, field
from pathlib import Path


HARNESS_EXIT_CODE = 125


class HarnessError(Exception):
    """The harness failed before or around the task, not in the task."""


@dataclass
class RunSpec:
    """One task to run in one fresh sandbox."""

    command: list
    root: Path
    allow_env: tuple = ("PATH",)
    network: bool = False
    pass_fds: tuple = ()


@dataclass
class RunReport:
    """The record of one run, printed by the command line as json."""

    backend: str
    run_id: str
    sandbox_dir: str
    command: list
    outcome: str
    task_exit_code: int | None = None
    task_signal: int | None = None
    error: str | None = None
    proc_mounted: bool | None = None
    sandbox_removed: bool | None = None
    timings_ms: dict = field(default_factory=dict)
    stdout_tail: str = ""
    stderr_tail: str = ""

    def to_dict(self):
        return {
            "backend": self.backend,
            "run_id": self.run_id,
            "sandbox_dir": self.sandbox_dir,
            "command": list(self.command),
            "outcome": self.outcome,
            "task_exit_code": self.task_exit_code,
            "task_signal": self.task_signal,
            "error": self.error,
            "proc_mounted": self.proc_mounted,
            "sandbox_removed": self.sandbox_removed,
            "timings_ms": dict(self.timings_ms),
            "stdout_tail": self.stdout_tail,
            "stderr_tail": self.stderr_tail,
        }
