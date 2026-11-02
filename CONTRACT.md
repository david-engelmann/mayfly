# Contract

Mayfly asks whether an agent task can run inside a disposable micro-VM that spins up fast, runs in isolation, and is destroyed the moment the task ends. This file is the stub contract for the setup slice. It states the setup obligations only, and each obligation is stated so it can fail.

Obligation S1 covers seed integrity. The seed files README.md and LICENSE and .gitignore are present from the first commit, and every later change lands through a pull request rather than a direct push to main. This is covered by inspection of the repository history and by the status section of the README.

Obligation S2 covers the local gate. The test suite runs with python and pytest alone and running it needs no network access and no paid service, and the continuous integration workflow runs the same commands as the local gate. This is covered by the tests in the tests folder and by the workflow in the .github folder.

Obligation S3 covers honest status. The README status section records the seed state and the contract state truthfully and it reports closed perfection rounds as 0 of 100 until a full cycle of work and audit and review and red team and approval has closed. This is covered by inspection of the README against this contract and against the repository history.

Obligation S4 covers synthetic content only. Every fixture and every example in this repository is synthetic and written for this repo, and no file carries a real trace or a real transcript or a real name or any private operational detail. This is covered by inspection of the repository contents before any push.

Obligation S5 covers the canonical agents file. The part of AGENTS.md above the marked line is identical to the canonical template for the program, and only the repo annex below the marked line differs for mayfly. This is covered by tests in the tests folder that pin the SHA-256 digest of the part above the marked line and check the marked line and the annex.

The full contract for mayfly is the first build act of this repo and it lands with the first slice. This stub is not the full contract, and no feature claim for mayfly rests on this stub.

## Round 1 amendment and the full contract

Amendment, recorded with Round 1. The closing paragraph of the setup stub above said the full contract lands with the first slice and that no feature claim rests on the stub. This section is that full contract landing. The stub text stays standing as the record of the setup state, and the setup obligations S1 to S5 stay in force beside the feature obligations below.

### Probe preamble

The obligations below rest on probes run against the real runtime on the build machine on 2026-11-02, before any of them was written.

The build machine has no docker and no podman and no firecracker and no jailer and no qemu and no bubblewrap, and it has no /dev/kvm, so no hardware virtualisation is available on this box. It has unshare and systemd-run and Python 3.12.3. The first backend is therefore a Linux namespace sandbox built on unshare, and this contract calls it a namespace sandbox everywhere. No obligation below claims a micro-VM, and a container backend may join behind the same lifecycle interface in a later round where docker or podman exists.

A live probe created a user namespace with map-root-user together with mount and pid and net and uts and ipc namespaces and exited 0. Inside that set, bind mounts and recursive private propagation and read only remounts and single file device binds and tmpfs mounts all succeed, and a prepared root assembled from those mounts takes a chroot and runs a task with the namespace init as pid 1 and the task as pid 2.

The same probe found one refusal. Mounting proc or sysfs or cgroup2 inside a user namespace fails with a permission error on this machine, while the same proc mount succeeds without a user namespace, so the refusal is tied to the user namespace path in this container and not to namespaces in general. The prepared root therefore carries a proc directory that stays empty where the mount is refused, and the run report records whether a fresh proc was mounted, so the record never claims a process listing isolation this machine did not grant.

A probe of exit behaviour found that unshare passes a normal exit code through exactly and reports 0 when pid 1 dies by a signal, so no claim below rests on unshare for signal honesty. The in sandbox init forks the task itself and converts a signal death into its own exit report, and the probes confirmed the init sees the exact exit code and the exact signal number through its wait.

A network probe found that a fresh network namespace lists only the loopback interface with loopback down, and a connection from inside it to a listener on the host loopback failed with network unreachable while the same connection succeeded outside the namespace. A file written on the tmpfs work directory inside the namespace was absent on the host after the namespace died, and no mount for the sandbox appeared in the host mount table at any point.

### Feature obligations

Obligation MF-1 covers the lifecycle and destruction. A run prepares one sandbox for one task, runs one command inside it, and destroys the sandbox, and after the run the sandbox directory is absent on the host and a file the task wrote inside its work directory is absent with it and no process from the task tree survives teardown. This is covered by tests that write inside the sandbox and check the host afterwards, and by a test that hands the task an inherited descriptor and proves the whole tree is dead when nothing ever writes to it after teardown.

Obligation MF-2 covers the run report and the phase timings. Every run prints a run report that names the backend and the command and the outcome and the task exit information, and the report carries a measured timing for each phase, named prepare and start and task and teardown, taken on the named backend during that run. No timing in any public copy may come from anywhere except these measured reports. This is covered by tests that run a task and read the report fields back.

Obligation MF-3 covers exit honesty. A task exit code is reported exactly and the command line exit code mirrors it, and a task killed by a signal is reported with that signal named and no exit code claimed, with the command line exiting at 128 plus the signal number. This is covered by tests with a task that exits at a chosen code and a task that kills itself with a chosen signal.

Obligation MF-4 covers failure honesty. A failure before the task starts is a harness error and never a task failure, and that covers a namespace or mount or prepared root failure and a command that cannot be executed at all. A harness error report carries no task exit code, and the command line exits at 125 for a harness error. A task that starts and itself exits at 125 or 127 is a task outcome at that code. This is covered by tests on both sides of that line.

Obligation MF-5 covers the prepared root and the working directory. The task runs inside a root prepared for that run, assembled from read only binds of the host system directories with the host layout for the merged system directories mirrored, single file binds for the basic device files, an empty proc directory or a fresh proc mount where the runtime grants one, and a work directory the task starts in. The working directory of the task is that work directory and nothing outside the prepared root is the working directory. This is covered by a test that reads the working directory from inside the task.

Obligation MF-6 covers the environment pin. The task runs with an environment scrubbed to an explicit allowlist carried over from the caller, plus a marker the harness sets itself, and nothing else. The scrubbed environment is also the environment of the supervisor processes, so no caller variable leaks to the task through a supervisor. This is covered by a test that plants a secret and a kept variable in the caller environment and reads the whole environment back from inside the task.

Obligation MF-7 covers the mount pin. The work directory inside the sandbox is a tmpfs mounted for that run and sits on its own device, the mounts are private to the namespace, and no mount for the sandbox ever appears in the host mount table. This is covered by a test that compares the device of the work directory against the device of the prepared root from inside, and a test that reads the host mount table before and after a run.

Obligation MF-8 covers the process pin. The task is a child of the namespace init at pid 1, process numbering inside the sandbox is the numbering of its own pid namespace, and no host process is addressable by its host number from inside. This is covered by a test that reads the task pid and parent pid from inside the task.

Obligation MF-9 covers the network pin. The default profile runs the task in a fresh network namespace holding only a loopback interface that is down, so no host service is reachable from the task. A run may opt in to the host network explicitly, and only then does the task share the host networking. This is covered by tests that try to reach a host loopback listener from inside both profiles, and by a test that lists the interfaces from inside the default profile.

Obligation MF-10 covers the hostname pin. The task sees the hostname mayfly inside the sandbox, whatever the host is called. This is covered by a test that reads the hostname from inside the task.

Obligation MF-11 covers backend pluggability. The harness selects a backend by name from a registry behind one lifecycle interface, an unknown backend name fails as a harness error before any sandbox is prepared, and the namespace backend is registered under the name namespace and reports that name in every run report. This is covered by a test that asks for an unknown backend and checks that nothing was prepared.

Obligation MF-12 covers the named limits, and it fails if any public copy in this repo claims past them. Slice 1 has no hypervisor and no micro-VM and no GPU support. The prepared root binds the host system directories read only, so a task sees those files but cannot write to them, and slice 1 sets no resource limits and no timeout on the task. A fresh proc mount depends on the runtime and is refused inside user namespaces on the build machine, so slice 1 claims no process listing isolation there and reports the fact per run instead. The task runs as the calling user mapped to root inside the user namespace. This is covered by inspection of the README and this contract against the code before any push.
