import json
import os
import select
import shutil
import socket
import subprocess
import sys
import threading
import time
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from mayfly import HarnessError, run_task
from mayfly.backends.base import RunSpec
from mayfly.backends.namespace import NamespaceBackend

pytestmark = pytest.mark.skipif(
    shutil.which("unshare") is None, reason="unshare is not available on this machine"
)

PY = sys.executable


def last_json(report):
    lines = [line for line in report.stdout_tail.strip().splitlines() if line.strip()]
    return json.loads(lines[-1])


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


def test_task_exit_code_is_reported_exactly(tmp_path):
    report = run_task([PY, "-c", "import sys; sys.exit(7)"], tmp_path)
    assert report.outcome == "task"
    assert report.task_exit_code == 7
    assert report.task_signal is None


def test_task_killed_by_a_signal_is_reported_as_a_signal(tmp_path):
    report = run_task(
        [PY, "-c", "import os, signal; os.kill(os.getpid(), signal.SIGTERM)"], tmp_path
    )
    assert report.outcome == "task"
    assert report.task_exit_code is None
    assert report.task_signal == 15


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


def test_the_default_profile_has_no_network(tmp_path):
    with LoopbackListener() as listener:
        report = run_task([PY, "-c", CONNECT_SCRIPT, str(listener.port)], tmp_path)
        seen = last_json(report)
        assert seen["connected"] is False
        assert seen["errno"] == 101
        time.sleep(0.3)
        assert listener.received == []


def test_the_default_profile_lists_only_loopback(tmp_path):
    script = "import json, socket; print(json.dumps([name for _, name in socket.if_nameindex()]))"
    report = run_task([PY, "-c", script], tmp_path)
    assert last_json(report) == ["lo"]


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


def test_the_stderr_tail_carries_a_failing_task_stderr(tmp_path):
    script = "import sys; sys.stderr.write('a named failure line' + chr(10)); sys.exit(3)"
    report = run_task([PY, "-c", script], tmp_path)
    assert report.outcome == "task"
    assert report.task_exit_code == 3
    assert "a named failure line" in report.stderr_tail


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


def test_a_submount_of_a_bound_tree_is_read_only_inside(tmp_path):
    import uuid

    probe_dir = Path("/etc") / ("mayfly-subprobe-" + uuid.uuid4().hex[:12])
    probe_dir.mkdir()
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
