"""The mayfly command line.

mayfly run takes a root directory and a command, runs the command
in a fresh sandbox under that root, prints the run report as json,
and exits with the task exit code. A harness error exits at 125 and
a task killed by a signal exits at 128 plus the signal number, so
the three outcomes are never confused with each other.
"""

import argparse
import json
import sys

from .backends.base import HARNESS_EXIT_CODE, HarnessError
from .runner import run_task


def build_parser():
    parser = argparse.ArgumentParser(
        prog="mayfly",
        description="Run one task in a disposable sandbox and print the run report.",
    )
    sub = parser.add_subparsers(dest="subcommand", required=True)
    run = sub.add_parser("run", help="run one command in a fresh sandbox")
    run.add_argument("--root", required=True, help="directory under which sandboxes are prepared")
    run.add_argument("--backend", default="namespace", help="backend name, namespace is the default")
    run.add_argument(
        "--allow-env",
        action="append",
        default=None,
        metavar="KEY",
        help="caller environment key the task may see, repeatable, PATH is the default",
    )
    run.add_argument(
        "--net",
        action="store_true",
        help="share the host network instead of a fresh network namespace",
    )
    run.add_argument("command", nargs=argparse.REMAINDER)
    return parser


def main(argv=None):
    args = build_parser().parse_args(argv)
    command = list(args.command)
    if command and command[0] == "--":
        command = command[1:]
    if not command:
        print("mayfly run needs a command after --", file=sys.stderr)
        return 2
    allow_env = tuple(args.allow_env) if args.allow_env else ("PATH",)
    try:
        report = run_task(
            command,
            args.root,
            backend=args.backend,
            allow_env=allow_env,
            network=args.net,
        )
    except HarnessError as exc:
        payload = {
            "backend": args.backend,
            "command": command,
            "outcome": "harness_error",
            "error": str(exc),
        }
        print(json.dumps(payload, indent=2))
        return HARNESS_EXIT_CODE
    print(json.dumps(report.to_dict(), indent=2))
    if report.outcome == "harness_error":
        return HARNESS_EXIT_CODE
    if report.task_signal is not None:
        return 128 + report.task_signal
    if report.task_exit_code is not None:
        return report.task_exit_code
    return HARNESS_EXIT_CODE


if __name__ == "__main__":
    sys.exit(main())
