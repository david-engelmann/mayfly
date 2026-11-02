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
        assert report.timings_ms[phase] >= 0, phase
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
