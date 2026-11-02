"""The backend registry.

Backends register by name here and nowhere else, so the runner and
the command line resolve a name to a backend in exactly one place.
"""

from .base import HarnessError
from .namespace import NamespaceBackend

_BACKENDS = {"namespace": NamespaceBackend}


def get_backend(name):
    try:
        cls = _BACKENDS[name]
    except KeyError:
        raise HarnessError("unknown backend, " + str(name)) from None
    return cls()


def backend_names():
    return sorted(_BACKENDS)
