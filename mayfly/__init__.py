"""mayfly, a disposable sandbox harness for agent tasks."""

from .backends.base import HarnessError, RunReport, RunSpec
from .runner import run_task

__version__ = "0.1.0"

__all__ = ["HarnessError", "RunReport", "RunSpec", "run_task", "__version__"]
