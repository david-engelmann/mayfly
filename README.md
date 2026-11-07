# mayfly

Runs one command in a disposable directory with a scrubbed environment and the network off by default, mirrors the command's exit code, and deletes the directory when the command ends.

Mayfly is a small Rust program that gives one task a disposable sandbox to run in. You hand it a root directory and a command. It builds a fresh namespace sandbox under that root from Linux namespaces and a prepared root filesystem, runs the command inside under its own init, prints a run report as JSON, and tears the whole sandbox down when the command ends.

[![CI](https://github.com/david-engelmann/mayfly/actions/workflows/ci.yml/badge.svg)](https://github.com/david-engelmann/mayfly/actions/workflows/ci.yml)

```
{
  "backend": "namespace",
  "run_id": "51a2b1241885",
  "sandbox_dir": "/tmp/mayfly-runs/run-51a2b1241885",
  "command": [
    "/bin/echo",
    "hello",
    "from",
    "the",
    "sandbox"
  ],
  "outcome": "task",
  "task_exit_code": 0,
  "task_signal": null,
  "error": null,
  "proc_mounted": false,
  "sandbox_removed": true,
  "timings_ms": {
    "prepare": 3.715,
    "start": 70.995,
    "task": 0.839,
    "teardown": 18.499,
    "total": 104.36
  },
  "stdout_tail": "hello from the sandbox\n",
  "stderr_tail": ""
}
```

## Install

Mayfly builds from source with Cargo. Clone the repo and install the binary onto your path.

```
git clone https://github.com/david-engelmann/mayfly.git
cd mayfly
cargo install --path .
```

The install puts the mayfly binary in the Cargo bin directory. On a default install that directory is ~/.cargo/bin, and it must stand on your PATH for the mayfly command to resolve. Mayfly runs on Linux and is wired for x86_64 and aarch64 machines.

## Quickstart

Run one harmless command inside a fresh sandbox. The root is the directory mayfly manages its sandboxes under, and the command after the double dash is the task.

```
mayfly run --root /tmp/mayfly-runs -- /bin/echo hello from the sandbox
```

```
{
  "backend": "namespace",
  "run_id": "51a2b1241885",
  "sandbox_dir": "/tmp/mayfly-runs/run-51a2b1241885",
  "command": [
    "/bin/echo",
    "hello",
    "from",
    "the",
    "sandbox"
  ],
  "outcome": "task",
  "task_exit_code": 0,
  "task_signal": null,
  "error": null,
  "proc_mounted": false,
  "sandbox_removed": true,
  "timings_ms": {
    "prepare": 3.715,
    "start": 70.995,
    "task": 0.839,
    "teardown": 18.499,
    "total": 104.36
  },
  "stdout_tail": "hello from the sandbox\n",
  "stderr_tail": ""
}
```

The report is the run record. It names the outcome and the task exit code, carries the tail of what the task printed, and gives the measured phase timings for that run. The sandbox directory is gone once the run ends, and the report says so in its sandbox_removed field.

A task that fails keeps its own code. The harness mirrors it instead of masking it, and the shell sees the same code the task exited with.

```
mayfly run --root /tmp/mayfly-runs -- /bin/sh -c "echo the task gives up; exit 3"
{
  "backend": "namespace",
  "run_id": "d12c22f38a9d",
  "sandbox_dir": "/tmp/mayfly-runs/run-d12c22f38a9d",
  "command": [
    "/bin/sh",
    "-c",
    "echo the task gives up; exit 3"
  ],
  "outcome": "task",
  "task_exit_code": 3,
  "task_signal": null,
  "error": null,
  "proc_mounted": false,
  "sandbox_removed": true,
  "timings_ms": {
    "prepare": 4.261,
    "start": 71.931,
    "task": 0.071,
    "teardown": 25.085,
    "total": 157.735
  },
  "stdout_tail": "the task gives up\n",
  "stderr_tail": ""
}
3
```

The last line is the shell exit code of that run, printed by echo $? in the same session.

## Usage

### One run and its report

A run prints one JSON report and nothing else on its own standard output. The task sees the hostname mayfly, starts in a fresh work directory, and runs as root inside the user namespace with an empty capability set, supervised by the init at pid 1. This run asks the task for its hostname, its working directory, and its identity, and the report carries the answers in its stdout_tail field.

```
mayfly run --root /tmp/mayfly-runs -- /bin/sh -c "hostname; pwd; id"
{
  "backend": "namespace",
  "run_id": "f1b58fc00169",
  "sandbox_dir": "/tmp/mayfly-runs/run-f1b58fc00169",
  "command": [
    "/bin/sh",
    "-c",
    "hostname; pwd; id"
  ],
  "outcome": "task",
  "task_exit_code": 0,
  "task_signal": null,
  "error": null,
  "proc_mounted": false,
  "sandbox_removed": true,
  "timings_ms": {
    "prepare": 4.756,
    "start": 95.977,
    "task": 90.689,
    "teardown": 21.17,
    "total": 225.364
  },
  "stdout_tail": "mayfly\n/work\nuid=0(root) gid=0(root) groups=0(root)\n",
  "stderr_tail": ""
}
```

### The environment allowlist

The caller environment reaches the task scrubbed. By default the only caller value that stands inside is PATH. Name keys with repeated allow-env flags and those keys pass through with their values, and naming any key replaces the default list, so a run that wants its key and its PATH names both. The MAYFLY_SANDBOX and PYTHONCOERCECLOCALE lines are set inside the sandbox and stand in every run. What the task printed stands in each report in its stdout_tail field, and that field is what the two blocks below show.

```
MAYFLY_DEMO_KEY=demo-value mayfly run --root /tmp/mayfly-runs -- /usr/bin/env
MAYFLY_SANDBOX=namespace
PATH=/usr/bin:/bin
PYTHONCOERCECLOCALE=0
```

```
MAYFLY_DEMO_KEY=demo-value mayfly run --root /tmp/mayfly-runs --allow-env MAYFLY_DEMO_KEY --allow-env PATH -- /usr/bin/env
MAYFLY_DEMO_KEY=demo-value
MAYFLY_SANDBOX=namespace
PATH=/usr/bin:/bin
PYTHONCOERCECLOCALE=0
```

### The network flag

A run gets a fresh network namespace whose only interface is the loopback. Pass the net flag and the task shares the host network, and the host interface stands in the list beside the loopback. Its name varies by machine. The blocks below show what the task printed, carried in the report in its stdout_tail field.

```
mayfly run --root /tmp/mayfly-runs -- /usr/bin/python3 -c "import socket; print([name for index, name in socket.if_nameindex()])"
['lo']
```

```
mayfly run --root /tmp/mayfly-runs --net -- /usr/bin/python3 -c "import socket; print([name for index, name in socket.if_nameindex()])"
['lo', 'host0']
```

### Exit codes

The command line exit code states which of three outcomes happened. A task exit code is mirrored exactly, as the quickstart failure shows. A failure before the task starts is a harness error and exits at 125, and its report claims no task exit code. A task killed by a signal exits at 128 plus the signal number, and its report names the signal and claims no exit code. The last line of each block is the shell exit code of that run, printed by echo $? in the same session. A malformed call stands outside the three outcomes. Missing the root, missing the command, or naming an option mayfly does not know exits at 2 with the usage line, and no report is printed.

```
mayfly run --root /tmp/mayfly-runs -- /bin/does-not-exist
{
  "backend": "namespace",
  "run_id": "23b4fef55aae",
  "sandbox_dir": "/tmp/mayfly-runs/run-23b4fef55aae",
  "command": [
    "/bin/does-not-exist"
  ],
  "outcome": "harness_error",
  "task_exit_code": null,
  "task_signal": null,
  "error": "could not start the task, 2 No such file or directory",
  "proc_mounted": false,
  "sandbox_removed": true,
  "timings_ms": {
    "prepare": 3.149,
    "start": null,
    "task": null,
    "teardown": 7.851,
    "total": 24.992
  },
  "stdout_tail": "",
  "stderr_tail": ""
}
125
```

```
mayfly run --root /tmp/mayfly-runs -- /bin/sh -c "kill -TERM $$"
{
  "backend": "namespace",
  "run_id": "bb926005aeaa",
  "sandbox_dir": "/tmp/mayfly-runs/run-bb926005aeaa",
  "command": [
    "/bin/sh",
    "-c",
    "kill -TERM $$"
  ],
  "outcome": "task",
  "task_exit_code": null,
  "task_signal": 15,
  "error": null,
  "proc_mounted": false,
  "sandbox_removed": true,
  "timings_ms": {
    "prepare": 3.348,
    "start": 60.545,
    "task": 0.049,
    "teardown": 17.734,
    "total": 84.676
  },
  "stdout_tail": "",
  "stderr_tail": ""
}
143
```

## Status

The namespace backend works today. A run builds its sandbox, supervises the task, prints the report, and tears the sandbox down, and interrupting the command still tears the sandbox down.

Mayfly is not a micro-VM and there is no hypervisor backend. Isolation rests on Linux namespaces and the prepared root, and slice 1 sets no resource limits and no timeout on the task and has no GPU support. A janitor watches for a supervisor that dies without removing its sandbox and removes the directory, and where the janitor dies too the next run under the same root sweeps the directory away before it prepares its own. A removal that does not land leaves the directory standing for the janitor, and the report says so with sandbox_removed set to false. A command killed before its provenance is written can leave an empty sandbox directory standing under the root, and the contract names that residue rather than hiding it. A fresh proc mount inside a user namespace is refused on some machines, and on those machines the prepared root carries an empty proc directory and the report says so. Every capture in this README was taken on such a machine, which is why every report above shows proc_mounted as false.

The contract lives in CONTRACT.md, where the setup obligations S1 to S5 stand beside the full contract that landed with slice 1, obligations MF-1 to MF-12, each stated so it can fail.

Closed perfection rounds are 3 of 100. A round counts only when a full cycle of work and audit and review and red team and approval has closed, and seeding and setup are not rounds.

The local gate is cargo test plus the codepoint scan run through cargo, run on the exact final head before any push, with the continuous integration workflow running the same commands as a backstop.

The seed commit of this repo is ca5df488b41c75114a08b752aaa0ed2eae255c92, verified against the staged seed on 2026-11-02.

## License

MIT. See [LICENSE](LICENSE).
