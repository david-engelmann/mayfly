# mayfly

A disposable sandbox for agent tasks. It fires up one fresh box per task, runs the task inside, and incinerates the box on exit.

## What this is

Agents run code they just wrote, on your machine, with your credentials nearby. Mayfly gives that code somewhere else to live. Each task gets a fresh disposable box, runs in isolation, and is destroyed the moment the task ends. Nothing persists unless you ask for it. The box is a mayfly by design. It lives for one job and dies without ceremony. The aim is a fresh disposable sandbox for every task, with every backend behind the one lifecycle interface, so a backend on a stronger isolation runtime can take the same place where such a runtime exists. Slice 1 runs that lifecycle on a Linux namespace sandbox, and every run prints its measured phase timings in its run report.

## Slice 1, the namespace backend

The harness is a small Python package with one command, mayfly run. Give it a root directory and a command and it prepares one sandbox under that root, runs the command inside, prints a run report as json, and destroys the sandbox. The command line exits with the task exit code, with 125 for a harness error and 128 plus the signal number for a task killed by a signal, so the three outcomes never blur.

    mayfly run --root /path/to/sandboxes -- python3 task.py

The sandbox is a set of Linux namespaces, a user namespace with the caller mapped to root, plus mount and pid and uts and ipc namespaces, and a fresh network namespace unless a run passes the --net flag to share the host network. Inside them an init assembles a prepared root from read only binds of the host system directories, the basic device files, and a tmpfs work directory, then pivots into it with the old root detached behind it and supervises the task under itself at pid 1. The task starts in the work directory with an environment scrubbed to an allowlist, PATH by default, with repeated --allow-env flags adding more, and it sees the hostname mayfly. The task runs with an empty capability set, so it cannot chroot or mount or remount its way out of the prepared root. When the task ends the namespace dies with it, every process in the tree dies with it, and the sandbox directory is removed from the host. Interrupting the command still tears the sandbox down, and a command killed outright leaves no task running, and its sandbox directory is removed by a janitor watching for the supervisor to die without removing it, and where the janitor dies too the next run under the same root sweeps the directory away before it prepares its own.

The limits are named in the contract and they are real. This backend is a namespace sandbox and not a micro-VM, and slice 1 has no hypervisor and no GPU support and sets no resource limits and no timeout on the task. The backend is wired for x86_64 and aarch64 machines, and on any other Linux architecture a run ends as a harness error before the task starts. A fresh proc mount inside a user namespace depends on the runtime and is refused on some machines, and where it is refused the prepared root carries an empty proc directory and the run report says so. Timings are measured on every run and printed in the report, and no speed claim in this repo comes from anywhere else.

## Status

The remote seed was verified on 2026-11-02 against the staged seed and its commit is ca5df488b41c75114a08b752aaa0ed2eae255c92, a parentless commit holding exactly README.md and LICENSE and .gitignore. Setup arrives through a pull request and no work lands on main directly.

The contract lives in CONTRACT.md, where the setup obligations S1 to S5 stand beside the full contract that landed with slice 1, obligations MF-1 to MF-12, each stated so it can fail.

Closed perfection rounds are 0 of 100. A round counts only when a full cycle of work and audit and review and red team and approval has closed, and seeding and setup are not rounds.

The local gate is python -m pytest -q plus python scripts/codepoint_scan.py, run on the exact final head before any push, with the continuous integration workflow running the same commands as a backstop.

## Licence

MIT. See LICENSE.
