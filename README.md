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
cargo install --path . --bin mayfly
```

The install puts the mayfly binary in the Cargo bin directory. On a default install that directory is ~/.cargo/bin, and it must stand on your PATH for the mayfly command to resolve. The command names the mayfly binary on purpose. The repo also builds a codepoint_scan helper, and a machine that already carries that helper from a sibling repo refuses a plain install, so the named form installs mayfly alone. Mayfly runs on Linux and is wired for x86_64 and aarch64 machines.

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
mayfly run --root /tmp/mayfly-runs -- /bin/sh -c 'kill -TERM $$'
{
  "backend": "namespace",
  "run_id": "8151c89feaec",
  "sandbox_dir": "/tmp/mayfly-runs/run-8151c89feaec",
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
    "prepare": 1.503,
    "start": 11.662,
    "task": 0.609,
    "teardown": 9.489,
    "total": 33.798
  },
  "stdout_tail": "",
  "stderr_tail": ""
}
143
```

### The bounds a caller can set

A run may carry three bounds, each set by its caller and none set by default, so a run that names no bound runs under no bound. The timeout option bounds the wall clock span of the task phase in whole seconds. The max-cpu option bounds the processor time of the task in whole seconds, as a soft limit the task dies by the processor limit signal at, with a hard limit one second past it that kills a task ignoring that signal. The max-memory option bounds the address space of the task in bytes, and a task that reaches it finds its allocations refused and answers for the refusal in its own way, and no report claims the ceiling fired. A value under a floor is refused before anything is prepared, at one second for each of the two time bounds and at 16777216 bytes for the memory bound. The widest processor value the surface can carry, 18446744073709551615 seconds, is refused the same way, because its hard limit stands one second past it where no count can hold it, and the highest formable processor ceiling is 18446744073709551614 seconds. A timeout that fires ends the run as a harness error at 125 with the teardown still run, as the capture below shows.

```
mayfly run --root /tmp/mayfly-runs --timeout 2 -- /usr/bin/python3 -c "import time; time.sleep(30)"
{
  "backend": "namespace",
  "run_id": "d00e22a6a8d1",
  "sandbox_dir": "/tmp/mayfly-runs/run-d00e22a6a8d1",
  "command": [
    "/usr/bin/python3",
    "-c",
    "import time; time.sleep(30)"
  ],
  "outcome": "harness_error",
  "task_exit_code": null,
  "task_signal": null,
  "error": "the task did not finish within the timeout of 2 seconds and the sandbox was torn down",
  "proc_mounted": false,
  "sandbox_removed": true,
  "timings_ms": {
    "prepare": 0.816,
    "start": 6.816,
    "task": null,
    "teardown": 0.244,
    "total": 2064.419
  },
  "stdout_tail": "",
  "stderr_tail": ""
}
125
```

The last line is the shell exit code of that run, printed by echo $? in the same session.

## Measurements

Every figure in this section was measured on one machine, an AMD EPYC 9D25 showing 4 logical processors and 8124908 kB of memory under kernel 7.0.0-43-generic, while the machine was shared with the rest of the program fleet and other work was running on it. The figures are records of those runs on that machine under that load. They are not promises about any other machine or about this machine under another load, and no figure from this section stands in the repo description or in any claim about the tool.

The runs were taken on 2026-11-09. Each shape below ran 25 times unless its entry says otherwise, and the figures are the median and the maximum of the run totals and of the phases the shape names, taken from the run reports themselves. A median and a maximum are printed and no mean is, because a mean hides the tail a reader handing work to this tool most needs to see. The exact command of every shape is printed with its figures.

### Process startup

The task is /bin/echo carrying one word.

```
mayfly run --root /tmp/mayfly-runs -- /bin/echo hello
```

The total stands at a median of 96.085 ms and a maximum of 193.599 ms. The prepare phase stands at a median of 2.536 ms and a maximum of 60.428 ms, the start phase at a median of 24.759 ms and a maximum of 111.557 ms, and the teardown phase at a median of 12.527 ms and a maximum of 85.274 ms. Beside the sandbox runs stand 25 runs of the same command bare on the host, timed by the same monotonic clock, at a median of 2.407 ms and a maximum of 56.38 ms, so the startup overhead of the sandbox stands as the measured difference between the two totals rather than as an adjective.

### Compile shaped

The task writes a small C source into its work directory and compiles it with the system compiler standing in /usr/bin. The source is a few dozen lines written for this record and carried in the shape's command, and nothing of it is a fixture of this repo.

```
mayfly run --root /tmp/mayfly-runs -- /usr/bin/python3 -c "import subprocess, sys
src = '#include <stdio.h>\n#include <stdlib.h>\n#include <string.h>\n\nstruct point {\n    double x;\n    double y;\n};\n\nstatic double distance(const struct point *a, const struct point *b)\n{\n    double dx = a->x - b->x;\n    double dy = a->y - b->y;\n    return dx * dx + dy * dy;\n}\n\nint main(void)\n{\n    struct point points[8];\n    double total = 0.0;\n    int i;\n    for (i = 0; i < 8; i++) {\n        points[i].x = (double)i * 1.5;\n        points[i].y = (double)i * 0.25;\n    }\n    for (i = 1; i < 8; i++) {\n        total += distance(&points[i - 1], &points[i]);\n    }\n    printf(\"total %.2f over %d points\\\\n\", total, 8);\n    return total < 0.0 ? 1 : 0;\n}\n'
open('shape.c', 'w').write(src)
done = subprocess.run(['/usr/bin/cc', '-O2', '-o', 'shape', 'shape.c'])
sys.exit(done.returncode)"
```

The total stands at a median of 438.56 ms and a maximum of 997.719 ms, and the task phase at a median of 383.241 ms and a maximum of 891.954 ms.

### IO shaped

The task writes 64 megabytes into a file in its work directory, reads the file back, and prints a checksum of what it read.

```
mayfly run --root /tmp/mayfly-runs -- /usr/bin/python3 -c "import sys, zlib
chunk = bytes(range(256)) * 4096
with open('data.bin', 'wb') as f:
    for _ in range(64):
        f.write(chunk)
total = 0
with open('data.bin', 'rb') as f:
    while True:
        part = f.read(1 << 20)
        if not part:
            break
        total = zlib.crc32(part, total)
print('checksum', total)
"
```

The total stands at a median of 297.502 ms and a maximum of 500.734 ms, and the task phase at a median of 208.677 ms and a maximum of 339.421 ms.

### Search shaped

The task builds a tree of one thousand small files in its work directory, one file in ten carrying the term mayfly-needle, and then greps that tree for the term. The tree is built inside the same task because a sandbox holds nothing between runs for a later run to search, so the task phase below covers the building and the search together.

```
mayfly run --root /tmp/mayfly-runs -- /usr/bin/python3 -c "import os, subprocess, sys
os.makedirs('tree', exist_ok=True)
for i in range(1000):
    text = 'filler line for file %d\n' % i
    if i % 10 == 0:
        text += 'mayfly-needle\n'
    open('tree/file-%04d.txt' % i, 'w').write(text)
done = subprocess.run(['grep', '-r', '-l', 'mayfly-needle', 'tree'], capture_output=True, text=True)
print('files carrying the term', len(done.stdout.splitlines()))
sys.exit(done.returncode)"
```

The total stands at a median of 194.23 ms and a maximum of 357.673 ms, and the task phase at a median of 136.143 ms and a maximum of 269.235 ms.

### The bounds demonstrated

Three shapes ran once each, because each demonstrates a bound rather than a distribution. A sleeping task under a timeout of 2 seconds ended as a harness error with the error line naming the timeout, no task phase timing, and a total of 2025.39 ms, at a command line exit of 125.

```
mayfly run --root /tmp/mayfly-runs --timeout 2 -- /usr/bin/python3 -c "import time; time.sleep(30)"
```

An allocating task under an address space ceiling of 268435456 bytes, trying to allocate three times that space, met the refusal and answered at the exit code 42 it chose for it, as a task outcome.

```
mayfly run --root /tmp/mayfly-runs --max-memory 268435456 -- /usr/bin/python3 -c "import sys
try:
    blob = bytearray(3 * 268435456)
    sys.exit(0)
except MemoryError:
    sys.exit(42)"
```

A busy task under a processor ceiling of 1 second died by the processor limit signal, signal 24, as a task outcome, at a command line exit of 152.

```
mayfly run --root /tmp/mayfly-runs --max-cpu 1 -- /usr/bin/python3 -c "exec('while True:' + chr(10) + '    pass')"
```

## Status

The namespace backend works today. A run builds its sandbox, supervises the task, prints the report, and tears the sandbox down, and interrupting the command still tears the sandbox down.

Mayfly is not a micro-VM and there is no hypervisor backend. Isolation rests on Linux namespaces and the prepared root, and a run may carry a wall clock timeout and processor and address space ceilings its caller sets, and slice 1 has no GPU support. A janitor watches for a supervisor that dies without removing its sandbox and removes the directory, and where the janitor dies too the next run under the same root sweeps the directory away before it prepares its own. A removal that does not land leaves the directory standing for the janitor, and the report says so with sandbox_removed set to false. A command killed before its provenance is written can leave an empty sandbox directory standing under the root, and the contract names that residue rather than hiding it. A fresh proc mount inside a user namespace is refused on some machines, and on those machines the prepared root carries an empty proc directory and the report says so. Every capture in this README was taken on such a machine, which is why every report above shows proc_mounted as false.

The contract lives in CONTRACT.md, where the setup obligations S1 to S5 stand beside the full contract that landed with slice 1, obligations MF-1 to MF-12, each stated so it can fail.

Closed perfection rounds are 15 of 100. A round counts only when a full cycle of work and audit and review and red team and approval has closed, and seeding and setup are not rounds.

The local gate is cargo test plus the codepoint scan run through cargo, run on the exact final head before any push, with the continuous integration workflow running the same commands as a backstop.

The seed commit of this repo is ca5df488b41c75114a08b752aaa0ed2eae255c92, verified against the staged seed on 2026-11-02.

## License

MIT. See [LICENSE](LICENSE).
