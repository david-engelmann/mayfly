import json
import os
import re
import select
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from mayfly import HarnessError, run_task
from mayfly.backends import namespace as namespace_module
from mayfly.backends.base import RunReport, RunSpec
from mayfly.backends.namespace import NamespaceBackend

pytestmark = pytest.mark.skipif(
    shutil.which("unshare") is None, reason="unshare is not available on this machine"
)

PY = sys.executable

FORCE_REFUSAL_SWITCH = "MAYFLY_TEST_FORCE_SANDBOX_REFUSAL"


def _probe_failure_is_host_refusal(report):
    """Whether a failed probe run is the host refusing the sandbox.

    The refusal this guards against is the one the integration
    host presents, where unshare cannot write the user
    namespace id map and the sandbox never starts, or where
    unshare cannot run at all. The signature is the failure
    naming that refusal. A failure naming anything else means
    the namespace machinery started and something in the
    sandbox broke, which is a defect in the slice and not a
    property of the host.
    """
    text = " ".join(
        part for part in (report.error, report.stderr_tail) if part
    ).lower()
    if "uid_map" in text:
        return True
    if "unshare" in text and (
        "not permitted" in text or "permission denied" in text
    ):
        return True
    return False


def _probe_sandbox():
    """Ask whether this host can start a real sandbox at all.

    One trivial task runs through the real backend. Where the
    run ends in a task outcome the host can start a sandbox and
    the answer is (True, an empty refusal). Where it does not,
    the failure is classified. A failure carrying the host
    refusal signature answers (False, the refusal the failed
    run reported), so a skip reason names the refusal the host
    actually gave, the uid map refusal on a host that forbids
    the user namespace mapping above all. Any other failure
    raises, because a probe that took it for a refusal would
    skip the guarded suite green over a broken sandbox, in
    this environment and in continuous integration alike. A
    suite that cannot tell a host that forbids the sandbox
    from a sandbox that is broken fails wholesale on the first
    kind of host and says nothing true about either. The
    MAYFLY_TEST_FORCE_SANDBOX_REFUSAL switch is a test only
    hook that forces the refusal answer, so the guards below
    can be shown to fire.
    """
    if os.environ.get(FORCE_REFUSAL_SWITCH):
        return False, "the probe was forced to report a refusal by " + FORCE_REFUSAL_SWITCH
    probe_root = Path(tempfile.mkdtemp(prefix="mayfly-probe-"))
    try:
        report = run_task([PY, "-c", "pass"], probe_root)
    finally:
        shutil.rmtree(probe_root, ignore_errors=True)
    if report.outcome == "task":
        return True, ""
    refusal = " ".join(
        part for part in (report.error, report.stderr_tail) if part
    ).strip()
    if _probe_failure_is_host_refusal(report):
        return False, refusal or "the sandbox did not start and gave no reason"
    raise AssertionError(
        "the sandbox probe failed for a reason that is not a host refusal, "
        "so the guarded tests would be skipped over a broken sandbox, "
        + (refusal or "the failed run gave no reason")
    )


_SANDBOX_OK, _SANDBOX_REFUSAL = _probe_sandbox()

requires_sandbox = pytest.mark.skipif(
    not _SANDBOX_OK,
    reason="this host refuses to start a sandbox, " + _SANDBOX_REFUSAL,
)


def _synthetic_probe_report(error=None, stderr_tail=""):
    return RunReport(
        backend="namespace",
        run_id="probe0000000",
        sandbox_dir="/nonexistent",
        command=[PY, "-c", "pass"],
        outcome="harness_error",
        error=error,
        stderr_tail=stderr_tail,
    )


def test_the_probe_takes_the_uid_map_refusal_for_a_host_refusal():
    report = _synthetic_probe_report(
        error="the sandbox exited without starting the task, unshare exit 1",
        stderr_tail=(
            "unshare: write failed /proc/self/uid_map: Operation not permitted"
        ),
    )
    assert _probe_failure_is_host_refusal(report) is True
    report = _synthetic_probe_report(
        error="could not launch unshare, [Errno 13] Permission denied"
    )
    assert _probe_failure_is_host_refusal(report) is True


def test_the_probe_takes_any_other_failure_for_a_broken_sandbox():
    broken_shapes = [
        _synthetic_probe_report(
            error=(
                "the prepared root failed, "
                "[Errno 2] No such file or directory: '/probe/rootfs'"
            )
        ),
        _synthetic_probe_report(
            error="could not launch unshare, [Errno 2] No such file or directory"
        ),
        _synthetic_probe_report(
            error="the sandbox exited without starting the task, unshare exit 1",
            stderr_tail="the init died on a name the driver never sent",
        ),
        _synthetic_probe_report(),
    ]
    for report in broken_shapes:
        assert _probe_failure_is_host_refusal(report) is False, report.error


def test_the_probe_skips_on_a_refusal_and_fails_loudly_on_a_break(monkeypatch):
    monkeypatch.delenv(FORCE_REFUSAL_SWITCH, raising=False)
    module = sys.modules[__name__]
    refusal = _synthetic_probe_report(
        error="the sandbox exited without starting the task, unshare exit 1",
        stderr_tail=(
            "unshare: write failed /proc/self/uid_map: Operation not permitted"
        ),
    )
    monkeypatch.setattr(module, "run_task", lambda command, root: refusal)
    ok, reason = _probe_sandbox()
    assert ok is False
    assert "uid_map" in reason
    broken = _synthetic_probe_report(
        error="the prepared root failed, [Errno 2] No such file or directory"
    )
    monkeypatch.setattr(module, "run_task", lambda command, root: broken)
    with pytest.raises(AssertionError):
        _probe_sandbox()


def last_json(report):
    lines = [line for line in report.stdout_tail.strip().splitlines() if line.strip()]
    return json.loads(lines[-1])


@requires_sandbox
def test_run_report_carries_the_phases_and_the_outcome(tmp_path):
    report = run_task([PY, "-c", "import time; time.sleep(0.3); print('hello')"], tmp_path)
    assert report.backend == "namespace"
    assert report.outcome == "task"
    assert report.task_exit_code == 0
    assert report.task_signal is None
    assert "hello" in report.stdout_tail
    for phase in ("prepare", "start", "task", "teardown", "total"):
        assert phase in report.timings_ms, phase
        assert report.timings_ms[phase] > 0, phase
    assert report.timings_ms["task"] >= 200
    assert isinstance(report.proc_mounted, bool)


@requires_sandbox
def test_task_exit_code_is_reported_exactly(tmp_path):
    report = run_task([PY, "-c", "import sys; sys.exit(7)"], tmp_path)
    assert report.outcome == "task"
    assert report.task_exit_code == 7
    assert report.task_signal is None


@requires_sandbox
def test_task_killed_by_a_signal_is_reported_as_a_signal(tmp_path):
    report = run_task(
        [PY, "-c", "import os, signal; os.kill(os.getpid(), signal.SIGTERM)"], tmp_path
    )
    assert report.outcome == "task"
    assert report.task_exit_code is None
    assert report.task_signal == 15


@requires_sandbox
def test_a_task_exiting_at_125_is_a_task_outcome(tmp_path):
    report = run_task([PY, "-c", "import sys; sys.exit(125)"], tmp_path)
    assert report.outcome == "task"
    assert report.task_exit_code == 125


def test_a_command_that_cannot_start_is_a_harness_error(tmp_path):
    report = run_task(["mayfly-no-such-binary-anywhere"], tmp_path)
    assert report.outcome == "harness_error"
    assert report.task_exit_code is None
    assert report.task_signal is None
    assert report.error


def test_a_missing_unshare_is_a_harness_error(tmp_path):
    backend = NamespaceBackend(unshare_path="/nonexistent/unshare")
    report = backend.run(RunSpec(command=[PY, "-c", "pass"], root=tmp_path))
    assert report.outcome == "harness_error"
    assert report.task_exit_code is None
    assert report.sandbox_removed is True
    assert list(tmp_path.iterdir()) == []


def test_an_unknown_backend_fails_before_anything_is_prepared(tmp_path):
    with pytest.raises(HarnessError):
        run_task([PY, "-c", "pass"], tmp_path, backend="no-such-backend")
    assert list(tmp_path.iterdir()) == []


@requires_sandbox
def test_the_task_starts_in_the_work_directory_of_a_prepared_root(tmp_path):
    script = (
        "import json, os, socket\n"
        "print(json.dumps({\n"
        "  'cwd': os.getcwd(),\n"
        "  'pid': os.getpid(),\n"
        "  'ppid': os.getppid(),\n"
        "  'hostname': socket.gethostname(),\n"
        "  'work_dev': os.stat('.').st_dev,\n"
        "  'root_dev': os.stat('/').st_dev,\n"
        "}))\n"
    )
    report = run_task([PY, "-c", script], tmp_path)
    seen = last_json(report)
    assert seen["cwd"] == "/work"
    assert seen["ppid"] == 1
    assert 1 < seen["pid"] < 100
    assert seen["hostname"] == "mayfly"
    assert seen["work_dev"] != seen["root_dev"]


@requires_sandbox
def test_the_system_directories_are_read_only_inside(tmp_path):
    script = (
        "import json, os\n"
        "path = '/usr/.mayfly-write-probe'\n"
        "errno = 0\n"
        "try:\n"
        "    handle = open(path, 'w')\n"
        "    handle.close()\n"
        "    os.unlink(path)\n"
        "except OSError as exc:\n"
        "    errno = exc.errno\n"
        "print(json.dumps({'usr_write_errno': errno}))\n"
    )
    report = run_task([PY, "-c", script], tmp_path)
    seen = last_json(report)
    assert seen["usr_write_errno"] == 30


@requires_sandbox
def test_the_environment_is_scrubbed_to_the_allowlist(tmp_path, monkeypatch):
    monkeypatch.setenv("PATH", "/usr/bin:/bin")
    monkeypatch.setenv("MAYFLY_LEAK_PROBE", "a secret value")
    monkeypatch.setenv("MAYFLY_KEEP_ME", "a kept value")
    report = run_task(
        ["/usr/bin/env"], tmp_path, allow_env=("PATH", "MAYFLY_KEEP_ME")
    )
    seen = {}
    for line in report.stdout_tail.strip().splitlines():
        if "=" in line:
            key, _, value = line.partition("=")
            seen[key] = value
    assert seen == {
        "PATH": "/usr/bin:/bin",
        "MAYFLY_KEEP_ME": "a kept value",
        "MAYFLY_SANDBOX": "namespace",
        "PYTHONCOERCECLOCALE": "0",
    }


@requires_sandbox
def test_a_file_written_inside_is_gone_with_the_sandbox(tmp_path):
    script = (
        "import json, os\n"
        "with open('proof.txt', 'w') as handle:\n"
        "    handle.write('written inside the sandbox')\n"
        "print(json.dumps({'wrote': os.path.exists('proof.txt')}))\n"
    )
    report = run_task([PY, "-c", script], tmp_path)
    assert last_json(report)["wrote"] is True
    assert report.sandbox_removed is True
    assert not Path(report.sandbox_dir).exists()
    assert list(tmp_path.rglob("proof.txt")) == []


@requires_sandbox
def test_no_process_from_the_task_tree_survives_teardown(tmp_path):
    read_fd, write_fd = os.pipe()
    script = (
        "import os, sys, time\n"
        "fd = int(sys.argv[1])\n"
        "pid = os.fork()\n"
        "if pid == 0:\n"
        "    os.setsid()\n"
        "    if os.fork() == 0:\n"
        "        time.sleep(2)\n"
        "        os.write(fd, b'ALIVE')\n"
        "    os._exit(0)\n"
        "os.waitpid(pid, 0)\n"
    )
    report = run_task([PY, "-c", script, str(write_fd)], tmp_path, pass_fds=(write_fd,))
    os.close(write_fd)
    assert report.outcome == "task"
    assert report.task_exit_code == 0
    data = b""
    eof = False
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        ready, _, _ = select.select([read_fd], [], [], 0.2)
        if not ready:
            continue
        chunk = os.read(read_fd, 4096)
        if chunk == b"":
            eof = True
            break
        data += chunk
    os.close(read_fd)
    assert data == b""
    assert eof


def test_no_sandbox_mount_appears_in_the_host_mount_table(tmp_path):
    before = Path("/proc/self/mounts").read_text(encoding="utf-8")
    report = run_task([PY, "-c", "pass"], tmp_path)
    after = Path("/proc/self/mounts").read_text(encoding="utf-8")
    assert report.sandbox_dir not in before
    assert report.sandbox_dir not in after


class LoopbackListener:
    def __init__(self):
        self.sock = socket.socket()
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.sock.bind(("127.0.0.1", 0))
        self.sock.listen(5)
        self.port = self.sock.getsockname()[1]
        self.received = []
        self.thread = threading.Thread(target=self._serve, daemon=True)

    def _serve(self):
        while True:
            try:
                conn, _ = self.sock.accept()
            except OSError:
                return
            self.received.append(conn.recv(16))
            conn.close()

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *exc):
        self.sock.close()
        return False


CONNECT_SCRIPT = (
    "import json, socket, sys\n"
    "sock = socket.socket()\n"
    "sock.settimeout(3)\n"
    "try:\n"
    "    sock.connect(('127.0.0.1', int(sys.argv[1])))\n"
    "    sock.sendall(b'ping')\n"
    "    print(json.dumps({'connected': True}))\n"
    "except OSError as exc:\n"
    "    print(json.dumps({'connected': False, 'errno': exc.errno}))\n"
)


@requires_sandbox
def test_the_default_profile_has_no_network(tmp_path):
    with LoopbackListener() as listener:
        report = run_task([PY, "-c", CONNECT_SCRIPT, str(listener.port)], tmp_path)
        seen = last_json(report)
        assert seen["connected"] is False
        assert seen["errno"] == 101
        time.sleep(0.3)
        assert listener.received == []


@requires_sandbox
def test_the_default_profile_lists_only_loopback(tmp_path):
    script = "import json, socket; print(json.dumps([name for _, name in socket.if_nameindex()]))"
    report = run_task([PY, "-c", script], tmp_path)
    assert last_json(report) == ["lo"]


@requires_sandbox
def test_the_network_opt_in_shares_the_host_network(tmp_path):
    with LoopbackListener() as listener:
        report = run_task(
            [PY, "-c", CONNECT_SCRIPT, str(listener.port)], tmp_path, network=True
        )
        seen = last_json(report)
        assert seen["connected"] is True
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline and not listener.received:
            time.sleep(0.05)
        assert listener.received == [b"ping"]


def run_cli(args, root):
    env = dict(os.environ)
    env["PYTHONPATH"] = str(ROOT)
    return subprocess.run(
        [PY, "-m", "mayfly"] + args,
        capture_output=True,
        text=True,
        cwd=ROOT,
        env=env,
    )


@requires_sandbox
def test_cli_mirrors_the_task_exit_code(tmp_path):
    result = run_cli(
        ["run", "--root", str(tmp_path), "--", PY, "-c", "import sys; sys.exit(7)"],
        tmp_path,
    )
    assert result.returncode == 7
    payload = json.loads(result.stdout)
    assert payload["outcome"] == "task"
    assert payload["task_exit_code"] == 7
    assert payload["backend"] == "namespace"


@requires_sandbox
def test_cli_reports_a_signal_death_at_128_plus_the_signal(tmp_path):
    result = run_cli(
        [
            "run",
            "--root",
            str(tmp_path),
            "--",
            PY,
            "-c",
            "import os, signal; os.kill(os.getpid(), signal.SIGTERM)",
        ],
        tmp_path,
    )
    assert result.returncode == 143
    payload = json.loads(result.stdout)
    assert payload["task_signal"] == 15
    assert payload["task_exit_code"] is None


def test_cli_reports_a_harness_error_at_125(tmp_path):
    result = run_cli(
        ["run", "--root", str(tmp_path), "--", "mayfly-no-such-binary-anywhere"],
        tmp_path,
    )
    assert result.returncode == 125
    payload = json.loads(result.stdout)
    assert payload["outcome"] == "harness_error"
    assert payload["task_exit_code"] is None


def test_contract_carries_the_round_one_obligations():
    text = (ROOT / "CONTRACT.md").read_text(encoding="utf-8")
    for number in range(1, 13):
        assert f"Obligation MF-{number} covers" in text, number
    assert "no /dev/kvm" in text
    assert "namespace sandbox" in text
    readme = (ROOT / "README.md").read_text(encoding="utf-8")
    assert "namespace sandbox" in readme


@requires_sandbox
def test_the_stderr_tail_carries_a_failing_task_stderr(tmp_path):
    script = "import sys; sys.stderr.write('a named failure line' + chr(10)); sys.exit(3)"
    report = run_task([PY, "-c", script], tmp_path)
    assert report.outcome == "task"
    assert report.task_exit_code == 3
    assert "a named failure line" in report.stderr_tail


@requires_sandbox
def test_a_python_task_sees_the_scrubbed_environment_through_its_interpreter(
    tmp_path, monkeypatch
):
    monkeypatch.setenv("PATH", "/usr/bin:/bin")
    monkeypatch.setenv("MAYFLY_LEAK_PROBE", "a secret value")
    script = "import json, os; print(json.dumps(dict(os.environ)))"
    report = run_task([PY, "-c", script], tmp_path)
    seen = last_json(report)
    assert seen == {
        "PATH": "/usr/bin:/bin",
        "MAYFLY_SANDBOX": "namespace",
        "PYTHONCOERCECLOCALE": "0",
    }


@requires_sandbox
def test_an_allowlisted_coercion_key_keeps_the_caller_value(tmp_path, monkeypatch):
    monkeypatch.setenv("PATH", "/usr/bin:/bin")
    monkeypatch.setenv("PYTHONCOERCECLOCALE", "caller-chosen")
    report = run_task(
        ["/usr/bin/env"], tmp_path, allow_env=("PATH", "PYTHONCOERCECLOCALE")
    )
    seen = {}
    for line in report.stdout_tail.strip().splitlines():
        if "=" in line:
            key, _, value = line.partition("=")
            seen[key] = value
    assert seen["PYTHONCOERCECLOCALE"] == "caller-chosen"


CAPS_SCRIPT = (
    "import ctypes, json\n"
    "libc = ctypes.CDLL(None, use_errno=True)\n"
    "bounding = [cap for cap in range(64) if libc.prctl(23, cap, 0, 0, 0) == 1]\n"
    "class Hdr(ctypes.Structure):\n"
    "    _fields_ = [('version', ctypes.c_uint32), ('pid', ctypes.c_int)]\n"
    "class Data(ctypes.Structure):\n"
    "    _fields_ = [('effective', ctypes.c_uint32), ('permitted', ctypes.c_uint32), ('inheritable', ctypes.c_uint32)]\n"
    "hdr = Hdr(0x20080522, 0)\n"
    "data = (Data * 2)()\n"
    "libc.capget(ctypes.byref(hdr), data)\n"
    "print(json.dumps({\n"
    "  'bounding': bounding,\n"
    "  'effective': data[0].effective | (data[1].effective << 32),\n"
    "  'permitted': data[0].permitted | (data[1].permitted << 32),\n"
    "}))\n"
)


@requires_sandbox
def test_the_task_holds_no_capabilities(tmp_path):
    report = run_task([PY, "-c", CAPS_SCRIPT], tmp_path)
    seen = last_json(report)
    assert seen["bounding"] == []
    assert seen["effective"] == 0
    assert seen["permitted"] == 0


ESCAPE_SCRIPT = (
    "import json, os, sys\n"
    "secret_path, host_target = sys.argv[1], sys.argv[2]\n"
    "result = {'chroot_ok': False, 'secret_read': False, 'host_write': False}\n"
    "os.makedirs('/work/trap', exist_ok=True)\n"
    "root_fd = os.open('/', os.O_RDONLY)\n"
    "try:\n"
    "    os.chroot('/work/trap')\n"
    "    result['chroot_ok'] = True\n"
    "except OSError:\n"
    "    pass\n"
    "if result['chroot_ok']:\n"
    "    os.fchdir(root_fd)\n"
    "    for _ in range(24):\n"
    "        os.chdir('..')\n"
    "    try:\n"
    "        os.chroot('.')\n"
    "    except OSError:\n"
    "        pass\n"
    "    try:\n"
    "        with open(secret_path) as handle:\n"
    "            result['secret_read'] = 'mayfly-host-secret' in handle.read()\n"
    "    except OSError:\n"
    "        pass\n"
    "    try:\n"
    "        with open(host_target, 'w') as handle:\n"
    "            handle.write('escaped')\n"
    "        result['host_write'] = True\n"
    "    except OSError:\n"
    "        pass\n"
    "print(json.dumps(result))\n"
)


@requires_sandbox
def test_the_chroot_climb_escape_shape_cannot_reach_the_host(tmp_path):
    secret = tmp_path / "host-secret.txt"
    secret.write_text("mayfly-host-secret", encoding="utf-8")
    host_target = tmp_path / "escaped-write.txt"
    report = run_task(
        [PY, "-c", ESCAPE_SCRIPT, str(secret), str(host_target)], tmp_path
    )
    seen = last_json(report)
    assert seen["chroot_ok"] is False
    assert seen["secret_read"] is False
    assert seen["host_write"] is False
    assert not host_target.exists()


@requires_sandbox
def test_a_submount_of_a_bound_tree_is_read_only_inside(tmp_path):
    import uuid

    probe_dir = Path("/etc") / ("mayfly-subprobe-" + uuid.uuid4().hex[:12])
    try:
        probe_dir.mkdir()
    except PermissionError:
        pytest.skip("this machine would not let the probe create a directory under /etc")
    mounted = subprocess.run(
        ["mount", "-t", "tmpfs", "tmpfs", str(probe_dir)],
        capture_output=True,
    ).returncode
    if mounted != 0:
        probe_dir.rmdir()
        pytest.skip("this machine would not create a host submount to probe")
    try:
        script = (
            "import json, os\n"
            "errno = 0\n"
            "try:\n"
            "    handle = open(" + repr(str(probe_dir / "probe.txt")) + ", 'w')\n"
            "    handle.close()\n"
            "except OSError as exc:\n"
            "    errno = exc.errno\n"
            "print(json.dumps({'errno': errno}))\n"
        )
        report = run_task([PY, "-c", script], tmp_path)
        assert report.outcome == "task"
        assert last_json(report)["errno"] == 30
        assert not (probe_dir / "probe.txt").exists()
    finally:
        subprocess.run(["umount", str(probe_dir)], capture_output=True)
        probe_dir.rmdir()


def test_a_command_with_a_nul_byte_is_a_harness_error(tmp_path):
    report = run_task(["mayfly-bad\0command"], tmp_path)
    assert report.outcome == "harness_error"
    assert report.task_exit_code is None
    assert report.error


def test_a_prepare_failure_report_carries_the_full_shape(tmp_path):
    blocker = tmp_path / "a-file-not-a-directory"
    blocker.write_text("standing in the way", encoding="utf-8")
    report = run_task([PY, "-c", "pass"], blocker)
    assert report.outcome == "harness_error"
    assert report.error
    assert report.proc_mounted is False
    assert isinstance(report.sandbox_removed, bool)
    for phase in ("prepare", "start", "task", "teardown", "total"):
        assert phase in report.timings_ms, phase


@requires_sandbox
def test_a_large_task_output_is_bounded_to_its_tail(tmp_path):
    from mayfly.backends.namespace import TAIL_CHARS

    script = (
        "import sys\n"
        "sys.stdout.write('x' * 300000)\n"
        "sys.stdout.write('the-end-marker')\n"
    )
    report = run_task([PY, "-c", script], tmp_path)
    assert report.outcome == "task"
    assert len(report.stdout_tail) <= TAIL_CHARS
    assert report.stdout_tail.endswith("the-end-marker")


def _host_pids_with_marker(marker):
    pids = []
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            cmdline = Path("/proc", entry, "cmdline").read_bytes()
        except OSError:
            continue
        if marker.encode() in cmdline:
            pids.append(int(entry))
    return pids


def _wait_until(predicate, timeout):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(0.1)
    return predicate()


def _start_cli_with_sleeping_task(root, marker):
    script = "import time\n# " + marker + "\ntime.sleep(60)\n"
    env = dict(os.environ)
    env["PYTHONPATH"] = str(ROOT)
    proc = subprocess.Popen(
        [PY, "-m", "mayfly", "run", "--root", str(root), "--", PY, "-c", script],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        cwd=ROOT,
        env=env,
    )
    started = _wait_until(
        lambda: len(_host_pids_with_marker(marker)) >= 4 and list(root.glob("run-*")),
        30,
    )
    assert started, "the sleeping task never came up"
    return proc


@requires_sandbox
def test_sigterm_to_the_supervisor_still_tears_the_sandbox_down(tmp_path):
    import signal as signal_module
    import uuid

    marker = "mayfly-death-" + uuid.uuid4().hex
    proc = _start_cli_with_sleeping_task(tmp_path, marker)
    proc.send_signal(signal_module.SIGTERM)
    out, _ = proc.communicate(timeout=30)
    assert proc.returncode == 125
    payload = json.loads(out)
    assert payload["outcome"] == "harness_error"
    assert _wait_until(lambda: not _host_pids_with_marker(marker), 15)
    assert list(tmp_path.glob("run-*")) == []


@requires_sandbox
def test_sigkill_to_the_supervisor_leaves_no_process_and_no_sandbox(tmp_path):
    import signal as signal_module
    import uuid

    marker = "mayfly-death-" + uuid.uuid4().hex
    proc = _start_cli_with_sleeping_task(tmp_path, marker)
    proc.send_signal(signal_module.SIGKILL)
    proc.wait(timeout=30)
    assert _wait_until(lambda: not _host_pids_with_marker(marker), 30)
    assert _wait_until(lambda: list(tmp_path.glob("run-*")) == [], 30)
    assert list(tmp_path.glob(".mayfly-clean-*")) == []


def _pids_running_script(script_name, extra_needle=None):
    """Pids running a mayfly helper script as their own program.

    The script must be the second word of the command line, the
    program the process was started with, because the unshare
    process also names the init script further along its own
    command line without being the init.
    """
    pids = []
    tail = os.fsencode("/" + script_name)
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            cmdline = Path("/proc", entry, "cmdline").read_bytes()
        except OSError:
            continue
        parts = cmdline.split(b"\0")
        if len(parts) < 2 or not parts[1].endswith(tail):
            continue
        if extra_needle is not None and extra_needle not in cmdline:
            continue
        pids.append(int(entry))
    return pids


def _age_directory(path, seconds=60):
    old = time.time() - seconds
    os.utime(path, (old, old))


def _let_the_run_get_under_way():
    """Give a started run one clear moment before a pin acts on it.

    The death pins are about a run whose task is under way and
    whose started event has reached the runner. The task process
    becomes visible a breath before the init writes that event, so
    a pin that acts the instant the task appears can land inside
    the breath and test a different scenario than it names.
    """
    time.sleep(1)


def _plant_marker_and_sigkill(tmp_path, marker_content):
    import signal as signal_module
    import uuid

    marker = "mayfly-death-" + uuid.uuid4().hex
    proc = _start_cli_with_sleeping_task(tmp_path, marker)
    sandboxes = list(tmp_path.glob("run-*"))
    assert len(sandboxes) == 1
    run_id = sandboxes[0].name[len("run-"):]
    planted = tmp_path / (".mayfly-clean-" + run_id)
    planted.write_text(marker_content, encoding="utf-8")
    proc.send_signal(signal_module.SIGKILL)
    proc.wait(timeout=30)
    assert _wait_until(lambda: not _host_pids_with_marker(marker), 30)
    assert _wait_until(lambda: list(tmp_path.glob("run-*")) == [], 30)
    assert list(tmp_path.glob(".mayfly-clean-*")) == []


@requires_sandbox
def test_a_planted_clean_marker_does_not_stand_the_janitor_down(tmp_path):
    _plant_marker_and_sigkill(tmp_path, "planted by anyone who can list the root\n")


@requires_sandbox
def test_a_forged_token_in_a_planted_marker_does_not_stand_the_janitor_down(tmp_path):
    _plant_marker_and_sigkill(tmp_path, "0" * 32 + "\n")


@requires_sandbox
def test_a_clean_run_leaves_no_marker_and_no_sandbox(tmp_path):
    report = run_task([PY, "-c", "print('done')"], tmp_path)
    assert report.outcome == "task"
    assert report.sandbox_removed is True
    assert list(tmp_path.iterdir()) == []


@requires_sandbox
def test_a_later_run_sweeps_an_orphan_whose_janitor_also_died(tmp_path):
    import signal as signal_module
    import uuid

    marker = "mayfly-death-" + uuid.uuid4().hex
    proc = _start_cli_with_sleeping_task(tmp_path, marker)
    sandboxes = list(tmp_path.glob("run-*"))
    assert len(sandboxes) == 1
    sandbox = sandboxes[0]
    janitors = _pids_running_script("_janitor.py", os.fsencode(str(sandbox)))
    assert len(janitors) == 1
    os.kill(janitors[0], signal_module.SIGKILL)
    proc.send_signal(signal_module.SIGKILL)
    proc.wait(timeout=30)
    assert _wait_until(lambda: not _host_pids_with_marker(marker), 30)
    assert sandbox.exists()
    _age_directory(sandbox)
    report = run_task([PY, "-c", "pass"], tmp_path)
    assert report.outcome == "task"
    assert not sandbox.exists()
    assert list(tmp_path.iterdir()) == []


@requires_sandbox
def test_the_sweep_never_touches_a_live_run(tmp_path):
    import signal as signal_module
    import uuid

    marker = "mayfly-death-" + uuid.uuid4().hex
    proc = _start_cli_with_sleeping_task(tmp_path, marker)
    sandboxes = list(tmp_path.glob("run-*"))
    assert len(sandboxes) == 1
    sandbox = sandboxes[0]
    _age_directory(sandbox)
    report = run_task([PY, "-c", "pass"], tmp_path)
    assert report.outcome == "task"
    assert sandbox.exists()
    assert _host_pids_with_marker(marker)
    proc.send_signal(signal_module.SIGTERM)
    out, _ = proc.communicate(timeout=30)
    assert proc.returncode == 125
    assert json.loads(out)["outcome"] == "harness_error"
    assert _wait_until(lambda: not _host_pids_with_marker(marker), 15)
    assert list(tmp_path.glob("run-*")) == []


@requires_sandbox
def test_a_sandbox_killed_mid_task_does_not_claim_the_task_never_started(tmp_path):
    import signal as signal_module
    import uuid

    marker = "mayfly-death-" + uuid.uuid4().hex
    proc = _start_cli_with_sleeping_task(tmp_path, marker)
    _let_the_run_get_under_way()
    sandboxes = list(tmp_path.glob("run-*"))
    assert len(sandboxes) == 1
    inits = _pids_running_script("_init.py", os.fsencode(str(sandboxes[0])))
    assert len(inits) == 1
    os.kill(inits[0], signal_module.SIGKILL)
    out, _ = proc.communicate(timeout=30)
    assert proc.returncode == 125
    payload = json.loads(out)
    assert payload["outcome"] == "harness_error"
    assert "without starting the task" not in payload["error"]
    assert "after the task started" in payload["error"]
    assert payload["timings_ms"]["task"] is None
    assert _wait_until(lambda: not _host_pids_with_marker(marker), 15)
    assert list(tmp_path.glob("run-*")) == []


def test_a_sandbox_that_dies_before_the_task_starts_keeps_the_plain_report(tmp_path):
    fake_unshare = tmp_path / "fake-unshare"
    fake_unshare.write_text("#!/bin/sh\nexit 3\n", encoding="utf-8")
    fake_unshare.chmod(0o755)
    root = tmp_path / "root"
    backend = NamespaceBackend(unshare_path=str(fake_unshare))
    report = backend.run(RunSpec(command=[PY, "-c", "pass"], root=root))
    assert report.outcome == "harness_error"
    assert "without starting the task" in report.error
    assert report.timings_ms["start"] is None
    assert report.timings_ms["task"] is None
    assert report.sandbox_removed is True
    assert list(root.iterdir()) == []


@requires_sandbox
def test_an_interrupted_run_reports_the_unfinished_task_phase_as_null(tmp_path):
    import signal as signal_module
    import uuid

    marker = "mayfly-death-" + uuid.uuid4().hex
    proc = _start_cli_with_sleeping_task(tmp_path, marker)
    _let_the_run_get_under_way()
    proc.send_signal(signal_module.SIGINT)
    out, _ = proc.communicate(timeout=30)
    assert proc.returncode == 125
    payload = json.loads(out)
    assert payload["outcome"] == "harness_error"
    assert payload["timings_ms"]["task"] is None
    assert payload["timings_ms"]["start"] > 0
    assert payload["timings_ms"]["teardown"] > 0
    assert payload["timings_ms"]["total"] > 0
    assert _wait_until(lambda: not _host_pids_with_marker(marker), 15)
    assert list(tmp_path.glob("run-*")) == []


def test_the_supported_architectures_are_the_documented_ones():
    init_source = (ROOT / "mayfly" / "_init.py").read_text(encoding="utf-8")
    expected = {
        "SYS_PIVOT_ROOT": {"x86_64": 155, "aarch64": 41},
        "SYS_MOUNT_SETATTR": {"x86_64": 442, "aarch64": 442},
    }
    for table, numbers in expected.items():
        line = next(
            line for line in init_source.splitlines() if line.startswith(table)
        )
        found = {
            name: int(value)
            for name, value in re.findall(r'"([a-z0-9_]+)":\s*(\d+)', line)
        }
        assert found == numbers, table
    contract = (ROOT / "CONTRACT.md").read_text(encoding="utf-8")
    readme = (ROOT / "README.md").read_text(encoding="utf-8")
    for arch in ("x86_64", "aarch64"):
        assert arch in contract
        assert arch in readme


def test_the_contract_promises_no_more_than_the_provenance_check_holds():
    contract = (ROOT / "CONTRACT.md").read_text(encoding="utf-8")
    mf12 = contract[contract.index("Obligation MF-12 covers"):]
    assert "never swept however it is named" not in mf12
    assert "it is not authentication" in mf12
    assert "written again by the init" in mf12
    assert "lands whole or not at all" in mf12


def test_the_readme_cleanup_claim_carries_the_sweep_fallback():
    # The layered cleanup is the truth the contract states,
    # so the README sentence about a killed command must
    # carry the sweep fallback rather than promise the
    # directory is always already gone.
    readme = (ROOT / "README.md").read_text(encoding="utf-8")
    assert "leaves no task running and no sandbox directory behind" not in readme
    assert "the next run under the same root sweeps the directory away" in readme


def _drive_janitor(sandbox, marker, token):
    """Run one janitor the way the runner runs it, with a dead supervisor.

    The token arrives over standard input and the watch pipe is
    closed at once, which is the supervisor dying without a
    word. Returns the janitor exit code once it exits.
    """
    read_fd, write_fd = os.pipe()
    janitor_path = ROOT / "mayfly" / "_janitor.py"
    proc = subprocess.Popen(
        [
            PY,
            str(janitor_path),
            "--sandbox",
            str(sandbox),
            "--marker",
            str(marker),
            "--fd",
            str(read_fd),
        ],
        stdin=subprocess.PIPE,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        pass_fds=(read_fd,),
        start_new_session=True,
    )
    os.close(read_fd)
    proc.stdin.write(token.encode("utf-8") + b"\n")
    proc.stdin.close()
    os.close(write_fd)
    proc.wait(timeout=40)
    return proc.returncode


def test_the_janitor_marker_read_is_bounded_and_bad_bytes_read_as_no_token(tmp_path):
    token = "ab" * 16
    cases = (
        ("genuine", (token + "\n").encode("utf-8"), True),
        ("padded", (token + " " * 200 + "\n").encode("utf-8"), False),
        ("badbytes", token.encode("utf-8") + b"\xff\xfe\n", False),
    )
    for name, content, stands_down in cases:
        run_dir = tmp_path / name
        sandbox = run_dir / "sandbox"
        sandbox.mkdir(parents=True)
        (sandbox / "leftover.txt").write_text("left behind", encoding="utf-8")
        marker = run_dir / "clean-marker"
        marker.write_bytes(content)
        assert _drive_janitor(sandbox, marker, token) == 0, name
        if stands_down:
            assert sandbox.exists(), name
        else:
            assert not sandbox.exists(), name


def _plant_marker_bytes_and_sigkill(tmp_path, content):
    import signal as signal_module
    import uuid

    marker = "mayfly-death-" + uuid.uuid4().hex
    proc = _start_cli_with_sleeping_task(tmp_path, marker)
    sandboxes = list(tmp_path.glob("run-*"))
    assert len(sandboxes) == 1
    run_id = sandboxes[0].name[len("run-"):]
    planted = tmp_path / (".mayfly-clean-" + run_id)
    planted.write_bytes(content)
    proc.send_signal(signal_module.SIGKILL)
    proc.wait(timeout=30)
    assert _wait_until(lambda: not _host_pids_with_marker(marker), 30)
    assert _wait_until(lambda: list(tmp_path.glob("run-*")) == [], 30)
    assert list(tmp_path.glob(".mayfly-clean-*")) == []


@requires_sandbox
def test_a_planted_marker_with_bytes_that_are_not_text_does_not_stand_the_janitor_down(
    tmp_path,
):
    _plant_marker_bytes_and_sigkill(tmp_path, b"\xff\xfe" * 64)


@requires_sandbox
def test_a_planted_oversized_marker_does_not_stand_the_janitor_down(tmp_path):
    _plant_marker_bytes_and_sigkill(tmp_path, b"x" * (32 << 20))


@requires_sandbox
def test_the_sweep_skips_a_run_whose_liveness_is_unknown(tmp_path, monkeypatch):
    import signal as signal_module
    import uuid

    marker = "mayfly-death-" + uuid.uuid4().hex
    proc = _start_cli_with_sleeping_task(tmp_path, marker)
    sandboxes = list(tmp_path.glob("run-*"))
    assert len(sandboxes) == 1
    sandbox = sandboxes[0]
    _age_directory(sandbox)
    real_listdir = os.listdir

    def refusing_listdir(path):
        if os.fspath(path) == "/proc":
            raise OSError("the process table cannot be listed")
        return real_listdir(path)

    monkeypatch.setattr(os, "listdir", refusing_listdir)
    assert namespace_module._pids_referencing(str(sandbox)) is None
    namespace_module._sweep_orphans(tmp_path)
    assert sandbox.exists()
    monkeypatch.undo()
    proc.send_signal(signal_module.SIGTERM)
    out, _ = proc.communicate(timeout=30)
    assert proc.returncode == 125
    assert json.loads(out)["outcome"] == "harness_error"
    assert _wait_until(lambda: not _host_pids_with_marker(marker), 15)
    assert list(tmp_path.glob("run-*")) == []


@requires_sandbox
def test_the_sweep_leaves_a_foreign_run_directory_standing(tmp_path):
    root = tmp_path / "root"
    foreign = root / "run-foreign0001"
    foreign.mkdir(parents=True)
    (foreign / "not-mayfly.txt").write_text(
        "not a mayfly sandbox", encoding="utf-8"
    )
    _age_directory(foreign)
    report = run_task([PY, "-c", "pass"], root)
    assert report.outcome == "task"
    assert (foreign / "not-mayfly.txt").read_text(encoding="utf-8") == (
        "not a mayfly sandbox"
    )
    assert list(root.glob("run-*")) == [foreign]


@requires_sandbox
def test_the_sweep_leaves_a_directory_whose_provenance_names_another_run(tmp_path):
    root = tmp_path / "root"
    foreign = root / "run-aaaa00000001"
    foreign.mkdir(parents=True)
    (foreign / ".mayfly-provenance").write_text("bbbb00000002\n", encoding="utf-8")
    _age_directory(foreign)
    report = run_task([PY, "-c", "pass"], root)
    assert report.outcome == "task"
    assert foreign.exists()
    assert list(root.glob("run-*")) == [foreign]


@requires_sandbox
def test_a_provenance_with_bytes_that_are_not_text_leaves_the_directory_standing(
    tmp_path,
):
    root = tmp_path / "root"
    planted = root / "run-cccc00000003"
    planted.mkdir(parents=True)
    (planted / ".mayfly-provenance").write_bytes(b"\xff\xfe\x00binary")
    _age_directory(planted)
    report = run_task([PY, "-c", "pass"], root)
    assert report.outcome == "task"
    assert (planted / ".mayfly-provenance").read_bytes() == b"\xff\xfe\x00binary"
    assert list(root.glob("run-*")) == [planted]


def _run_task_on_a_thread(root, command):
    outcome = {}

    def drive():
        try:
            outcome["report"] = run_task(command, root)
        except BaseException as exc:
            outcome["error"] = exc

    thread = threading.Thread(target=drive, daemon=True)
    thread.start()
    return thread, outcome


@requires_sandbox
def test_a_fifo_at_the_provenance_path_neither_hangs_nor_breaks_the_run(tmp_path):
    root = tmp_path / "root"
    planted = root / "run-dddd00000004"
    planted.mkdir(parents=True)
    os.mkfifo(planted / ".mayfly-provenance")
    _age_directory(planted)
    thread, outcome = _run_task_on_a_thread(root, [PY, "-c", "pass"])
    thread.join(timeout=60)
    assert not thread.is_alive(), "the triggered run hung on the planted pipe"
    assert "error" not in outcome
    assert outcome["report"].outcome == "task"
    assert planted.exists()
    assert list(root.glob("run-*")) == [planted]


@requires_sandbox
def test_a_fifo_at_the_marker_path_does_not_keep_the_sandbox(tmp_path):
    import signal as signal_module
    import uuid

    marker = "mayfly-death-" + uuid.uuid4().hex
    proc = _start_cli_with_sleeping_task(tmp_path, marker)
    sandboxes = list(tmp_path.glob("run-*"))
    assert len(sandboxes) == 1
    run_id = sandboxes[0].name[len("run-"):]
    os.mkfifo(tmp_path / (".mayfly-clean-" + run_id))
    proc.send_signal(signal_module.SIGKILL)
    proc.wait(timeout=30)
    assert _wait_until(lambda: not _host_pids_with_marker(marker), 30)
    assert _wait_until(lambda: list(tmp_path.glob("run-*")) == [], 30)
    assert list(tmp_path.glob(".mayfly-clean-*")) == []


@requires_sandbox
def test_a_fifo_at_the_marker_path_does_not_block_a_clean_run(tmp_path):
    root = tmp_path / "root"
    thread, outcome = _run_task_on_a_thread(
        root, [PY, "-c", "import time; time.sleep(4)"]
    )
    assert _wait_until(lambda: len(list(root.glob("run-*"))) == 1, 15)
    sandbox = next(root.glob("run-*"))
    run_id = sandbox.name[len("run-"):]
    os.mkfifo(root / (".mayfly-clean-" + run_id))
    thread.join(timeout=60)
    assert not thread.is_alive(), "the clean run blocked on the planted pipe"
    assert "error" not in outcome
    report = outcome["report"]
    assert report.outcome == "task"
    assert report.sandbox_removed is True
    assert list(root.iterdir()) == []


def test_the_janitor_reads_a_fifo_marker_as_no_token(tmp_path):
    sandbox = tmp_path / "sandbox"
    sandbox.mkdir()
    (sandbox / "leftover.txt").write_text("left behind", encoding="utf-8")
    marker = tmp_path / "clean-marker"
    os.mkfifo(marker)
    assert _drive_janitor(sandbox, marker, "ab" * 16) == 0
    assert not sandbox.exists()


def test_the_janitor_removes_a_sandbox_that_is_already_gone_as_a_noop(tmp_path):
    sandbox = tmp_path / "sandbox-that-is-not-there"
    marker = tmp_path / "clean-marker-that-is-not-there"
    assert _drive_janitor(sandbox, marker, "ab" * 16) == 0
    assert not sandbox.exists()


def test_the_sweep_reads_a_table_it_cannot_read_at_all_as_unknown(
    tmp_path, monkeypatch
):
    root = tmp_path / "root"
    planted = root / "run-eeee00000005"
    planted.mkdir(parents=True)
    (planted / ".mayfly-provenance").write_text("eeee00000005\n", encoding="utf-8")
    _age_directory(planted)
    sleeper = subprocess.Popen(
        [PY, "-c", "import time; time.sleep(60)", str(planted)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        assert _wait_until(
            lambda: sleeper.pid in (namespace_module._pids_referencing(str(planted)) or []),
            15,
        )
        real_open = open

        def refusing_open(path, *args, **kwargs):
            text = os.fspath(path) if isinstance(path, (str, os.PathLike)) else ""
            if text.startswith("/proc") and text.endswith("cmdline"):
                raise OSError("the command line cannot be read")
            return real_open(path, *args, **kwargs)

        monkeypatch.setattr(namespace_module, "open", refusing_open, raising=False)
        assert namespace_module._pids_referencing(str(planted)) is None
        namespace_module._sweep_orphans(root)
        assert planted.exists()
    finally:
        sleeper.terminate()
        sleeper.wait(timeout=15)


def test_the_sweep_reads_a_refused_cmdline_as_unknown_liveness(
    tmp_path, monkeypatch
):
    root = tmp_path / "root"
    planted = root / "run-eeee00000006"
    planted.mkdir(parents=True)
    (planted / ".mayfly-provenance").write_text("eeee00000006\n", encoding="utf-8")
    _age_directory(planted)
    sleeper = subprocess.Popen(
        [PY, "-c", "import time; time.sleep(60)", str(planted)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        assert _wait_until(
            lambda: sleeper.pid in (namespace_module._pids_referencing(str(planted)) or []),
            15,
        )
        real_open = open

        def withholding_open(path, *args, **kwargs):
            text = os.fspath(path) if isinstance(path, (str, os.PathLike)) else ""
            if text == "/proc/" + str(sleeper.pid) + "/cmdline":
                raise PermissionError("the command line read was refused")
            return real_open(path, *args, **kwargs)

        monkeypatch.setattr(namespace_module, "open", withholding_open, raising=False)
        assert namespace_module._pids_referencing(str(planted)) is None
        namespace_module._sweep_orphans(root)
        assert planted.exists()
    finally:
        sleeper.terminate()
        sleeper.wait(timeout=15)


def test_a_vanished_process_does_not_make_liveness_unknown(tmp_path, monkeypatch):
    root = tmp_path / "root"
    planted = root / "run-eeee00000007"
    planted.mkdir(parents=True)
    (planted / ".mayfly-provenance").write_text("eeee00000007\n", encoding="utf-8")
    sleeper = subprocess.Popen(
        [PY, "-c", "import time; time.sleep(60)", str(planted)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        assert _wait_until(
            lambda: sleeper.pid in (namespace_module._pids_referencing(str(planted)) or []),
            15,
        )
        real_open = open

        def vanishing_open(path, *args, **kwargs):
            text = os.fspath(path) if isinstance(path, (str, os.PathLike)) else ""
            if (
                text.startswith("/proc/")
                and text.endswith("/cmdline")
                and str(sleeper.pid) not in text
            ):
                raise FileNotFoundError("the process vanished mid scan")
            return real_open(path, *args, **kwargs)

        monkeypatch.setattr(namespace_module, "open", vanishing_open, raising=False)
        found = namespace_module._pids_referencing(str(planted))
        assert found is not None
        assert sleeper.pid in found
    finally:
        sleeper.terminate()
        sleeper.wait(timeout=15)


FAKE_UNSHARE = """#!/usr/bin/env python3
import json
import os
import sys
import time

argv = sys.argv[1:]
fd = int(argv[argv.index("--status-fd") + 1])
rootfs = argv[argv.index("--rootfs") + 1]
sandbox = os.path.dirname(rootfs)
mode = os.environ.get("MAYFLY_FAKE_MODE", "late-done")


def emit(payload):
    payload.setdefault("ts", time.monotonic())
    os.write(fd, (json.dumps(payload) + "\\n").encode())


def note(name):
    with open(os.path.join(sandbox, name), "w") as handle:
        handle.write("noted\\n")


emit({"event": "ready", "proc_mounted": False})
emit({"event": "started"})
note("fake-started")
if mode == "late-done":
    pid = os.fork()
    if pid == 0:
        time.sleep(1.2)
        emit({"event": "task_done", "exit_code": 0})
        os._exit(0)
    time.sleep(60)
else:
    emit({"event": "task_done", "exit_code": 0})
    note("fake-done")
    time.sleep(60)
"""


def _fake_unshare_text():
    """The fake unshare script with this interpreter in its shebang.

    The backend scrubs PATH out of the environment the fake
    runs with, so a shebang that searches for the interpreter
    by name depends on the host keeping one on the default
    search path. The sibling helpers run their scripts through
    this interpreter directly, and the fake names it outright
    for the same reason.
    """
    return FAKE_UNSHARE.replace("#!/usr/bin/env python3", "#!" + PY, 1)


def _write_fake_unshare(fakebin):
    fakebin.mkdir(parents=True, exist_ok=True)
    fake = fakebin / "unshare"
    fake.write_text(_fake_unshare_text(), encoding="utf-8")
    fake.chmod(0o755)
    return fake


def _start_cli_with_fake_unshare(root, fakebin, mode):
    _write_fake_unshare(fakebin)
    env = dict(os.environ)
    env["PYTHONPATH"] = str(ROOT)
    env["PATH"] = str(fakebin) + os.pathsep + env.get("PATH", "")
    env["MAYFLY_FAKE_MODE"] = mode
    return subprocess.Popen(
        [
            PY,
            "-m",
            "mayfly",
            "run",
            "--root",
            str(root),
            "--allow-env",
            "MAYFLY_FAKE_MODE",
            "--",
            PY,
            "-c",
            "pass",
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        cwd=ROOT,
        env=env,
    )


def test_the_fake_unshare_names_this_interpreter_in_its_shebang(tmp_path):
    fake = _write_fake_unshare(tmp_path / "fakebin")
    first_line = fake.read_text(encoding="utf-8").splitlines()[0]
    assert first_line == "#!" + PY


def test_an_interrupt_before_the_task_done_stamp_is_a_harness_error(tmp_path):
    import signal as signal_module

    root = tmp_path / "root"
    root.mkdir()
    proc = _start_cli_with_fake_unshare(root, tmp_path / "fakebin", "late-done")
    noted = _wait_until(
        lambda: any((d / "fake-started").exists() for d in root.glob("run-*")),
        30,
    )
    assert noted, "the fake sandbox never reported its start"
    proc.send_signal(signal_module.SIGTERM)
    out, _ = proc.communicate(timeout=30)
    assert proc.returncode == 125
    payload = json.loads(out)
    assert payload["outcome"] == "harness_error"
    assert payload["task_exit_code"] is None
    assert "interrupted by signal 15" in payload["error"]
    assert list(root.glob("run-*")) == []
    assert list(root.glob(".mayfly-clean-*")) == []


def test_an_interrupt_after_the_task_done_stamp_keeps_the_task_outcome(tmp_path):
    import signal as signal_module

    root = tmp_path / "root"
    root.mkdir()
    proc = _start_cli_with_fake_unshare(root, tmp_path / "fakebin", "done-early")
    noted = _wait_until(
        lambda: any((d / "fake-done").exists() for d in root.glob("run-*")),
        30,
    )
    assert noted, "the fake sandbox never reported its task done"
    proc.send_signal(signal_module.SIGTERM)
    out, _ = proc.communicate(timeout=30)
    assert proc.returncode == 0
    payload = json.loads(out)
    assert payload["outcome"] == "task"
    assert payload["task_exit_code"] == 0
    assert list(root.glob("run-*")) == []
    assert list(root.glob(".mayfly-clean-*")) == []


def test_a_command_with_a_none_element_is_a_harness_error(tmp_path):
    report = run_task([PY, None], tmp_path)
    assert report.outcome == "harness_error"
    assert report.task_exit_code is None
    assert report.task_signal is None
    assert report.error
    assert "could not launch unshare" in report.error
    assert report.sandbox_removed is True
    assert list(tmp_path.glob("run-*")) == []


def test_cli_reports_a_none_command_element_as_a_harness_error(tmp_path, capsys):
    # The command list is handed to main the way a caller of
    # the library builds it, because shell arguments cannot
    # carry a None element through a process boundary.
    from mayfly.cli import main as cli_main

    code = cli_main(["run", "--root", str(tmp_path), "--", PY, None])
    payload = json.loads(capsys.readouterr().out)
    assert code == 125
    assert payload["outcome"] == "harness_error"
    assert payload["task_exit_code"] is None
    assert payload["task_signal"] is None


@requires_sandbox
def test_the_init_leaves_provenance_beside_an_assembled_root(tmp_path):
    # Drive the real assemble_root inside a real namespace with
    # no runner involved, so no runner write can account for
    # the provenance. The residue a killed prepare leaves is
    # whatever this assembly created, and it must carry the
    # provenance the sweep trusts or no sweep will collect it.
    sandbox = tmp_path / "run-seam00000001"
    rootfs = sandbox / "rootfs"
    driver = (
        "import importlib.util, sys\n"
        "spec = importlib.util.spec_from_file_location('mayfly_init', sys.argv[2])\n"
        "mod = importlib.util.module_from_spec(spec)\n"
        "spec.loader.exec_module(mod)\n"
        "mod.assemble_root(sys.argv[1])\n"
    )
    result = subprocess.run(
        [
            shutil.which("unshare"),
            "--user",
            "--map-root-user",
            "--mount",
            "--pid",
            "--uts",
            "--ipc",
            "--fork",
            PY,
            "-c",
            driver,
            str(rootfs),
            str(ROOT / "mayfly" / "_init.py"),
        ],
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert result.returncode == 0, result.stderr
    provenance = sandbox / ".mayfly-provenance"
    assert provenance.is_file()
    assert provenance.read_text(encoding="utf-8").strip() == "seam00000001"
    assert (rootfs / "usr").is_dir()


class _DyingWriteHandle:
    """A file handle whose write dies, as a death lands mid write.

    The real handle underneath was opened the way the code
    under test opened it, so any truncate on open has already
    happened by the time the write raises, which is exactly
    the window the atomic provenance write exists to close.
    """

    def __init__(self, handle):
        self._handle = handle

    def write(self, data):
        raise OSError("a death inside the write")

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc, tb):
        self._handle.close()
        return False


def _open_with_dying_write(path, mode="r", *args, **kwargs):
    handle = open(path, mode, *args, **kwargs)
    if "w" in mode:
        return _DyingWriteHandle(handle)
    return handle


def test_the_init_provenance_write_leaves_the_born_copy_standing(tmp_path, monkeypatch):
    # The init rewrites the provenance the runner wrote at
    # creation. A death inside that write must leave the born
    # copy standing whole, because a truncated provenance
    # names nothing and no sweep would ever collect the
    # directory it stands in.
    import importlib.util

    spec = importlib.util.spec_from_file_location(
        "mayfly_init_atomic", ROOT / "mayfly" / "_init.py"
    )
    init_module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(init_module)
    sandbox = tmp_path / "run-born00000001"
    (sandbox / "rootfs").mkdir(parents=True)
    provenance = sandbox / ".mayfly-provenance"
    provenance.write_text("born00000001\n", encoding="utf-8")
    monkeypatch.setattr(
        init_module, "open", _open_with_dying_write, raising=False
    )
    init_module._write_provenance(str(sandbox / "rootfs"))
    assert provenance.read_text(encoding="utf-8") == "born00000001\n"
    assert sorted(p.name for p in sandbox.iterdir()) == [
        ".mayfly-provenance",
        "rootfs",
    ]


def test_the_runner_provenance_write_leaves_no_partial_file_behind(
    tmp_path, monkeypatch
):
    # The runner writes the provenance when it creates the
    # sandbox directory. A death inside that write must leave
    # the provenance path absent at creation, and a death
    # inside a rewrite must leave the complete file standing,
    # never a truncated one.
    sandbox = tmp_path / "run-born00000002"
    sandbox.mkdir()
    provenance = sandbox / ".mayfly-provenance"
    monkeypatch.setattr(
        namespace_module, "open", _open_with_dying_write, raising=False
    )
    with pytest.raises(OSError):
        namespace_module._write_provenance(sandbox, "born00000002")
    assert not provenance.exists()
    assert list(sandbox.iterdir()) == []
    monkeypatch.setattr(namespace_module, "open", open, raising=False)
    namespace_module._write_provenance(sandbox, "born00000002")
    assert provenance.read_text(encoding="utf-8") == "born00000002\n"
    monkeypatch.setattr(
        namespace_module, "open", _open_with_dying_write, raising=False
    )
    with pytest.raises(OSError):
        namespace_module._write_provenance(sandbox, "born00000003")
    assert provenance.read_text(encoding="utf-8") == "born00000002\n"
    assert sorted(p.name for p in sandbox.iterdir()) == [".mayfly-provenance"]


@requires_sandbox
def test_a_run_killed_inside_prepare_leaves_nothing_unsweepable(tmp_path):
    import signal as signal_module

    root = tmp_path / "root"
    root.mkdir()
    marker = "prepare-window-" + str(os.getpid())
    env = dict(os.environ)
    env["PYTHONPATH"] = str(ROOT)
    for _trial in range(8):
        proc = subprocess.Popen(
            [
                PY,
                "-m",
                "mayfly",
                "run",
                "--root",
                str(root),
                "--",
                PY,
                "-c",
                "import time; time.sleep(30)  # " + marker,
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            cwd=ROOT,
            env=env,
        )
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if proc.poll() is not None:
                break
            under_way = any(
                (candidate / "rootfs" / "usr").exists()
                for candidate in root.glob("run-*")
            )
            if under_way:
                break
            time.sleep(0.002)
        if proc.poll() is None:
            proc.send_signal(signal_module.SIGKILL)
        proc.wait(timeout=30)

    def quiet():
        if _host_pids_with_marker(marker):
            return False
        for leftover in root.glob("run-*"):
            if namespace_module._pids_referencing(str(leftover)):
                return False
        return True

    deadline = time.monotonic() + 60
    while time.monotonic() < deadline:
        if quiet():
            time.sleep(0.5)
            if quiet():
                break
    else:
        raise AssertionError("the killed runs never went quiet")

    leftovers = sorted(root.glob("run-*"))
    for leftover in leftovers:
        run_id = leftover.name[len("run-"):]
        provenance = leftover / ".mayfly-provenance"
        assert provenance.is_file(), leftover
        assert provenance.read_text(encoding="utf-8").strip() == run_id
        _age_directory(leftover)
    for clean_marker in root.glob(".mayfly-clean-*"):
        _age_directory(clean_marker)
    for _attempt in range(3):
        if not list(root.glob("run-*")) and not list(root.glob(".mayfly-clean-*")):
            break
        report = run_task([PY, "-c", "pass"], root)
        assert report.outcome == "task"
    assert list(root.glob("run-*")) == []
    assert list(root.glob(".mayfly-clean-*")) == []
