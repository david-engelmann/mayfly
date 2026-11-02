"""The runner entry point the command line and the tests share."""

from pathlib import Path

from .backends import get_backend
from .backends.base import RunSpec


def run_task(command, root, backend="namespace", allow_env=("PATH",), network=False, pass_fds=()):
    spec = RunSpec(
        command=list(command),
        root=Path(root),
        allow_env=tuple(allow_env),
        network=network,
        pass_fds=tuple(pass_fds),
    )
    return get_backend(backend).run(spec)
