//! The behaviour pins, translated pin for pin from the Python
//! suite that closed Round 1. Each test names the same fact its
//! Python original named. Where the Rust harness cannot carry a
//! Python mechanism, the pin keeps its substance and changes its
//! injection, and each such change is stated where it lands.
//! The caller environment monkeypatching of the Python suite is
//! the caller_env field of the spec here. The uuid fixing of
//! the Python suite is the run_id_override field of the backend.
//! The patched process table of the Python suite is a synthetic
//! process table directory read through the same library entry
//! points the sweep uses. The none element a Python command
//! list can carry is a NUL byte inside an argument here, which
//! no exec can carry either, and it lands on the same refusal.
//! The outcome and the timing phases are typed values in the
//! crate rather than the strings the report prints, so the
//! pins name them as Outcome and Phase values where the Python
//! suite compared the printed strings, a mechanical
//! consequence of the typing change in the crate. The printed
//! report is unchanged, and the command line pins go on
//! comparing the printed strings in the json they parse.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::io::RawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Once};
use std::time::{Duration, Instant};

use mayfly::cli::cli_main;
use mayfly::{
    pids_referencing, pids_referencing_in, sweep_orphans_in, NamespaceBackend, Outcome, Phase,
    RunReport, RunSpec, HARNESS_EXIT_CODE, TAIL_CHARS,
};

mod common;

use common::{mkfifo, parse_env_output, root, Tmp};
use common::{PHASE_NAMES, REPORT_FIELDS};

fn mayfly_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mayfly"))
}

static HELPER_ONCE: Once = Once::new();

fn ensure_helper() {
    HELPER_ONCE.call_once(|| {
        mayfly::set_helper_exe(&mayfly_bin());
    });
}

static PY: LazyLock<PathBuf> = LazyLock::new(|| {
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join("python3");
        if candidate.is_file() {
            // Resolve through version manager wrapper scripts such as pyenv
            // shims to the real interpreter binary, because a kernel shebang
            // cannot name another script as its interpreter.
            if let Ok(output) = Command::new(&candidate)
                .args(["-c", "import sys; print(sys.executable)"])
                .output()
            {
                if output.status.success() {
                    let text = String::from_utf8_lossy(&output.stdout);
                    let resolved = PathBuf::from(text.trim());
                    if resolved.is_file() {
                        return resolved;
                    }
                }
            }
            return candidate;
        }
    }
    PathBuf::from("python3")
});

fn py() -> OsString {
    PY.clone().into_os_string()
}

fn os(text: &str) -> OsString {
    OsString::from(text)
}

fn cmd(parts: &[&str]) -> Vec<OsString> {
    parts.iter().map(|part| os(part)).collect()
}

fn py_cmd(code: &str) -> Vec<OsString> {
    vec![py(), os("-c"), os(code)]
}

fn spec(command: Vec<OsString>, root: &Path) -> RunSpec {
    RunSpec::new(command, root.to_path_buf())
}

fn run_task(command: Vec<OsString>, root: &Path) -> RunReport {
    ensure_helper();
    NamespaceBackend::new().run(&spec(command, root))
}

fn run_task_spec(spec: &RunSpec) -> RunReport {
    ensure_helper();
    NamespaceBackend::new().run(spec)
}

fn last_json(report: &RunReport) -> serde_json::Value {
    let line = report
        .stdout_tail
        .lines()
        .filter(|line| !line.trim().is_empty())
        .next_back()
        .expect("the task printed no json line");
    serde_json::from_str(line).expect("the last stdout line is not json")
}

fn wait_until<F: FnMut() -> bool>(mut predicate: F, timeout_secs: f64) -> bool {
    let deadline = Instant::now() + Duration::from_secs_f64(timeout_secs);
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    predicate()
}

fn age_directory(path: &Path, seconds: f64) {
    let old = std::time::SystemTime::now() - Duration::from_secs_f64(seconds);
    let since_epoch = old
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    let times = [
        libc::timespec {
            tv_sec: since_epoch.as_secs() as libc::time_t,
            tv_nsec: 0,
        },
        libc::timespec {
            tv_sec: since_epoch.as_secs() as libc::time_t,
            tv_nsec: 0,
        },
    ];
    let cpath = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    unsafe {
        libc::utimensat(libc::AT_FDCWD, cpath.as_ptr(), times.as_ptr(), 0);
    }
}

fn host_pids_with_marker(marker: &str) -> Vec<u32> {
    let needle = marker.as_bytes();
    let mut pids = Vec::new();
    let entries = match std::fs::read_dir("/proc") {
        Ok(entries) => entries,
        Err(_) => return pids,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) {
            if mayfly::contains_subslice(&cmdline, needle) {
                if let Ok(pid) = name.parse() {
                    pids.push(pid);
                }
            }
        }
    }
    pids
}

/// Pids running a mayfly helper as their own program. The
/// helper subcommand must be the second word of the command
/// line, the program the process was started with, because the
/// unshare process also names the init further along its own
/// command line without being the init. This stands in for the
/// Python helper's script path check, the subcommand word of
/// the re-invoked binary taking the script path's place.
fn pids_running_helper(subcommand: &str, extra_needle: Option<&[u8]>) -> Vec<u32> {
    let mut pids = Vec::new();
    let entries = match std::fs::read_dir("/proc") {
        Ok(entries) => entries,
        Err(_) => return pids,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let cmdline = match std::fs::read(entry.path().join("cmdline")) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        let parts: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
        if parts.len() < 2 || parts[1] != subcommand.as_bytes() {
            continue;
        }
        if let Some(needle) = extra_needle {
            if !mayfly::contains_subslice(&cmdline, needle) {
                continue;
            }
        }
        if let Ok(pid) = name.parse() {
            pids.push(pid);
        }
    }
    pids
}

fn snapshot_tree(base: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![base.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.is_file() {
                    let rel = path
                        .strip_prefix(base)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned();
                    out.insert(rel, std::fs::read(&path).unwrap_or_default());
                }
            }
        }
    }
    out
}

fn dir_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    names
}

fn glob_prefixed(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().starts_with(prefix))
                    .unwrap_or(false)
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    out.sort();
    out
}

fn glob_run_dirs(dir: &Path) -> Vec<PathBuf> {
    glob_prefixed(dir, "run-")
}

fn glob_clean_markers(dir: &Path) -> Vec<PathBuf> {
    glob_prefixed(dir, ".mayfly-clean-")
}

fn unique_hex() -> String {
    let mut bytes = [0u8; 16];
    let _ = getrandom::fill(&mut bytes);
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

// ---------- the sandbox probe ----------

const FORCE_REFUSAL_SWITCH: &str = "MAYFLY_TEST_FORCE_SANDBOX_REFUSAL";

/// The probe's one reading of a failed run's text. The
/// report's error and its stderr tail, in that order, with
/// empty parts dropped, joined by a single space. The
/// classification reads the text lowercased and the refusal
/// the probe hands back reads it trimmed, and both read the
/// same text because both answer from the same failure.
fn report_text(report: &RunReport) -> String {
    [
        report.error.clone().unwrap_or_default(),
        report.stderr_tail.clone(),
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join(" ")
}

fn probe_failure_is_host_refusal(report: &RunReport) -> bool {
    let text = report_text(report).to_lowercase();
    if text.contains("uid_map") {
        return true;
    }
    if text.contains("unshare")
        && (text.contains("not permitted") || text.contains("permission denied"))
    {
        return true;
    }
    false
}

fn synthetic_probe_report(error: Option<&str>, stderr_tail: &str) -> RunReport {
    let mut report = RunReport::new(
        "namespace",
        "probe0000000",
        "/nonexistent",
        py_cmd("pass"),
        Outcome::HarnessError,
    );
    report.error = error.map(|text| text.to_string());
    report.stderr_tail = stderr_tail.to_string();
    report
}

fn probe_core(runner: &dyn Fn(Vec<OsString>, &Path) -> RunReport) -> (bool, String) {
    let probe_root = std::env::temp_dir().join(format!("mayfly-probe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&probe_root);
    std::fs::create_dir_all(&probe_root).unwrap();
    let report = runner(py_cmd("pass"), &probe_root);
    let _ = std::fs::remove_dir_all(&probe_root);
    if report.outcome == Outcome::Task {
        return (true, String::new());
    }
    let refusal = report_text(&report).trim().to_string();
    if probe_failure_is_host_refusal(&report) {
        (
            false,
            if refusal.is_empty() {
                "the sandbox did not start and gave no reason".to_string()
            } else {
                refusal
            },
        )
    } else {
        panic!(
            "the sandbox probe failed for a reason that is not a host refusal, \
             so the guarded tests would be skipped over a broken sandbox, {}",
            if refusal.is_empty() {
                "the failed run gave no reason".to_string()
            } else {
                refusal
            }
        );
    }
}

/// Ask whether this host can start a real sandbox at all.
///
/// One trivial task runs through the real backend. Where the
/// run ends in a task outcome the host can start a sandbox and
/// the answer is (true, an empty refusal). Where it does not,
/// the failure is classified. A failure carrying the host
/// refusal signature answers (false, the refusal the failed
/// run reported), so a skip reason names the refusal the host
/// actually gave, the uid map refusal on a host that forbids
/// the user namespace mapping above all. Any other failure
/// panics, because a probe that took it for a refusal would
/// skip the guarded suite green over a broken sandbox, in
/// this environment and in continuous integration alike. A
/// suite that cannot tell a host that forbids the sandbox
/// from a sandbox that is broken fails wholesale on the first
/// kind of host and says nothing true about either. The
/// MAYFLY_TEST_FORCE_SANDBOX_REFUSAL switch is a test only
/// hook that forces the refusal answer, so the guards below
/// can be shown to fire.
fn probe_sandbox_with(runner: &dyn Fn(Vec<OsString>, &Path) -> RunReport) -> (bool, String) {
    if std::env::var_os(FORCE_REFUSAL_SWITCH).is_some() {
        return (
            false,
            format!(
                "the probe was forced to report a refusal by {}",
                FORCE_REFUSAL_SWITCH
            ),
        );
    }
    probe_core(runner)
}

#[cfg(target_os = "linux")]
fn real_runner(command: Vec<OsString>, root: &Path) -> RunReport {
    ensure_helper();
    NamespaceBackend::new().run(&spec(command, root))
}

static SANDBOX_PROBE: LazyLock<(bool, String)> = LazyLock::new(|| {
    // Portability adaptation because Linux namespaces exist only on
    // Linux, so non-Linux hosts report the host refusal directly
    // rather than invoking the Linux namespace runner.
    #[cfg(not(target_os = "linux"))]
    {
        (
            false,
            "Linux namespaces are not available on this host".to_string(),
        )
    }
    #[cfg(target_os = "linux")]
    {
        probe_sandbox_with(&real_runner)
    }
});

/// The requires-sandbox gate of the Python suite. A gated test
/// returns early where the host refuses the sandbox, which is
/// the Rust shape of a skip, and the probe above panics at
/// first use where the sandbox is broken rather than refused,
/// so the suite can never pass green over a broken sandbox.
fn require_sandbox() -> bool {
    let (ok, refusal) = &*SANDBOX_PROBE;
    if !ok {
        eprintln!("sandbox refused on this host, {}", refusal);
    }
    *ok
}

#[test]
fn the_probe_takes_the_uid_map_refusal_for_a_host_refusal() {
    let report = synthetic_probe_report(
        Some("the sandbox exited without starting the task, unshare exit 1"),
        "unshare: write failed /proc/self/uid_map: Operation not permitted",
    );
    assert!(probe_failure_is_host_refusal(&report));
    let report = synthetic_probe_report(
        Some("could not launch unshare, [Errno 13] Permission denied"),
        "",
    );
    assert!(probe_failure_is_host_refusal(&report));
}

#[test]
fn the_probe_takes_any_other_failure_for_a_broken_sandbox() {
    let broken_shapes = [
        synthetic_probe_report(
            Some(
                "the prepared root failed, \
                 [Errno 2] No such file or directory: '/probe/rootfs'",
            ),
            "",
        ),
        synthetic_probe_report(
            Some("could not launch unshare, [Errno 2] No such file or directory"),
            "",
        ),
        synthetic_probe_report(
            Some("the sandbox exited without starting the task, unshare exit 1"),
            "the init died on a name the driver never sent",
        ),
        synthetic_probe_report(None, ""),
    ];
    for report in broken_shapes {
        assert!(
            !probe_failure_is_host_refusal(&report),
            "{:?}",
            report.error
        );
    }
}

static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn the_probe_skips_on_a_refusal_and_fails_loudly_on_a_break() {
    let _guard = ENV_LOCK.lock().unwrap();
    let saved = std::env::var_os(FORCE_REFUSAL_SWITCH);
    std::env::remove_var(FORCE_REFUSAL_SWITCH);
    let refusal = synthetic_probe_report(
        Some("the sandbox exited without starting the task, unshare exit 1"),
        "unshare: write failed /proc/self/uid_map: Operation not permitted",
    );
    let refusal_runner = |_: Vec<OsString>, _: &Path| refusal.clone();
    let (ok, reason) = probe_sandbox_with(&refusal_runner);
    assert!(!ok);
    assert!(reason.contains("uid_map"));
    let broken = synthetic_probe_report(
        Some("the prepared root failed, [Errno 2] No such file or directory"),
        "",
    );
    let broken_runner = |_: Vec<OsString>, _: &Path| broken.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        probe_sandbox_with(&broken_runner)
    }));
    assert!(result.is_err());
    match saved {
        Some(value) => std::env::set_var(FORCE_REFUSAL_SWITCH, value),
        None => std::env::remove_var(FORCE_REFUSAL_SWITCH),
    }
}

// ---------- run behaviour ----------

#[test]
fn run_report_carries_the_phases_and_the_outcome() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("phases");
    let report = run_task(
        py_cmd("import time; time.sleep(0.3); print('hello')"),
        tmp.path(),
    );
    assert_eq!(report.backend, "namespace");
    assert_eq!(report.outcome, Outcome::Task);
    assert_eq!(report.task_exit_code, Some(0));
    assert_eq!(report.task_signal, None);
    assert!(report.stdout_tail.contains("hello"));
    for phase in Phase::ALL {
        let value = report.timings_ms.get(phase);
        assert!(value.is_some(), "{}", phase.name());
        assert!(value.unwrap() > 0.0, "{}", phase.name());
    }
    assert!(report.timings_ms.task.unwrap() >= 200.0);
    assert!(report.proc_mounted.is_some());
}

#[test]
fn a_task_reads_and_writes_the_bound_device_files() {
    if !require_sandbox() {
        return;
    }
    // MF-5 binds the basic device files into the prepared
    // root as single files, and no standing pin opens one
    // from inside a task, so a bind loop that dropped a
    // name would pass the whole suite. This pin drives a
    // task that reads sixteen zero bytes from /dev/zero,
    // writes to /dev/null, and draws sixteen bytes from
    // /dev/urandom, and holds the run's own word that all
    // three stood. A dropped bind leaves the name out of
    // the root's dev, so the reads fail inside the task on
    // the missing name. The write leg reads /dev/null back
    // after writing, because the task runs as the owner of
    // the root and could create a missing name as a plain
    // file and write into it. The bound device reads back
    // empty, and a file in its place reads back the bytes.
    let tmp = Tmp::new("device-files");
    let report = run_task(
        py_cmd(
            "z = open('/dev/zero', 'rb').read(16); assert z == bytes(16); \
             w = open('/dev/null', 'wb'); assert w.write(b'x' * 64) == 64; w.close(); \
             v = open('/dev/null', 'rb').read(16); assert v == b''; \
             u = open('/dev/urandom', 'rb').read(16); assert len(u) == 16; \
             print('devices-ok')",
        ),
        tmp.path(),
    );
    assert_eq!(report.outcome, Outcome::Task);
    assert_eq!(report.task_exit_code, Some(0));
    assert!(report.stdout_tail.contains("devices-ok"));
}

#[test]
fn task_exit_code_is_reported_exactly() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("exit-code");
    let report = run_task(py_cmd("import sys; sys.exit(7)"), tmp.path());
    assert_eq!(report.outcome, Outcome::Task);
    assert_eq!(report.task_exit_code, Some(7));
    assert_eq!(report.task_signal, None);
}

#[test]
fn task_killed_by_a_signal_is_reported_as_a_signal() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("signal");
    let report = run_task(
        py_cmd("import os, signal; os.kill(os.getpid(), signal.SIGTERM)"),
        tmp.path(),
    );
    assert_eq!(report.outcome, Outcome::Task);
    assert_eq!(report.task_exit_code, None);
    assert_eq!(report.task_signal, Some(15));
}

#[test]
fn a_task_exiting_at_125_is_a_task_outcome() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("exit-125");
    let report = run_task(py_cmd("import sys; sys.exit(125)"), tmp.path());
    assert_eq!(report.outcome, Outcome::Task);
    assert_eq!(report.task_exit_code, Some(125));
}

#[test]
fn a_command_that_cannot_start_is_a_harness_error() {
    let tmp = Tmp::new("cannot-start");
    let report = run_task(cmd(&["mayfly-no-such-binary-anywhere"]), tmp.path());
    assert_eq!(report.outcome, Outcome::HarnessError);
    assert_eq!(report.task_exit_code, None);
    assert_eq!(report.task_signal, None);
    assert!(report.error.is_some());
}

#[test]
fn a_missing_unshare_is_a_harness_error() {
    let tmp = Tmp::new("missing-unshare");
    ensure_helper();
    let backend = NamespaceBackend::with_unshare_path(Some(PathBuf::from("/nonexistent/unshare")));
    let report = backend.run(&spec(py_cmd("pass"), tmp.path()));
    assert_eq!(report.outcome, Outcome::HarnessError);
    assert_eq!(report.task_exit_code, None);
    assert_eq!(report.sandbox_removed, Some(true));
    assert!(dir_names(tmp.path()).is_empty());
}

#[test]
fn an_unknown_backend_fails_before_anything_is_prepared() {
    let tmp = Tmp::new("unknown-backend");
    ensure_helper();
    let result = mayfly::run_task(py_cmd("pass"), tmp.path(), "no-such-backend");
    assert!(result.is_err());
    assert!(dir_names(tmp.path()).is_empty());
}

#[test]
fn the_task_starts_in_the_work_directory_of_a_prepared_root() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("work-dir");
    let script = "import json, os, socket\n\
        print(json.dumps({\n\
        \x20 'cwd': os.getcwd(),\n\
        \x20 'pid': os.getpid(),\n\
        \x20 'ppid': os.getppid(),\n\
        \x20 'hostname': socket.gethostname(),\n\
        \x20 'work_dev': os.stat('.').st_dev,\n\
        \x20 'root_dev': os.stat('/').st_dev,\n\
        }))\n";
    let report = run_task(py_cmd(script), tmp.path());
    let seen = last_json(&report);
    assert_eq!(seen["cwd"], "/work");
    assert_eq!(seen["ppid"], 1);
    let pid = seen["pid"].as_i64().unwrap();
    assert!(pid > 1 && pid < 100, "{}", pid);
    assert_eq!(seen["hostname"], "mayfly");
    assert_ne!(seen["work_dev"], seen["root_dev"]);
}

#[test]
fn the_system_directories_are_read_only_inside() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("read-only");
    let script = "import json, os\n\
        path = '/usr/.mayfly-write-probe'\n\
        errno = 0\n\
        try:\n\
        \x20   handle = open(path, 'w')\n\
        \x20   handle.close()\n\
        \x20   os.unlink(path)\n\
        except OSError as exc:\n\
        \x20   errno = exc.errno\n\
        print(json.dumps({'usr_write_errno': errno}))\n";
    let report = run_task(py_cmd(script), tmp.path());
    let seen = last_json(&report);
    assert_eq!(seen["usr_write_errno"], 30);
}

#[test]
fn the_environment_is_scrubbed_to_the_allowlist() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("env-scrub");
    let mut task_spec = spec(cmd(&["/usr/bin/env"]), tmp.path());
    task_spec.allow_env = vec![os("PATH"), os("MAYFLY_KEEP_ME")];
    task_spec.caller_env = vec![
        (os("PATH"), os("/usr/bin:/bin")),
        (os("MAYFLY_LEAK_PROBE"), os("a secret value")),
        (os("MAYFLY_KEEP_ME"), os("a kept value")),
    ];
    let report = run_task_spec(&task_spec);
    let seen = parse_env_output(&report.stdout_tail);
    let expected: BTreeMap<String, String> = [
        ("PATH".to_string(), "/usr/bin:/bin".to_string()),
        ("MAYFLY_KEEP_ME".to_string(), "a kept value".to_string()),
        ("MAYFLY_SANDBOX".to_string(), "namespace".to_string()),
        ("PYTHONCOERCECLOCALE".to_string(), "0".to_string()),
    ]
    .into_iter()
    .collect();
    assert_eq!(seen, expected);
}

#[test]
fn a_doubled_allowlisted_key_carries_its_first_standing_value() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("env-first-standing");
    let mut task_spec = spec(cmd(&["/usr/bin/env"]), tmp.path());
    task_spec.allow_env = vec![os("PATH"), os("MAYFLY_KEEP_ME")];
    task_spec.caller_env = vec![
        (os("PATH"), os("/usr/bin:/bin")),
        (os("MAYFLY_KEEP_ME"), os("the first kept value")),
        (os("PATH"), os("/second/path")),
        (os("MAYFLY_KEEP_ME"), os("the second kept value")),
    ];
    let report = run_task_spec(&task_spec);
    let seen = parse_env_output(&report.stdout_tail);
    let expected: BTreeMap<String, String> = [
        ("PATH".to_string(), "/usr/bin:/bin".to_string()),
        (
            "MAYFLY_KEEP_ME".to_string(),
            "the first kept value".to_string(),
        ),
        ("MAYFLY_SANDBOX".to_string(), "namespace".to_string()),
        ("PYTHONCOERCECLOCALE".to_string(), "0".to_string()),
    ]
    .into_iter()
    .collect();
    assert_eq!(seen, expected);
}

#[test]
fn a_file_written_inside_is_gone_with_the_sandbox() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("file-gone");
    let script = "import json, os\n\
        with open('proof.txt', 'w') as handle:\n\
        \x20   handle.write('written inside the sandbox')\n\
        print(json.dumps({'wrote': os.path.exists('proof.txt')}))\n";
    let report = run_task(py_cmd(script), tmp.path());
    assert_eq!(last_json(&report)["wrote"], true);
    assert_eq!(report.sandbox_removed, Some(true));
    assert!(!Path::new(&report.sandbox_dir).exists());
    assert!(snapshot_tree(tmp.path())
        .keys()
        .all(|name| !name.ends_with("proof.txt")));
}

// Portability adaptation because pipe2 is Linux-only, while std::io::pipe
// creates close-on-exec descriptors portably on other Unix hosts.
fn pipe_cloexec() -> (RawFd, RawFd) {
    #[cfg(target_os = "linux")]
    {
        let mut fds: [RawFd; 2] = [0, 0];
        let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
        assert_eq!(rc, 0);
        (fds[0], fds[1])
    }
    #[cfg(not(target_os = "linux"))]
    {
        use std::os::unix::io::IntoRawFd;
        let (reader, writer) = std::io::pipe().unwrap();
        (reader.into_raw_fd(), writer.into_raw_fd())
    }
}

#[test]
fn no_process_from_the_task_tree_survives_teardown() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("tree-death");
    let (read_fd, write_fd) = pipe_cloexec();
    let script = "import os, sys, time\n\
        fd = int(sys.argv[1])\n\
        pid = os.fork()\n\
        if pid == 0:\n\
        \x20   os.setsid()\n\
        \x20   if os.fork() == 0:\n\
        \x20       time.sleep(2)\n\
        \x20       os.write(fd, b'ALIVE')\n\
        \x20   os._exit(0)\n\
        os.waitpid(pid, 0)\n";
    let mut command = py_cmd(script);
    command.push(os(&write_fd.to_string()));
    let mut task_spec = spec(command, tmp.path());
    task_spec.pass_fds = vec![write_fd];
    let report = run_task_spec(&task_spec);
    unsafe {
        libc::close(write_fd);
    }
    assert_eq!(report.outcome, Outcome::Task);
    assert_eq!(report.task_exit_code, Some(0));
    let mut data: Vec<u8> = Vec::new();
    let mut eof = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let mut pfd = libc::pollfd {
            fd: read_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, 200) };
        if rc <= 0 {
            continue;
        }
        let mut buf = [0u8; 4096];
        let n = unsafe { libc::read(read_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n == 0 {
            eof = true;
            break;
        }
        if n > 0 {
            data.extend_from_slice(&buf[..n as usize]);
        }
    }
    unsafe {
        libc::close(read_fd);
    }
    assert!(data.is_empty());
    assert!(eof);
}

#[test]
fn no_sandbox_mount_appears_in_the_host_mount_table() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("mount-table");
    let before = std::fs::read_to_string("/proc/self/mounts").unwrap();
    let report = run_task(py_cmd("pass"), tmp.path());
    assert_eq!(report.outcome, Outcome::Task);
    let after = std::fs::read_to_string("/proc/self/mounts").unwrap();
    assert!(!before.contains(&report.sandbox_dir));
    assert!(!after.contains(&report.sandbox_dir));
}

struct LoopbackListener {
    port: u16,
    received: Arc<Mutex<Vec<Vec<u8>>>>,
    stop: Arc<AtomicBool>,
}

impl LoopbackListener {
    fn start() -> LoopbackListener {
        use std::net::TcpListener;
        let sock = TcpListener::bind("127.0.0.1:0").unwrap();
        sock.set_nonblocking(true).unwrap();
        let port = sock.local_addr().unwrap().port();
        let received = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        {
            let received = Arc::clone(&received);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match sock.accept() {
                        Ok((mut conn, _)) => {
                            let mut buf = [0u8; 16];
                            let n = conn.read(&mut buf).unwrap_or(0);
                            received.lock().unwrap().push(buf[..n].to_vec());
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(20)),
                    }
                }
            });
        }
        LoopbackListener {
            port,
            received,
            stop,
        }
    }

    fn received(&self) -> Vec<Vec<u8>> {
        self.received.lock().unwrap().clone()
    }
}

impl Drop for LoopbackListener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

const CONNECT_SCRIPT: &str = "import json, socket, sys\n\
    sock = socket.socket()\n\
    sock.settimeout(3)\n\
    try:\n\
    \x20   sock.connect(('127.0.0.1', int(sys.argv[1])))\n\
    \x20   sock.sendall(b'ping')\n\
    \x20   print(json.dumps({'connected': True}))\n\
    except OSError as exc:\n\
    \x20   print(json.dumps({'connected': False, 'errno': exc.errno}))\n";

#[test]
fn the_default_profile_has_no_network() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("no-net");
    let listener = LoopbackListener::start();
    let mut command = py_cmd(CONNECT_SCRIPT);
    command.push(os(&listener.port.to_string()));
    let report = run_task(command, tmp.path());
    let seen = last_json(&report);
    assert_eq!(seen["connected"], false);
    assert_eq!(seen["errno"], 101);
    std::thread::sleep(Duration::from_millis(300));
    assert!(listener.received().is_empty());
}

#[test]
fn the_default_profile_lists_only_loopback() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("loopback-only");
    let script =
        "import json, socket; print(json.dumps([name for _, name in socket.if_nameindex()]))";
    let report = run_task(py_cmd(script), tmp.path());
    assert_eq!(last_json(&report), serde_json::json!(["lo"]));
}

#[test]
fn the_network_opt_in_shares_the_host_network() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("net-opt-in");
    let listener = LoopbackListener::start();
    let mut command = py_cmd(CONNECT_SCRIPT);
    command.push(os(&listener.port.to_string()));
    let mut task_spec = spec(command, tmp.path());
    task_spec.network = true;
    let report = run_task_spec(&task_spec);
    let seen = last_json(&report);
    assert_eq!(seen["connected"], true);
    assert!(wait_until(|| !listener.received().is_empty(), 3.0));
    assert_eq!(listener.received(), vec![b"ping".to_vec()]);
}

// ---------- the command line ----------

struct CliOutput {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run_cli(args: &[OsString], env_extra: &[(String, String)]) -> CliOutput {
    let output = Command::new(mayfly_bin())
        .args(args)
        .envs(env_extra.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .unwrap();
    CliOutput {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn cli_args(root: &Path, task: &[OsString]) -> Vec<OsString> {
    let mut args = vec![
        os("run"),
        os("--root"),
        root.as_os_str().to_os_string(),
        os("--"),
    ];
    args.extend_from_slice(task);
    args
}

#[test]
fn cli_mirrors_the_task_exit_code() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("cli-exit");
    let result = run_cli(
        &cli_args(tmp.path(), &py_cmd("import sys; sys.exit(7)")),
        &[],
    );
    assert_eq!(result.code, 7);
    let payload: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert_eq!(payload["outcome"], "task");
    assert_eq!(payload["task_exit_code"], 7);
    assert_eq!(payload["backend"], "namespace");
}

#[test]
fn cli_reports_a_signal_death_at_128_plus_the_signal() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("cli-signal");
    let result = run_cli(
        &cli_args(
            tmp.path(),
            &py_cmd("import os, signal; os.kill(os.getpid(), signal.SIGTERM)"),
        ),
        &[],
    );
    assert_eq!(result.code, 143);
    let payload: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert_eq!(payload["task_signal"], 15);
    assert_eq!(payload["task_exit_code"], serde_json::Value::Null);
}

#[test]
fn cli_reports_a_harness_error_at_125() {
    let tmp = Tmp::new("cli-harness");
    let result = run_cli(
        &cli_args(tmp.path(), &cmd(&["mayfly-no-such-binary-anywhere"])),
        &[],
    );
    assert_eq!(result.code, 125);
    assert!(result.stderr.is_empty());
    let payload: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert_eq!(payload["outcome"], "harness_error");
    assert_eq!(payload["task_exit_code"], serde_json::Value::Null);
}

#[test]
fn cli_passes_two_named_allow_env_keys_to_the_task() {
    // MF-A82. The README states that keys named with
    // repeated allow-env flags pass through with their
    // values, and its own demonstration names two keys in
    // one call. The library pins drive a two key allow list
    // through the spec field, which bypasses the parser, and
    // the fake unshare drives pass one flag each, so no
    // command line drive passed two: a parser that replaced
    // the list instead of accumulating would drop the first
    // named key and every pin would still pass. Both keys
    // stand named here, and the task's environment carries
    // both lines.
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("cli-allow-env-two");
    let mut args = vec![
        os("run"),
        os("--root"),
        tmp.path().as_os_str().to_os_string(),
        os("--allow-env"),
        os("PATH"),
        os("--allow-env"),
        os("MAYFLY_KEEP_ME"),
        os("--"),
    ];
    args.extend_from_slice(&cmd(&["/usr/bin/env"]));
    let result = run_cli(
        &args,
        &[("MAYFLY_KEEP_ME".to_string(), "a kept value".to_string())],
    );
    assert_eq!(result.code, 0, "{}", result.stderr);
    let payload: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert_eq!(payload["outcome"], "task");
    let tail = payload["stdout_tail"].as_str().unwrap();
    assert!(tail.contains("PATH="), "{}", tail);
    assert!(tail.contains("MAYFLY_KEEP_ME=a kept value"), "{}", tail);
}

#[test]
fn cli_with_the_net_flag_shares_the_host_network() {
    // The library opt in stands pinned on the spec surface
    // by the_network_opt_in_shares_the_host_network, and
    // the command line's net flag is the same opt in on the
    // surface the README demonstrates it on. The token
    // stands in the suite once elsewhere, as the option
    // shaped prop of the missing value pin, where the
    // parser never takes it as the flag. This pin takes it
    // as the flag, standing before the separator, and reads
    // the run through the listener the library pin uses.
    // Against a parser that refuses the flag the drive
    // answers at 2 with no report and the pin fails on the
    // exit, and against an arm that takes the flag and
    // never sets the field the task answers from its fresh
    // namespace with connected false and the pin fails on
    // the connection.
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("cli-net-opt-in");
    let listener = LoopbackListener::start();
    let mut args = vec![
        os("run"),
        os("--root"),
        tmp.path().as_os_str().to_os_string(),
        os("--net"),
        os("--"),
    ];
    let mut command = py_cmd(CONNECT_SCRIPT);
    command.push(os(&listener.port.to_string()));
    args.extend(command);
    let result = run_cli(&args, &[]);
    assert_eq!(result.code, 0);
    let payload: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert_eq!(payload["outcome"], "task");
    let tail = payload["stdout_tail"].as_str().unwrap();
    let line = tail
        .lines()
        .filter(|line| !line.trim().is_empty())
        .next_back()
        .expect("the task printed no json line");
    let seen: serde_json::Value = serde_json::from_str(line).unwrap();
    assert_eq!(seen["connected"], true);
    assert!(wait_until(|| !listener.received().is_empty(), 3.0));
    assert_eq!(listener.received(), vec![b"ping".to_vec()]);
}

#[test]
fn contract_carries_the_round_one_obligations() {
    let text = std::fs::read_to_string(root().join("CONTRACT.md")).unwrap();
    for number in 1..=13 {
        assert!(
            text.contains(&format!("Obligation MF-{} covers", number)),
            "{}",
            number
        );
    }
    assert!(text.contains("no /dev/kvm"));
    assert!(text.contains("namespace sandbox"));
    let readme = std::fs::read_to_string(root().join("README.md")).unwrap();
    assert!(readme.contains("namespace sandbox"));
}

#[test]
fn the_stderr_tail_carries_a_failing_task_stderr() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("stderr-tail");
    let script = "import sys; sys.stderr.write('a named failure line' + chr(10)); sys.exit(3)";
    let report = run_task(py_cmd(script), tmp.path());
    assert_eq!(report.outcome, Outcome::Task);
    assert_eq!(report.task_exit_code, Some(3));
    assert!(report.stderr_tail.contains("a named failure line"));
}

#[test]
fn a_python_task_sees_the_scrubbed_environment_through_its_interpreter() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("env-interpreter");
    let script = "import json, os; print(json.dumps(dict(os.environ)))";
    let mut task_spec = spec(py_cmd(script), tmp.path());
    task_spec.caller_env = vec![
        (os("PATH"), os("/usr/bin:/bin")),
        (os("MAYFLY_LEAK_PROBE"), os("a secret value")),
    ];
    let report = run_task_spec(&task_spec);
    let seen = last_json(&report);
    assert_eq!(
        seen,
        serde_json::json!({
            "PATH": "/usr/bin:/bin",
            "MAYFLY_SANDBOX": "namespace",
            "PYTHONCOERCECLOCALE": "0",
        })
    );
}

#[test]
fn an_allowlisted_coercion_key_keeps_the_caller_value() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("env-coercion");
    let mut task_spec = spec(cmd(&["/usr/bin/env"]), tmp.path());
    task_spec.allow_env = vec![os("PATH"), os("PYTHONCOERCECLOCALE")];
    task_spec.caller_env = vec![
        (os("PATH"), os("/usr/bin:/bin")),
        (os("PYTHONCOERCECLOCALE"), os("caller-chosen")),
    ];
    let report = run_task_spec(&task_spec);
    let seen = parse_env_output(&report.stdout_tail);
    assert_eq!(
        seen.get("PYTHONCOERCECLOCALE").map(|s| s.as_str()),
        Some("caller-chosen")
    );
}

const CAPS_SCRIPT: &str = "import ctypes, json\n\
    libc = ctypes.CDLL(None, use_errno=True)\n\
    bounding = [cap for cap in range(64) if libc.prctl(23, cap, 0, 0, 0) == 1]\n\
    class Hdr(ctypes.Structure):\n\
    \x20   _fields_ = [('version', ctypes.c_uint32), ('pid', ctypes.c_int)]\n\
    class Data(ctypes.Structure):\n\
    \x20   _fields_ = [('effective', ctypes.c_uint32), ('permitted', ctypes.c_uint32), ('inheritable', ctypes.c_uint32)]\n\
    hdr = Hdr(0x20080522, 0)\n\
    data = (Data * 2)()\n\
    libc.capget(ctypes.byref(hdr), data)\n\
    print(json.dumps({\n\
    \x20 'bounding': bounding,\n\
    \x20 'effective': data[0].effective | (data[1].effective << 32),\n\
    \x20 'permitted': data[0].permitted | (data[1].permitted << 32),\n\
    }))\n";

#[test]
fn the_task_holds_no_capabilities() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("caps");
    let report = run_task(py_cmd(CAPS_SCRIPT), tmp.path());
    let seen = last_json(&report);
    assert_eq!(seen["bounding"], serde_json::json!([]));
    assert_eq!(seen["effective"], 0);
    assert_eq!(seen["permitted"], 0);
}

const ESCAPE_SCRIPT: &str = "import json, os, sys\n\
    secret_path, host_target = sys.argv[1], sys.argv[2]\n\
    result = {'chroot_ok': False, 'secret_read': False, 'host_write': False}\n\
    os.makedirs('/work/trap', exist_ok=True)\n\
    root_fd = os.open('/', os.O_RDONLY)\n\
    try:\n\
    \x20   os.chroot('/work/trap')\n\
    \x20   result['chroot_ok'] = True\n\
    except OSError:\n\
    \x20   pass\n\
    if result['chroot_ok']:\n\
    \x20   os.fchdir(root_fd)\n\
    \x20   for _ in range(24):\n\
    \x20       os.chdir('..')\n\
    \x20   try:\n\
    \x20       os.chroot('.')\n\
    \x20   except OSError:\n\
    \x20       pass\n\
    \x20   try:\n\
    \x20       with open(secret_path) as handle:\n\
    \x20           result['secret_read'] = 'mayfly-host-secret' in handle.read()\n\
    \x20   except OSError:\n\
    \x20       pass\n\
    \x20   try:\n\
    \x20       with open(host_target, 'w') as handle:\n\
    \x20           handle.write('escaped')\n\
    \x20       result['host_write'] = True\n\
    \x20   except OSError:\n\
    \x20       pass\n\
    print(json.dumps(result))\n";

#[test]
fn the_chroot_climb_escape_shape_cannot_reach_the_host() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("escape");
    let secret = tmp.path().join("host-secret.txt");
    std::fs::write(&secret, "mayfly-host-secret").unwrap();
    let host_target = tmp.path().join("escaped-write.txt");
    let mut command = py_cmd(ESCAPE_SCRIPT);
    command.push(secret.into_os_string());
    command.push(host_target.clone().into_os_string());
    let report = run_task(command, tmp.path());
    let seen = last_json(&report);
    assert_eq!(seen["chroot_ok"], false);
    assert_eq!(seen["secret_read"], false);
    assert_eq!(seen["host_write"], false);
    assert!(!host_target.exists());
}

#[test]
fn a_submount_of_a_bound_tree_is_read_only_inside() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("submount");
    let probe_dir = PathBuf::from(format!("/etc/mayfly-subprobe-{}", &unique_hex()[..12]));
    if std::fs::create_dir(&probe_dir).is_err() {
        eprintln!("this machine would not let the probe create a directory under /etc");
        return;
    }
    let mounted = Command::new("mount")
        .args(["-t", "tmpfs", "tmpfs"])
        .arg(&probe_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !mounted {
        let _ = std::fs::remove_dir(&probe_dir);
        eprintln!("this machine would not create a host submount to probe");
        return;
    }
    let probe_file = probe_dir.join("probe.txt");
    let script = format!(
        "import json, os\n\
         errno = 0\n\
         try:\n\
         \x20   handle = open('{}', 'w')\n\
         \x20   handle.close()\n\
         except OSError as exc:\n\
         \x20   errno = exc.errno\n\
         print(json.dumps({{'errno': errno}}))\n",
        probe_file.display()
    );
    let report = run_task(py_cmd(&script), tmp.path());
    let outcome = report.outcome;
    let errno = last_json(&report)["errno"].clone();
    let probe_exists = probe_file.exists();
    let _ = Command::new("umount")
        .arg(&probe_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = std::fs::remove_dir(&probe_dir);
    assert_eq!(outcome, Outcome::Task);
    assert_eq!(errno, 30);
    assert!(!probe_exists);
}

#[test]
fn a_command_with_a_nul_byte_is_a_harness_error() {
    let tmp = Tmp::new("nul-byte");
    let bad = OsString::from_vec(b"mayfly-bad\0command".to_vec());
    let report = run_task(vec![bad], tmp.path());
    assert_eq!(report.outcome, Outcome::HarnessError);
    assert_eq!(report.task_exit_code, None);
    assert!(report.error.is_some());
}

#[test]
fn a_prepare_failure_report_carries_the_full_shape() {
    let tmp = Tmp::new("prepare-shape");
    let blocker = tmp.path().join("a-file-not-a-directory");
    std::fs::write(&blocker, "standing in the way").unwrap();
    let report = run_task(py_cmd("pass"), &blocker);
    assert_eq!(report.outcome, Outcome::HarnessError);
    assert!(report.error.is_some());
    assert_eq!(report.proc_mounted, Some(false));
    assert!(report.sandbox_removed.is_some());
    let json = report.to_json();
    for phase in PHASE_NAMES {
        assert!(json.contains(&format!("\"{}\"", phase)), "{}", phase);
    }
}

#[test]
fn a_large_task_output_is_bounded_to_its_tail() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("large-output");
    let script = "import sys\n\
        sys.stdout.write('x' * 300000)\n\
        sys.stdout.write('the-end-marker')\n";
    let report = run_task(py_cmd(script), tmp.path());
    assert_eq!(report.outcome, Outcome::Task);
    assert!(report.stdout_tail.chars().count() <= TAIL_CHARS);
    assert!(report.stdout_tail.ends_with("the-end-marker"));
}

#[test]
fn a_large_task_error_output_is_bounded_to_its_tail() {
    // The output tail's bound stands pinned at the run
    // level by the pin above, and the buffer stands pinned
    // over small feeds by the pin below. The error stream
    // is drained by its own spawn block into a buffer of
    // its own, and no pin read that seat: a drain that
    // accumulated the stream without the buffer would
    // leave the report's error tail unbounded, and one
    // that dropped the stream or fed it into the output
    // buffer would leave it empty. This pin drives the
    // same oversized body down the error stream and reads
    // the tail the report carries for it.
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("large-error-output");
    let script = "import sys\n\
        sys.stderr.write('x' * 300000)\n\
        sys.stderr.write('the-end-marker')\n";
    let report = run_task(py_cmd(script), tmp.path());
    assert_eq!(report.outcome, Outcome::Task);
    assert!(report.stderr_tail.chars().count() <= TAIL_CHARS);
    assert!(report.stderr_tail.ends_with("the-end-marker"));
}

#[test]
fn tail_buffer_reads_back_the_last_chars_after_small_feeds() {
    let buffer = mayfly::TailBuffer::new();
    let unit = "aé€b";
    let mut fed = String::new();
    while fed.len() < 3 * mayfly::TAIL_BYTES {
        fed.push_str(unit);
    }
    let bytes = fed.as_bytes();
    let split = bytes.len() - 4096;
    for chunk in bytes[..split].chunks(100) {
        buffer.feed(chunk);
    }
    for byte in &bytes[split..] {
        buffer.feed(&[*byte]);
    }
    let expected: String = fed.chars().skip(fed.chars().count() - TAIL_CHARS).collect();
    assert_eq!(buffer.text(), expected);
}

// ---------- supervisor death and the layered cleanup ----------

fn signal_child(child: &Child, signal: i32) {
    unsafe {
        libc::kill(child.id() as i32, signal);
    }
}

fn wait_child(child: &mut Child, timeout_secs: f64) -> i32 {
    let deadline = Instant::now() + Duration::from_secs_f64(timeout_secs);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code().unwrap_or(-1),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    panic!("a child did not exit inside {} seconds", timeout_secs);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => panic!("waiting on a child failed, {}", err),
        }
    }
}

fn communicate(child: &mut Child, timeout_secs: f64) -> (i32, String) {
    let mut stdout = child.stdout.take();
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let reader = std::thread::spawn(move || {
        let mut data = Vec::new();
        if let Some(mut out) = stdout.take() {
            let _ = out.read_to_end(&mut data);
        }
        let _ = tx.send(data);
    });
    if let Some(mut err) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut sink = Vec::new();
            let _ = err.read_to_end(&mut sink);
        });
    }
    let code = wait_child(child, timeout_secs);
    let data = rx.recv_timeout(Duration::from_secs(10)).unwrap_or_default();
    let _ = reader.join();
    (code, String::from_utf8_lossy(&data).into_owned())
}

fn start_cli_with_sleeping_task(root: &Path, marker: &str) -> Child {
    let script = format!("import time\n# {}\ntime.sleep(60)\n", marker);
    let args = cli_args(root, &py_cmd(&script));
    let proc = Command::new(mayfly_bin())
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let started = wait_until(
        || host_pids_with_marker(marker).len() >= 4 && !glob_run_dirs(root).is_empty(),
        30.0,
    );
    assert!(started, "the sleeping task never came up");
    proc
}

fn let_the_run_get_under_way() {
    // Give a started run one clear moment before a pin acts on
    // it. The death pins are about a run whose task is under way
    // and whose started event has reached the runner. The task
    // process becomes visible a breath before the init writes
    // that event, so a pin that acts the instant the task
    // appears can land inside the breath and test a different
    // scenario than it names.
    std::thread::sleep(Duration::from_secs(1));
}

#[test]
fn sigterm_to_the_supervisor_still_tears_the_sandbox_down() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("sigterm");
    let marker = format!("mayfly-death-{}", unique_hex());
    let mut proc = start_cli_with_sleeping_task(tmp.path(), &marker);
    signal_child(&proc, libc::SIGTERM);
    let (code, out) = communicate(&mut proc, 30.0);
    assert_eq!(code, 125);
    let payload: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(payload["outcome"], "harness_error");
    assert!(wait_until(
        || host_pids_with_marker(&marker).is_empty(),
        15.0
    ));
    assert!(glob_run_dirs(tmp.path()).is_empty());
}

#[test]
fn sigkill_to_the_supervisor_leaves_no_process_and_no_sandbox() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("sigkill");
    let marker = format!("mayfly-death-{}", unique_hex());
    let mut proc = start_cli_with_sleeping_task(tmp.path(), &marker);
    signal_child(&proc, libc::SIGKILL);
    wait_child(&mut proc, 30.0);
    assert!(wait_until(
        || host_pids_with_marker(&marker).is_empty(),
        30.0
    ));
    assert!(wait_until(|| glob_run_dirs(tmp.path()).is_empty(), 30.0));
    assert!(glob_clean_markers(tmp.path()).is_empty());
}

fn plant_marker_and_sigkill(tmp: &Path, marker_content: &[u8]) {
    let marker = format!("mayfly-death-{}", unique_hex());
    let mut proc = start_cli_with_sleeping_task(tmp, &marker);
    let sandboxes = glob_run_dirs(tmp);
    assert_eq!(sandboxes.len(), 1);
    let run_id = sandboxes[0]
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let run_id = run_id.strip_prefix("run-").unwrap().to_string();
    let planted = tmp.join(format!(".mayfly-clean-{}", run_id));
    std::fs::write(&planted, marker_content).unwrap();
    signal_child(&proc, libc::SIGKILL);
    wait_child(&mut proc, 30.0);
    assert!(wait_until(
        || host_pids_with_marker(&marker).is_empty(),
        30.0
    ));
    assert!(wait_until(|| glob_run_dirs(tmp).is_empty(), 30.0));
    assert!(glob_clean_markers(tmp).is_empty());
}

#[test]
fn a_planted_clean_marker_does_not_stand_the_janitor_down() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("planted-marker");
    plant_marker_and_sigkill(tmp.path(), b"planted by anyone who can list the root\n");
}

#[test]
fn a_forged_token_in_a_planted_marker_does_not_stand_the_janitor_down() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("forged-marker");
    let forged = format!("{}\n", "0".repeat(32));
    plant_marker_and_sigkill(tmp.path(), forged.as_bytes());
}

#[test]
fn a_clean_run_leaves_no_marker_and_no_sandbox() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("clean-run");
    let report = run_task(py_cmd("print('done')"), tmp.path());
    assert_eq!(report.outcome, Outcome::Task);
    assert_eq!(report.sandbox_removed, Some(true));
    assert!(dir_names(tmp.path()).is_empty());
}

#[test]
fn a_later_run_sweeps_an_orphan_whose_janitor_also_died() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("orphan-sweep");
    let marker = format!("mayfly-death-{}", unique_hex());
    let mut proc = start_cli_with_sleeping_task(tmp.path(), &marker);
    let sandboxes = glob_run_dirs(tmp.path());
    assert_eq!(sandboxes.len(), 1);
    let sandbox = sandboxes[0].clone();
    let janitors = pids_running_helper("__janitor", Some(sandbox.as_os_str().as_bytes()));
    assert_eq!(janitors.len(), 1);
    unsafe {
        libc::kill(janitors[0] as i32, libc::SIGKILL);
    }
    signal_child(&proc, libc::SIGKILL);
    wait_child(&mut proc, 30.0);
    assert!(wait_until(
        || host_pids_with_marker(&marker).is_empty(),
        30.0
    ));
    assert!(sandbox.exists());
    age_directory(&sandbox, 60.0);
    let report = run_task(py_cmd("pass"), tmp.path());
    assert_eq!(report.outcome, Outcome::Task);
    assert!(!sandbox.exists());
    assert!(dir_names(tmp.path()).is_empty());
}

#[test]
fn the_sweep_never_touches_a_live_run() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("live-run");
    let marker = format!("mayfly-death-{}", unique_hex());
    let mut proc = start_cli_with_sleeping_task(tmp.path(), &marker);
    let sandboxes = glob_run_dirs(tmp.path());
    assert_eq!(sandboxes.len(), 1);
    let sandbox = sandboxes[0].clone();
    age_directory(&sandbox, 60.0);
    let report = run_task(py_cmd("pass"), tmp.path());
    assert_eq!(report.outcome, Outcome::Task);
    assert!(sandbox.exists());
    assert!(!host_pids_with_marker(&marker).is_empty());
    signal_child(&proc, libc::SIGTERM);
    let (code, out) = communicate(&mut proc, 30.0);
    assert_eq!(code, 125);
    let payload: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(payload["outcome"], "harness_error");
    assert!(wait_until(
        || host_pids_with_marker(&marker).is_empty(),
        15.0
    ));
    assert!(glob_run_dirs(tmp.path()).is_empty());
}

#[test]
fn a_sandbox_killed_mid_task_does_not_claim_the_task_never_started() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("killed-mid-task");
    let marker = format!("mayfly-death-{}", unique_hex());
    let mut proc = start_cli_with_sleeping_task(tmp.path(), &marker);
    let_the_run_get_under_way();
    let sandboxes = glob_run_dirs(tmp.path());
    assert_eq!(sandboxes.len(), 1);
    let inits = pids_running_helper("__init", Some(sandboxes[0].as_os_str().as_bytes()));
    assert_eq!(inits.len(), 1);
    unsafe {
        libc::kill(inits[0] as i32, libc::SIGKILL);
    }
    let (code, out) = communicate(&mut proc, 30.0);
    assert_eq!(code, 125);
    let payload: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(payload["outcome"], "harness_error");
    let error = payload["error"].as_str().unwrap();
    assert!(!error.contains("without starting the task"), "{}", error);
    assert!(error.contains("after the task started"), "{}", error);
    assert_eq!(payload["timings_ms"]["task"], serde_json::Value::Null);
    assert!(wait_until(
        || host_pids_with_marker(&marker).is_empty(),
        15.0
    ));
    assert!(glob_run_dirs(tmp.path()).is_empty());
}

#[test]
fn a_sandbox_that_dies_before_the_task_starts_keeps_the_plain_report() {
    let tmp = Tmp::new("fake-unshare");
    let fake_unshare = tmp.path().join("fake-unshare");
    std::fs::write(&fake_unshare, "#!/bin/sh\nexit 3\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_unshare, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let root_dir = tmp.path().join("root");
    ensure_helper();
    let backend = NamespaceBackend::with_unshare_path(Some(fake_unshare));
    let report = backend.run(&spec(py_cmd("pass"), &root_dir));
    assert_eq!(report.outcome, Outcome::HarnessError);
    assert!(
        report
            .error
            .as_deref()
            .unwrap_or("")
            .contains("without starting the task"),
        "{:?}",
        report.error
    );
    assert_eq!(report.timings_ms.start, None);
    assert_eq!(report.timings_ms.task, None);
    assert_eq!(report.sandbox_removed, Some(true));
    assert!(dir_names(&root_dir).is_empty());
}

#[test]
fn an_interrupted_run_reports_the_unfinished_task_phase_as_null() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("interrupted");
    let marker = format!("mayfly-death-{}", unique_hex());
    let mut proc = start_cli_with_sleeping_task(tmp.path(), &marker);
    let_the_run_get_under_way();
    signal_child(&proc, libc::SIGINT);
    let (code, out) = communicate(&mut proc, 30.0);
    assert_eq!(code, 125);
    let payload: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(payload["outcome"], "harness_error");
    assert_eq!(payload["timings_ms"]["task"], serde_json::Value::Null);
    assert!(payload["timings_ms"]["start"].as_f64().unwrap() > 0.0);
    assert!(payload["timings_ms"]["teardown"].as_f64().unwrap() > 0.0);
    assert!(payload["timings_ms"]["total"].as_f64().unwrap() > 0.0);
    assert!(wait_until(
        || host_pids_with_marker(&marker).is_empty(),
        15.0
    ));
    assert!(glob_run_dirs(tmp.path()).is_empty());
}

#[test]
fn two_signals_at_once_name_one_whole_first_signal() {
    // Two signals delivered at the same moment can be handled
    // on different threads of the supervisor. The report must
    // name one whole first signal, one of the two that were
    // sent, with the run torn down as an interrupted run. A
    // snapshot that mixed the pair could name a signal that
    // was never sent or lose the interrupt entirely, and the
    // assertions below read for exactly those shapes. The
    // window the Round 1 register names for this finding is
    // two deliveries inside one load to store gap, which no
    // pin can stage on purpose, so this pin drives the
    // deliveries as close together as two threads released
    // from one barrier can make them and holds the report's
    // coherence against whatever interleaving the host picks.
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("two-signals");
    let marker = format!("mayfly-death-{}", unique_hex());
    let mut proc = start_cli_with_sleeping_task(tmp.path(), &marker);
    let_the_run_get_under_way();
    let pid = proc.id();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let mut senders = Vec::new();
    for signum in [libc::SIGINT, libc::SIGTERM] {
        let barrier = Arc::clone(&barrier);
        senders.push(std::thread::spawn(move || {
            barrier.wait();
            unsafe {
                libc::kill(pid as i32, signum);
            }
        }));
    }
    for sender in senders {
        let _ = sender.join();
    }
    let (code, out) = communicate(&mut proc, 30.0);
    assert_eq!(code, 125);
    let payload: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(payload["outcome"], "harness_error");
    let error = payload["error"].as_str().unwrap();
    assert!(
        error.contains("interrupted by signal 2") || error.contains("interrupted by signal 15"),
        "{}",
        error
    );
    assert!(wait_until(
        || host_pids_with_marker(&marker).is_empty(),
        15.0
    ));
    assert!(glob_run_dirs(tmp.path()).is_empty());
}

// ---------- wording pins ----------

#[test]
fn the_supported_architectures_are_the_documented_ones() {
    let init_source = std::fs::read_to_string(root().join("src/init.rs")).unwrap();
    let expected: [(&str, [(&str, i64); 2]); 2] = [
        ("SYS_PIVOT_ROOT", [("x86_64", 155), ("aarch64", 41)]),
        ("SYS_MOUNT_SETATTR", [("x86_64", 442), ("aarch64", 442)]),
    ];
    for (table, numbers) in expected {
        let line = init_source
            .lines()
            .find(|line| {
                line.trim_start()
                    .starts_with(&format!("pub const {}", table))
            })
            .unwrap_or_else(|| panic!("no line names {}", table));
        let mut found: BTreeMap<String, i64> = BTreeMap::new();
        for (name, value) in numbers {
            let needle = format!("(\"{}\", {})", name, value);
            assert!(line.contains(&needle), "{} in {}", needle, line);
            found.insert(name.to_string(), value);
        }
        assert_eq!(found.len(), 2, "{}", table);
    }
    let contract = std::fs::read_to_string(root().join("CONTRACT.md")).unwrap();
    let readme = std::fs::read_to_string(root().join("README.md")).unwrap();
    for arch in ["x86_64", "aarch64"] {
        assert!(contract.contains(arch));
        assert!(readme.contains(arch));
    }
}

#[test]
fn the_contract_promises_no_more_than_the_provenance_check_holds() {
    let contract = std::fs::read_to_string(root().join("CONTRACT.md")).unwrap();
    let mf12 = &contract[contract.find("Obligation MF-12 covers").unwrap()..];
    assert!(!mf12.contains("never swept however it is named"));
    assert!(mf12.contains("it is not authentication"));
    assert!(mf12.contains("written again by the init"));
    assert!(mf12.contains("when its assembly of the prepared root begins"));
    assert!(mf12.contains("lands whole or not at all"));
    assert!(!mf12.contains("can leave an empty run directory that no sweep collects"));
    assert!(mf12.contains("before the born provenance write lands"));
    assert!(mf12.contains("double kill that takes the janitor with the supervisor"));
}

#[test]
fn the_readme_cleanup_claim_carries_the_sweep_fallback() {
    let readme = std::fs::read_to_string(root().join("README.md")).unwrap();
    assert!(!readme.contains("leaves no task running and no sandbox directory behind"));
    assert!(readme.contains("the next run under the same root sweeps the directory away"));
    assert!(!readme.contains("killed in the instant its sandbox directory is created"));
    assert!(readme.contains("killed before its provenance is written"));
}

#[test]
fn the_contract_cleanup_claim_carries_the_layered_cleanup() {
    let contract = std::fs::read_to_string(root().join("CONTRACT.md")).unwrap();
    let mf12 = &contract[contract.find("Obligation MF-12 covers").unwrap()..];
    assert!(!mf12.contains("leaves no task running and no sandbox directory behind"));
    assert!(mf12.contains("once the layered cleanup this paragraph describes has run"));
}

// ---------- standing directory refusals ----------

fn run_with_a_fixed_run_id(run_id: &str, root: &Path) -> RunReport {
    ensure_helper();
    let mut backend = NamespaceBackend::new();
    backend.run_id_override = Some(run_id.to_string());
    backend.run(&spec(py_cmd("pass"), root))
}

#[test]
fn a_run_refuses_a_standing_run_directory_and_leaves_it_standing() {
    let tmp = Tmp::new("standing-dir");
    let run_id = "deadbeef0001";
    let sandbox = tmp.path().join(format!("run-{}", run_id));
    std::fs::create_dir_all(&sandbox).unwrap();
    std::fs::write(sandbox.join("sentinel.txt"), "planted before the run\n").unwrap();
    let before = snapshot_tree(&sandbox);
    let report = run_with_a_fixed_run_id(run_id, tmp.path());
    assert_eq!(report.run_id, run_id);
    assert_eq!(report.outcome, Outcome::HarnessError);
    assert!(report
        .error
        .as_deref()
        .unwrap_or("")
        .contains("prepare failed"));
    assert_eq!(report.sandbox_removed, Some(false));
    assert_eq!(snapshot_tree(&sandbox), before);
    assert_eq!(
        dir_names(tmp.path()),
        vec![sandbox.file_name().unwrap().to_string_lossy().into_owned()]
    );
}

#[test]
fn a_run_refuses_a_standing_rootfs_and_leaves_it_standing() {
    let tmp = Tmp::new("standing-rootfs");
    let run_id = "deadbeef0002";
    let sandbox = tmp.path().join(format!("run-{}", run_id));
    let rootfs = sandbox.join("rootfs");
    std::fs::create_dir_all(&rootfs).unwrap();
    std::fs::write(rootfs.join("sentinel.txt"), "planted before the run\n").unwrap();
    let before = snapshot_tree(&sandbox);
    let report = run_with_a_fixed_run_id(run_id, tmp.path());
    assert_eq!(report.run_id, run_id);
    assert_eq!(report.outcome, Outcome::HarnessError);
    assert!(report
        .error
        .as_deref()
        .unwrap_or("")
        .contains("prepare failed"));
    assert_eq!(report.sandbox_removed, Some(false));
    assert_eq!(snapshot_tree(&sandbox), before);
    assert_eq!(
        dir_names(tmp.path()),
        vec![sandbox.file_name().unwrap().to_string_lossy().into_owned()]
    );
}

// ---------- the janitor driven directly ----------

/// Run one janitor the way the runner runs it, with a dead
/// supervisor. The token arrives over standard input and the
/// watch pipe is closed at once, which is the supervisor dying
/// without a word.
fn spawn_janitor(sandbox: &Path, marker: &Path, token: &str) -> Child {
    let (read_fd, write_fd) = pipe_cloexec();
    let mut cmd = Command::new(mayfly_bin());
    cmd.arg("__janitor")
        .arg("--sandbox")
        .arg(sandbox.as_os_str())
        .arg("--marker")
        .arg(marker.as_os_str())
        .arg("--fd")
        .arg(read_fd.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        cmd.pre_exec(move || {
            let flags = libc::fcntl(read_fd, libc::F_GETFD);
            if flags >= 0 {
                libc::fcntl(read_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
            }
            Ok(())
        });
    }
    let mut proc = cmd.spawn().unwrap();
    unsafe {
        libc::close(read_fd);
    }
    if let Some(mut stdin) = proc.stdin.take() {
        let _ = stdin.write_all(token.as_bytes());
        let _ = stdin.write_all(b"\n");
        drop(stdin);
    }
    unsafe {
        libc::close(write_fd);
    }
    proc
}

fn drive_janitor(sandbox: &Path, marker: &Path, token: &str) -> i32 {
    let mut proc = spawn_janitor(sandbox, marker, token);
    wait_child(&mut proc, 40.0)
}

#[test]
fn the_janitor_marker_read_is_bounded_and_bad_bytes_read_as_no_token() {
    let tmp = Tmp::new("janitor-marker");
    let token = "ab".repeat(16);
    let mut padded = format!("{}{}", token, " ".repeat(200)).into_bytes();
    padded.extend_from_slice(b"\n");
    let mut badbytes = token.clone().into_bytes();
    badbytes.extend_from_slice(b"\xff\xfe\n");
    let cases: Vec<(&str, Vec<u8>, bool)> = vec![
        ("genuine", format!("{}\n", token).into_bytes(), true),
        ("padded", padded, false),
        ("badbytes", badbytes, false),
    ];
    for (name, content, stands_down) in cases {
        let run_dir = tmp.path().join(name);
        let sandbox = run_dir.join("sandbox");
        std::fs::create_dir_all(&sandbox).unwrap();
        std::fs::write(sandbox.join("leftover.txt"), "left behind").unwrap();
        let marker = run_dir.join("clean-marker");
        std::fs::write(&marker, &content).unwrap();
        assert_eq!(drive_janitor(&sandbox, &marker, &token), 0, "{}", name);
        if stands_down {
            assert!(sandbox.exists(), "{}", name);
        } else {
            assert!(!sandbox.exists(), "{}", name);
        }
    }
}

#[test]
fn an_empty_token_authenticates_nothing() {
    let tmp = Tmp::new("janitor-empty-token");
    let sandbox = tmp.path().join("sandbox");
    std::fs::create_dir_all(&sandbox).unwrap();
    std::fs::write(sandbox.join("leftover.txt"), "left behind").unwrap();
    let marker = tmp.path().join("clean-marker");
    std::fs::write(&marker, b"\n").unwrap();
    assert_eq!(drive_janitor(&sandbox, &marker, ""), 0);
    assert!(!sandbox.exists());
    assert!(!marker.exists());
}

#[test]
fn a_planted_marker_with_bytes_that_are_not_text_does_not_stand_the_janitor_down() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("planted-badbytes");
    let content: Vec<u8> = [0xffu8, 0xfe].repeat(64);
    plant_marker_and_sigkill(tmp.path(), &content);
}

#[test]
fn a_planted_oversized_marker_does_not_stand_the_janitor_down() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("planted-oversized");
    let content = vec![b'x'; 32 << 20];
    plant_marker_and_sigkill(tmp.path(), &content);
}

// ---------- sweep shapes ----------

#[test]
fn the_sweep_skips_a_run_whose_liveness_is_unknown() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("sweep-unknown");
    let marker = format!("mayfly-death-{}", unique_hex());
    let mut proc = start_cli_with_sleeping_task(tmp.path(), &marker);
    let sandboxes = glob_run_dirs(tmp.path());
    assert_eq!(sandboxes.len(), 1);
    let sandbox = sandboxes[0].clone();
    age_directory(&sandbox, 60.0);
    let bogus_proc = tmp.path().join("no-such-proc-table");
    assert_eq!(pids_referencing_in(&bogus_proc, &sandbox), None);
    sweep_orphans_in(tmp.path(), &bogus_proc);
    assert!(sandbox.exists());
    signal_child(&proc, libc::SIGTERM);
    let (code, out) = communicate(&mut proc, 30.0);
    assert_eq!(code, 125);
    let payload: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(payload["outcome"], "harness_error");
    assert!(wait_until(
        || host_pids_with_marker(&marker).is_empty(),
        15.0
    ));
    assert!(glob_run_dirs(tmp.path()).is_empty());
}

#[test]
fn the_sweep_leaves_a_foreign_run_directory_standing() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("sweep-foreign");
    let root_dir = tmp.path().join("root");
    let foreign = root_dir.join("run-foreign0001");
    std::fs::create_dir_all(&foreign).unwrap();
    std::fs::write(foreign.join("not-mayfly.txt"), "not a mayfly sandbox").unwrap();
    age_directory(&foreign, 60.0);
    let report = run_task(py_cmd("pass"), &root_dir);
    assert_eq!(report.outcome, Outcome::Task);
    assert_eq!(
        std::fs::read_to_string(foreign.join("not-mayfly.txt")).unwrap(),
        "not a mayfly sandbox"
    );
    assert_eq!(glob_run_dirs(&root_dir), vec![foreign]);
}

#[test]
fn the_sweep_leaves_a_directory_whose_provenance_names_another_run() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("sweep-mismatch");
    let root_dir = tmp.path().join("root");
    let foreign = root_dir.join("run-aaaa00000001");
    std::fs::create_dir_all(&foreign).unwrap();
    std::fs::write(foreign.join(".mayfly-provenance"), "bbbb00000002\n").unwrap();
    age_directory(&foreign, 60.0);
    let report = run_task(py_cmd("pass"), &root_dir);
    assert_eq!(report.outcome, Outcome::Task);
    assert!(foreign.exists());
    assert_eq!(glob_run_dirs(&root_dir), vec![foreign]);
}

#[test]
fn a_provenance_with_bytes_that_are_not_text_leaves_the_directory_standing() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("sweep-nontext");
    let root_dir = tmp.path().join("root");
    let planted = root_dir.join("run-cccc00000003");
    std::fs::create_dir_all(&planted).unwrap();
    std::fs::write(planted.join(".mayfly-provenance"), b"\xff\xfe\x00binary").unwrap();
    age_directory(&planted, 60.0);
    let report = run_task(py_cmd("pass"), &root_dir);
    assert_eq!(report.outcome, Outcome::Task);
    assert_eq!(
        std::fs::read(planted.join(".mayfly-provenance")).unwrap(),
        b"\xff\xfe\x00binary"
    );
    assert_eq!(glob_run_dirs(&root_dir), vec![planted]);
}

#[test]
fn the_sweep_leaves_a_symlink_at_a_run_directory_position_standing() {
    // The red team shape. A symlink stands at a run directory
    // position in the run root, naming a directory outside the
    // root that carries a provenance file forged to name the
    // link's own run id, aged past the grace period, with a
    // process table that reads clean and names nothing live.
    // The sweep judges the entry standing at the position
    // itself and never through a link, so the directory behind
    // the link keeps every byte and the link itself stands,
    // where a sweep that followed the link would read the
    // forged provenance as its own and unlink the link.
    let tmp = Tmp::new("sweep-symlink");
    let root_dir = tmp.path().join("root");
    std::fs::create_dir_all(&root_dir).unwrap();
    let victim = tmp.path().join("victim-dir");
    std::fs::create_dir_all(&victim).unwrap();
    std::fs::write(victim.join(".mayfly-provenance"), "eeee00000009\n").unwrap();
    std::fs::write(victim.join("precious.txt"), "not mayfly data").unwrap();
    let link = root_dir.join("run-eeee00000009");
    std::os::unix::fs::symlink(&victim, &link).unwrap();
    age_directory(&victim, 60.0);
    let fake = fake_proc_table(
        &tmp.path().join("fakeproc-link"),
        &[("424242", FakeCmdline::Bytes(b"unrelated\0".to_vec()))],
    );
    sweep_orphans_in(&root_dir, &fake);
    let meta = std::fs::symlink_metadata(&link).unwrap();
    assert!(meta.file_type().is_symlink());
    assert_eq!(std::fs::read_link(&link).unwrap(), victim);
    assert_eq!(
        std::fs::read_to_string(victim.join("precious.txt")).unwrap(),
        "not mayfly data"
    );
    assert_eq!(
        std::fs::read_to_string(victim.join(".mayfly-provenance")).unwrap(),
        "eeee00000009\n"
    );
}

fn run_task_on_a_thread(
    root: &Path,
    command: Vec<OsString>,
) -> (
    std::sync::mpsc::Receiver<RunReport>,
    std::thread::JoinHandle<()>,
) {
    let (tx, rx) = std::sync::mpsc::channel();
    let root = root.to_path_buf();
    let handle = std::thread::spawn(move || {
        let report = run_task(command, &root);
        let _ = tx.send(report);
    });
    (rx, handle)
}

#[test]
fn a_fifo_at_the_provenance_path_neither_hangs_nor_breaks_the_run() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("fifo-provenance");
    let root_dir = tmp.path().join("root");
    let planted = root_dir.join("run-dddd00000004");
    std::fs::create_dir_all(&planted).unwrap();
    mkfifo(&planted.join(".mayfly-provenance"));
    age_directory(&planted, 60.0);
    let (rx, handle) = run_task_on_a_thread(&root_dir, py_cmd("pass"));
    let report = rx
        .recv_timeout(Duration::from_secs(60))
        .expect("the triggered run hung on the planted pipe");
    let _ = handle.join();
    assert_eq!(report.outcome, Outcome::Task);
    assert!(planted.exists());
    assert_eq!(glob_run_dirs(&root_dir), vec![planted]);
}

#[test]
fn a_fifo_at_the_marker_path_does_not_keep_the_sandbox() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("fifo-marker-kill");
    let marker = format!("mayfly-death-{}", unique_hex());
    let mut proc = start_cli_with_sleeping_task(tmp.path(), &marker);
    let sandboxes = glob_run_dirs(tmp.path());
    assert_eq!(sandboxes.len(), 1);
    let run_id = sandboxes[0]
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let run_id = run_id.strip_prefix("run-").unwrap().to_string();
    mkfifo(&tmp.path().join(format!(".mayfly-clean-{}", run_id)));
    signal_child(&proc, libc::SIGKILL);
    wait_child(&mut proc, 30.0);
    assert!(wait_until(
        || host_pids_with_marker(&marker).is_empty(),
        30.0
    ));
    assert!(wait_until(|| glob_run_dirs(tmp.path()).is_empty(), 30.0));
    assert!(glob_clean_markers(tmp.path()).is_empty());
}

#[test]
fn a_fifo_at_the_marker_path_does_not_block_a_clean_run() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("fifo-marker-clean");
    let root_dir = tmp.path().join("root");
    std::fs::create_dir_all(&root_dir).unwrap();
    let (rx, handle) = run_task_on_a_thread(&root_dir, py_cmd("import time; time.sleep(4)"));
    assert!(wait_until(|| glob_run_dirs(&root_dir).len() == 1, 15.0));
    let sandbox = glob_run_dirs(&root_dir)[0].clone();
    let run_id = sandbox.file_name().unwrap().to_string_lossy().into_owned();
    let run_id = run_id.strip_prefix("run-").unwrap().to_string();
    mkfifo(&root_dir.join(format!(".mayfly-clean-{}", run_id)));
    let report = rx
        .recv_timeout(Duration::from_secs(60))
        .expect("the clean run blocked on the planted pipe");
    let _ = handle.join();
    assert_eq!(report.outcome, Outcome::Task);
    assert_eq!(report.sandbox_removed, Some(true));
    assert!(dir_names(&root_dir).is_empty());
}

#[test]
fn the_janitor_reads_a_fifo_marker_as_no_token() {
    let tmp = Tmp::new("janitor-fifo");
    let sandbox = tmp.path().join("sandbox");
    std::fs::create_dir_all(&sandbox).unwrap();
    std::fs::write(sandbox.join("leftover.txt"), "left behind").unwrap();
    let marker = tmp.path().join("clean-marker");
    mkfifo(&marker);
    assert_eq!(drive_janitor(&sandbox, &marker, &"ab".repeat(16)), 0);
    assert!(!sandbox.exists());
}

#[test]
fn a_symlink_at_the_marker_path_is_never_written_through() {
    // The runner leaves the clean marker through the marker
    // write. A symlink planted at the marker path, naming a
    // file outside the run root, must leave that file byte
    // identical and the write skipped, with the link itself
    // left standing for the janitor to remove in its cleanup.
    // A write that reaches the named file, whether by opening
    // through the link or by replacing the link with a written
    // marker of its own, fails this pin.
    let tmp = Tmp::new("marker-symlink");
    let victim = tmp.path().join("victim.txt");
    std::fs::write(&victim, "the file outside the run root\n").unwrap();
    let marker = tmp.path().join(".mayfly-clean-eeee00000008");
    std::os::unix::fs::symlink(&victim, &marker).unwrap();
    mayfly::namespace::write_clean_marker(&marker, &"ab".repeat(16));
    assert_eq!(
        std::fs::read(&victim).unwrap(),
        b"the file outside the run root\n"
    );
    let meta = std::fs::symlink_metadata(&marker).unwrap();
    assert!(meta.file_type().is_symlink());
    assert_eq!(std::fs::read_link(&marker).unwrap(), victim);
}

#[test]
fn the_janitor_removes_a_sandbox_that_is_already_gone_as_a_noop() {
    let tmp = Tmp::new("janitor-noop");
    let sandbox = tmp.path().join("sandbox-that-is-not-there");
    let marker = tmp.path().join("clean-marker-that-is-not-there");
    assert_eq!(drive_janitor(&sandbox, &marker, &"ab".repeat(16)), 0);
    assert!(!sandbox.exists());
}

#[test]
fn the_janitor_removes_a_sandbox_recreated_behind_its_removal() {
    let tmp = Tmp::new("janitor-watch");
    let sandbox = tmp.path().join("run-watch00000001");
    std::fs::create_dir_all(sandbox.join("rootfs").join("usr")).unwrap();
    let marker = tmp.path().join(".mayfly-clean-watch00000001");
    let mut proc = spawn_janitor(&sandbox, &marker, &"cd".repeat(16));
    let first = wait_until(|| !sandbox.exists(), 20.0);
    assert!(first);
    std::fs::create_dir_all(sandbox.join("rootfs").join("usr")).unwrap();
    let second = wait_until(|| !sandbox.exists(), 20.0);
    let code = if proc.try_wait().ok().flatten().is_none() {
        wait_child(&mut proc, 30.0)
    } else {
        proc.try_wait().ok().flatten().unwrap().code().unwrap_or(-1)
    };
    assert!(second);
    assert_eq!(code, 0);
}

// ---------- the sweep against synthetic process tables ----------

// The three live process table checks below first prove that a
// spawned sleeper appears under /proc before staging the synthetic
// table, which is a Linux /proc property.
#[cfg(target_os = "linux")]
fn spawn_sleeper(planted: &Path) -> Child {
    Command::new(&*PY)
        .args(["-c", "import time; time.sleep(60)"])
        .arg(planted.as_os_str())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn plant_sweepable(root: &Path, run_id: &str) -> PathBuf {
    let planted = root.join(format!("run-{}", run_id));
    std::fs::create_dir_all(&planted).unwrap();
    std::fs::write(planted.join(".mayfly-provenance"), format!("{}\n", run_id)).unwrap();
    age_directory(&planted, 60.0);
    planted
}

enum FakeCmdline {
    Bytes(Vec<u8>),
    Directory,
    #[cfg(target_os = "linux")]
    Dangling,
}

fn fake_proc_table(base: &Path, entries: &[(&str, FakeCmdline)]) -> PathBuf {
    let proc_dir = base.to_path_buf();
    for (pid, kind) in entries {
        let entry = proc_dir.join(pid);
        std::fs::create_dir_all(&entry).unwrap();
        match kind {
            FakeCmdline::Bytes(bytes) => {
                std::fs::write(entry.join("cmdline"), bytes).unwrap();
            }
            FakeCmdline::Directory => {
                std::fs::create_dir_all(entry.join("cmdline")).unwrap();
            }
            #[cfg(target_os = "linux")]
            FakeCmdline::Dangling => {
                std::os::unix::fs::symlink("/nonexistent-mayfly-target", entry.join("cmdline"))
                    .unwrap();
            }
        }
    }
    proc_dir
}

#[cfg(target_os = "linux")]
#[test]
fn the_sweep_reads_a_table_it_cannot_read_at_all_as_unknown() {
    let tmp = Tmp::new("proc-unreadable");
    let root_dir = tmp.path().join("root");
    std::fs::create_dir_all(&root_dir).unwrap();
    let planted = plant_sweepable(&root_dir, "eeee00000005");
    let mut sleeper = spawn_sleeper(&planted);
    let found = wait_until(
        || {
            pids_referencing(&planted)
                .map(|pids| pids.contains(&(sleeper.id())))
                .unwrap_or(false)
        },
        15.0,
    );
    assert!(found);
    let fake = fake_proc_table(
        &tmp.path().join("fakeproc-a"),
        &[
            ("111111", FakeCmdline::Directory),
            ("222222", FakeCmdline::Directory),
        ],
    );
    assert_eq!(pids_referencing_in(&fake, &planted), None);
    sweep_orphans_in(&root_dir, &fake);
    assert!(planted.exists());
    let _ = sleeper.kill();
    let _ = sleeper.wait();
}

#[cfg(target_os = "linux")]
#[test]
fn the_sweep_reads_a_refused_cmdline_as_unknown_liveness() {
    let tmp = Tmp::new("proc-refused");
    let root_dir = tmp.path().join("root");
    std::fs::create_dir_all(&root_dir).unwrap();
    let planted = plant_sweepable(&root_dir, "eeee00000006");
    let mut sleeper = spawn_sleeper(&planted);
    let found = wait_until(
        || {
            pids_referencing(&planted)
                .map(|pids| pids.contains(&(sleeper.id())))
                .unwrap_or(false)
        },
        15.0,
    );
    assert!(found);
    let fake = fake_proc_table(
        &tmp.path().join("fakeproc-b"),
        &[
            (
                "424242",
                FakeCmdline::Bytes(planted.as_os_str().as_bytes().to_vec()),
            ),
            ("424243", FakeCmdline::Directory),
        ],
    );
    assert_eq!(pids_referencing_in(&fake, &planted), None);
    sweep_orphans_in(&root_dir, &fake);
    assert!(planted.exists());
    let _ = sleeper.kill();
    let _ = sleeper.wait();
}

#[cfg(target_os = "linux")]
#[test]
fn a_vanished_process_does_not_make_liveness_unknown() {
    let tmp = Tmp::new("proc-vanished");
    let root_dir = tmp.path().join("root");
    std::fs::create_dir_all(&root_dir).unwrap();
    let planted = plant_sweepable(&root_dir, "eeee00000007");
    let mut sleeper = spawn_sleeper(&planted);
    let found = wait_until(
        || {
            pids_referencing(&planted)
                .map(|pids| pids.contains(&(sleeper.id())))
                .unwrap_or(false)
        },
        15.0,
    );
    assert!(found);
    let fake = fake_proc_table(
        &tmp.path().join("fakeproc-c"),
        &[
            (
                "424242",
                FakeCmdline::Bytes(planted.as_os_str().as_bytes().to_vec()),
            ),
            ("424243", FakeCmdline::Dangling),
        ],
    );
    let found = pids_referencing_in(&fake, &planted);
    assert_eq!(found, Some(vec![424242]));
    let _ = sleeper.kill();
    let _ = sleeper.wait();
}

#[test]
fn a_refused_cmdline_keeps_every_candidate_of_the_sweep() {
    // The poisoned table rule is a property of the whole
    // sweep, not of one candidate: where one command line
    // read is refused, liveness is unknown, and no candidate
    // of the sweep is removed on the strength of the readable
    // remainder. A form that read the table once and treated
    // the refused read as an empty table would remove the
    // second candidate here on an answer the table never
    // gave.
    let tmp = Tmp::new("sweep-poison-two");
    let root_dir = tmp.path().join("root");
    std::fs::create_dir_all(&root_dir).unwrap();
    let first = plant_sweepable(&root_dir, "eeee00000008");
    let second = plant_sweepable(&root_dir, "eeee00000009");
    let fake = fake_proc_table(
        &tmp.path().join("fakeproc-poison-two"),
        &[
            (
                "424242",
                FakeCmdline::Bytes(first.as_os_str().as_bytes().to_vec()),
            ),
            ("424243", FakeCmdline::Directory),
        ],
    );
    sweep_orphans_in(&root_dir, &fake);
    assert!(first.exists());
    assert!(second.exists());
}

#[test]
fn one_sweep_answers_each_candidate_from_the_one_table() {
    // Two candidates stand under one table that names only
    // the first. The referenced one stands and the other is
    // swept, so the sweep's answers are per candidate even
    // where the table behind them is read once: a form that
    // answered every candidate with the first candidate's
    // answer would leave the second standing here.
    let tmp = Tmp::new("sweep-two-candidates");
    let root_dir = tmp.path().join("root");
    std::fs::create_dir_all(&root_dir).unwrap();
    let referenced = plant_sweepable(&root_dir, "eeee0000000a");
    let unreferenced = plant_sweepable(&root_dir, "eeee0000000b");
    let fake = fake_proc_table(
        &tmp.path().join("fakeproc-two"),
        &[
            (
                "424242",
                FakeCmdline::Bytes(referenced.as_os_str().as_bytes().to_vec()),
            ),
            (
                "424243",
                FakeCmdline::Bytes(b"mayfly-test-unrelated".to_vec()),
            ),
        ],
    );
    sweep_orphans_in(&root_dir, &fake);
    assert!(referenced.exists());
    assert!(!unreferenced.exists());
}

#[test]
fn the_sweep_keeps_each_candidate_inside_its_grace_period() {
    // MF-12 states the sweep's terms as a conjunction, and one
    // term is that a candidate is swept only when it is older
    // than the grace period. Every other sweep pin ages its
    // candidates past the grace before it drives the sweep, so
    // the age term is the one term no pin discriminated: a
    // sweep that dropped both age returns would judge a
    // directory created a moment ago, in the segment between
    // its creation and its janitor's spawn where MF-12 names
    // no other protection, and every standing pin would still
    // pass. Four candidates stand here against a table that
    // reads clean and names nothing: a young run directory
    // whose provenance names it, that shape aged past the
    // grace, a young clean marker whose run directory is gone,
    // and that marker aged. The young candidates stand and
    // the aged ones are swept, so this pin fails on a sweep
    // missing its age term on the young assertions and on a
    // sweep that removes nothing on the aged ones.
    let tmp = Tmp::new("sweep-grace");
    let root_dir = tmp.path().join("root");
    std::fs::create_dir_all(&root_dir).unwrap();
    let young_dir = root_dir.join("run-ffff00000001");
    std::fs::create_dir_all(&young_dir).unwrap();
    std::fs::write(young_dir.join(".mayfly-provenance"), "ffff00000001\n").unwrap();
    let aged_dir = plant_sweepable(&root_dir, "ffff00000002");
    let young_marker = root_dir.join(".mayfly-clean-ffff00000003");
    std::fs::write(&young_marker, format!("{}\n", "ab".repeat(16))).unwrap();
    let aged_marker = root_dir.join(".mayfly-clean-ffff00000004");
    std::fs::write(&aged_marker, format!("{}\n", "ab".repeat(16))).unwrap();
    age_directory(&aged_marker, 60.0);
    let fake = fake_proc_table(
        &tmp.path().join("fakeproc-grace"),
        &[(
            "424242",
            FakeCmdline::Bytes(b"mayfly-test-unrelated".to_vec()),
        )],
    );
    sweep_orphans_in(&root_dir, &fake);
    assert!(young_dir.exists());
    assert!(!aged_dir.exists());
    assert!(young_marker.exists());
    assert!(!aged_marker.exists());
}

#[test]
fn the_sweep_keeps_a_clean_marker_whose_run_directory_still_stands() {
    // MF-A80. sweep_orphans_in's doc states that a clean
    // marker whose run directory is already gone is swept on
    // the same terms, which makes the marker arm's first term
    // the absence of the run directory: where the directory
    // still stands, the marker is not the sweep's to take,
    // whatever its age. MF-A69's grace pin stages its clean
    // markers with their run directories gone, the one shape
    // that term passes trivially, so a sweep missing the
    // absence return would remove an aged marker beside its
    // standing directory and every pin would still pass. The
    // directory here stands young, inside its grace period,
    // so the directory arm returns on grace and cannot take
    // the marker with it; the control marker, aged the same
    // with its run directory gone, is swept, so the pin also
    // fails on a sweep that removes nothing.
    let tmp = Tmp::new("sweep-marker-standing-dir");
    let root_dir = tmp.path().join("root");
    std::fs::create_dir_all(&root_dir).unwrap();
    let standing_dir = root_dir.join("run-ffff00000005");
    std::fs::create_dir_all(&standing_dir).unwrap();
    std::fs::write(standing_dir.join(".mayfly-provenance"), "ffff00000005\n").unwrap();
    let kept_marker = root_dir.join(".mayfly-clean-ffff00000005");
    std::fs::write(&kept_marker, format!("{}\n", "ab".repeat(16))).unwrap();
    age_directory(&kept_marker, 60.0);
    let control_marker = root_dir.join(".mayfly-clean-ffff00000006");
    std::fs::write(&control_marker, format!("{}\n", "ab".repeat(16))).unwrap();
    age_directory(&control_marker, 60.0);
    let fake = fake_proc_table(
        &tmp.path().join("fakeproc-standing"),
        &[(
            "424242",
            FakeCmdline::Bytes(b"mayfly-test-unrelated".to_vec()),
        )],
    );
    sweep_orphans_in(&root_dir, &fake);
    assert!(standing_dir.exists());
    assert!(kept_marker.exists());
    assert!(!control_marker.exists());
}

#[test]
fn the_sweep_takes_an_aged_clean_marker_whose_run_directory_position_holds_a_link() {
    // MF-P32. The port amendment states the sweep judges a
    // run directory position by the entry standing there and
    // never through a link, and the directory arm judges the
    // position that way, by the entry's own metadata. The
    // marker arm's first term took the run directory's
    // absence from exists(), which follows a link standing
    // at the position, so an aged clean marker whose
    // position holds a link naming a standing directory was
    // kept for exactly as long as the planted target stood,
    // and no later sweep collected it either. The standing
    // pin beside this one stages a real directory at the
    // position, the one shape where the two forms agree, so
    // no pin divided them. Here the position holds a link,
    // and the marker is swept with its aged control, while
    // the link and the directory it names stand byte for
    // byte as planted. The pin fails on the exists() form,
    // which keeps the first marker, and the control fails on
    // a sweep that removes nothing.
    let tmp = Tmp::new("sweep-marker-link-dir");
    let root_dir = tmp.path().join("root");
    std::fs::create_dir_all(&root_dir).unwrap();
    let target_dir = tmp.path().join("linked-target");
    std::fs::create_dir_all(&target_dir).unwrap();
    std::fs::write(target_dir.join("held.txt"), "held\n").unwrap();
    let link = root_dir.join("run-ffff00000007");
    std::os::unix::fs::symlink(&target_dir, &link).unwrap();
    let swept_marker = root_dir.join(".mayfly-clean-ffff00000007");
    std::fs::write(&swept_marker, format!("{}\n", "ab".repeat(16))).unwrap();
    age_directory(&swept_marker, 60.0);
    let control_marker = root_dir.join(".mayfly-clean-ffff00000008");
    std::fs::write(&control_marker, format!("{}\n", "ab".repeat(16))).unwrap();
    age_directory(&control_marker, 60.0);
    let fake = fake_proc_table(
        &tmp.path().join("fakeproc-marker-link"),
        &[(
            "424242",
            FakeCmdline::Bytes(b"mayfly-test-unrelated".to_vec()),
        )],
    );
    sweep_orphans_in(&root_dir, &fake);
    assert!(!swept_marker.exists());
    assert!(!control_marker.exists());
    let meta = std::fs::symlink_metadata(&link).unwrap();
    assert!(meta.file_type().is_symlink());
    assert_eq!(std::fs::read_link(&link).unwrap(), target_dir);
    assert_eq!(
        std::fs::read_to_string(target_dir.join("held.txt")).unwrap(),
        "held\n"
    );
}

// ---------- the canonical spelling of the root ----------

#[test]
fn the_sweep_judges_a_candidate_under_the_canonical_spelling_of_its_root() {
    // The sweep's liveness judgement is a byte comparison,
    // and the two sides of it are built under one spelling
    // of the root. A candidate stands under a root whose
    // canonical spelling the planted command line names,
    // and the sweep is driven through a symlinked spelling
    // of that same root. A sweep that built its candidates
    // from the spelling it was handed would test the link's
    // bytes against the canonical bytes the command line
    // carries, see no naming, and remove a directory whose
    // run the table names as live. The second candidate,
    // which no command line names, is the control: it is
    // swept under either spelling, so the pin also fails on
    // a sweep that removes nothing.
    let tmp = Tmp::new("sweep-spelling");
    let real_root = tmp.path().join("real-root");
    std::fs::create_dir_all(&real_root).unwrap();
    let canonical = mayfly::canonical_root(&real_root);
    let named = plant_sweepable(&canonical, "eeee0000000c");
    let unnamed = plant_sweepable(&canonical, "eeee0000000d");
    let link_root = tmp.path().join("link-root");
    std::os::unix::fs::symlink(&real_root, &link_root).unwrap();
    let fake = fake_proc_table(
        &tmp.path().join("fakeproc-spelling"),
        &[(
            "424242",
            FakeCmdline::Bytes(named.as_os_str().as_bytes().to_vec()),
        )],
    );
    sweep_orphans_in(&link_root, &fake);
    assert!(named.exists());
    assert!(!unnamed.exists());
}

#[test]
fn a_run_under_a_symlinked_root_spelling_reports_the_canonical_sandbox_dir() {
    // The runner builds its sandbox path from the canonical
    // form of the root it was given, so every process the
    // run spawns names the one spelling the sweep judges
    // under. A run handed its root through a symlink must
    // therefore report its sandbox directory under the
    // canonical spelling, not the link's. Against a runner
    // that built from the given spelling the report names
    // the link's path and this pin fails on the comparison.
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("canonical-root-run");
    let real_root = tmp.path().join("real-root");
    std::fs::create_dir_all(&real_root).unwrap();
    let link_root = tmp.path().join("link-root");
    std::os::unix::fs::symlink(&real_root, &link_root).unwrap();
    let report = run_task(py_cmd("pass"), &link_root);
    assert_eq!(report.outcome, Outcome::Task);
    let canonical = mayfly::canonical_root(&real_root);
    assert_eq!(
        report.sandbox_dir,
        format!("{}/run-{}", canonical.display(), report.run_id)
    );
    assert_eq!(report.sandbox_removed, Some(true));
}

#[test]
fn canonical_root_resolves_the_standing_prefix_and_keeps_the_missing_tail() {
    // The canonical form is taken from the deepest prefix
    // that stands, with the tail that does not stand yet
    // joined verbatim behind it, so a root under parents
    // the runner is about to create still resolves to the
    // one spelling. A form that canonicalized the whole
    // root or nothing would fail the first comparison on a
    // root whose tail is missing, and a form that left a
    // symlinked prefix unresolved would fail the second.
    let tmp = Tmp::new("canonical-helper");
    let real = tmp.path().join("real");
    std::fs::create_dir_all(&real).unwrap();
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let via_link = link.join("child").join("grandchild");
    let expected = mayfly::canonical_root(&real)
        .join("child")
        .join("grandchild");
    assert_eq!(mayfly::canonical_root(&via_link), expected);
    assert_eq!(mayfly::canonical_root(&link), mayfly::canonical_root(&real));
    // A parent reference over a component that does not
    // stand cancels that component, because the failed
    // canonicalizations on the climb prove it is not
    // there for the kernel to resolve through, and a
    // current directory reference names the directory it
    // stands in. A form that fell back to the given
    // spelling at either shape would hand the runner and
    // the sweep different spellings of this root once the
    // missing component's neighbours exist.
    let through_missing = real.join("missing").join("..").join("child");
    assert_eq!(
        mayfly::canonical_root(&through_missing),
        mayfly::canonical_root(&real).join("child")
    );
    let through_dot = real.join(".").join("child");
    assert_eq!(
        mayfly::canonical_root(&through_dot),
        mayfly::canonical_root(&real).join("child")
    );
}

#[test]
fn a_run_under_a_root_spelled_through_a_missing_parent_reports_the_canonical_sandbox_dir() {
    // The runner's half of the same shape at the run
    // level. The root reaches the runner spelled through
    // a component that does not stand, cancelled by a
    // parent reference behind it. The sandbox directory
    // the run reports must stand under the canonical
    // spelling the sweep will judge under once the run's
    // own creation has made the root's neighbours stand,
    // because a later sweep canonicalizes the root it is
    // handed and a report, a janitor, and an init naming
    // the spelled form would name a directory that
    // sweep's candidates never carry. Against a runner
    // that kept the given spelling for this root the
    // report names the spelled path and this pin fails
    // on the comparison.
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("canonical-root-dotdot");
    let base = tmp.path().join("base");
    std::fs::create_dir_all(&base).unwrap();
    let spelled = base.join("missing").join("..").join("runs");
    let report = run_task(py_cmd("pass"), &spelled);
    assert_eq!(report.outcome, Outcome::Task);
    let canonical = mayfly::canonical_root(&base).join("runs");
    assert_eq!(
        report.sandbox_dir,
        format!("{}/run-{}", canonical.display(), report.run_id)
    );
    assert_eq!(report.sandbox_removed, Some(true));
}

// ---------- the fake unshare interrupt protocol ----------

const FAKE_UNSHARE: &str = r#"#!/usr/bin/env python3
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
    # Python time.monotonic reads CLOCK_UPTIME_RAW on macOS, whereas
    # mayfly::now reads CLOCK_MONOTONIC on every Unix host.
    payload.setdefault("ts", time.clock_gettime(time.CLOCK_MONOTONIC))
    os.write(fd, (json.dumps(payload) + "\n").encode())


def note(name):
    with open(os.path.join(sandbox, name), "w") as handle:
        handle.write("noted\n")


emit({"event": "ready", "proc_mounted": mode == "proc-mounted"})
emit({"event": "started"})
note("fake-started")
if mode == "late-done":
    pid = os.fork()
    if pid == 0:
        time.sleep(1.2)
        emit({"event": "task_done", "exit_code": 0})
        os._exit(0)
    time.sleep(60)
elif mode == "status-held":
    emit({"event": "task_done", "exit_code": 0})
    pid = os.fork()
    if pid == 0:
        os.close(1)
        os.close(2)
        time.sleep(8)
        os._exit(0)
    os._exit(0)
else:
    emit({"event": "task_done", "exit_code": 0})
    note("fake-done")
    time.sleep(60)
"#;

fn fake_unshare_text() -> String {
    // The fake unshare script with this interpreter in its
    // shebang. The backend scrubs PATH out of the environment
    // the fake runs with, so a shebang that searches for the
    // interpreter by name depends on the host keeping one on
    // the default search path. The sibling helpers run their
    // scripts through this interpreter directly, and the fake
    // names it outright for the same reason.
    FAKE_UNSHARE.replacen("#!/usr/bin/env python3", &format!("#!{}", PY.display()), 1)
}

fn write_fake_unshare(fakebin: &Path) -> PathBuf {
    std::fs::create_dir_all(fakebin).unwrap();
    let fake = fakebin.join("unshare");
    std::fs::write(&fake, fake_unshare_text()).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    fake
}

fn start_cli_with_fake_unshare(root: &Path, fakebin: &Path, mode: &str) -> Child {
    write_fake_unshare(fakebin);
    let path_var = format!(
        "{}{}{}",
        fakebin.display(),
        ":",
        std::env::var("PATH").unwrap_or_default()
    );
    Command::new(mayfly_bin())
        .args([
            os("run"),
            os("--root"),
            root.as_os_str().to_os_string(),
            os("--allow-env"),
            os("MAYFLY_FAKE_MODE"),
            os("--"),
            py(),
            os("-c"),
            os("pass"),
        ])
        .env("PATH", path_var)
        .env("MAYFLY_FAKE_MODE", mode)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

#[test]
fn the_fake_unshare_names_this_interpreter_in_its_shebang() {
    let tmp = Tmp::new("fake-shebang");
    let fake = write_fake_unshare(&tmp.path().join("fakebin"));
    let text = std::fs::read_to_string(&fake).unwrap();
    let first_line = text.lines().next().unwrap();
    assert_eq!(first_line, format!("#!{}", PY.display()));
}

#[test]
fn an_interrupt_before_the_task_done_stamp_is_a_harness_error() {
    let tmp = Tmp::new("fake-late-done");
    let root_dir = tmp.path().join("root");
    std::fs::create_dir_all(&root_dir).unwrap();
    let mut proc = start_cli_with_fake_unshare(&root_dir, &tmp.path().join("fakebin"), "late-done");
    let noted = wait_until(
        || {
            glob_run_dirs(&root_dir)
                .iter()
                .any(|dir| dir.join("fake-started").exists())
        },
        30.0,
    );
    assert!(noted, "the fake sandbox never reported its start");
    signal_child(&proc, libc::SIGTERM);
    let (code, out) = communicate(&mut proc, 30.0);
    assert_eq!(code, 125);
    let payload: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(payload["outcome"], "harness_error");
    assert_eq!(payload["task_exit_code"], serde_json::Value::Null);
    assert!(
        payload["error"]
            .as_str()
            .unwrap()
            .contains("interrupted by signal 15"),
        "{}",
        payload["error"]
    );
    assert!(glob_run_dirs(&root_dir).is_empty());
    assert!(glob_clean_markers(&root_dir).is_empty());
}

#[test]
fn an_interrupt_after_the_task_done_stamp_keeps_the_task_outcome() {
    let tmp = Tmp::new("fake-done-early");
    let root_dir = tmp.path().join("root");
    std::fs::create_dir_all(&root_dir).unwrap();
    let mut proc =
        start_cli_with_fake_unshare(&root_dir, &tmp.path().join("fakebin"), "done-early");
    let noted = wait_until(
        || {
            glob_run_dirs(&root_dir)
                .iter()
                .any(|dir| dir.join("fake-done").exists())
        },
        30.0,
    );
    assert!(noted, "the fake sandbox never reported its task done");
    signal_child(&proc, libc::SIGTERM);
    let (code, out) = communicate(&mut proc, 30.0);
    assert_eq!(code, 0);
    let payload: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(payload["outcome"], "task");
    assert_eq!(payload["task_exit_code"], 0);
    assert!(glob_run_dirs(&root_dir).is_empty());
    assert!(glob_clean_markers(&root_dir).is_empty());
}

#[test]
fn the_report_carries_the_proc_mounted_answer_of_the_ready_event() {
    // The report records whether a fresh proc was mounted
    // in the prepared root, and the runner takes that answer
    // from the ready event the init reports. On the hosts
    // the fleet gates on the mount stands refused inside a
    // user namespace, so no real run can stage the true arm,
    // and the fake unshare is the one seam that can stage
    // the event's value at will: its proc-mounted mode
    // reports proc_mounted true on its ready event and
    // takes the standing done shape otherwise. This pin
    // drives the command line through the fake in that mode
    // and in the standing done-early mode and reads the
    // printed report's field in both directions. Against a
    // runner that ignores the ready event's field and lets
    // the backstop in run answer everywhere, the true arm
    // fails, and against one that inverts the field both
    // arms fail.
    let tmp = Tmp::new("fake-proc-mounted");
    for (mode, expected) in [("proc-mounted", true), ("done-early", false)] {
        let root_dir = tmp.path().join(format!("root-{mode}"));
        std::fs::create_dir_all(&root_dir).unwrap();
        let fakebin = tmp.path().join(format!("fakebin-{mode}"));
        let mut proc = start_cli_with_fake_unshare(&root_dir, &fakebin, mode);
        let noted = wait_until(
            || {
                glob_run_dirs(&root_dir)
                    .iter()
                    .any(|dir| dir.join("fake-done").exists())
            },
            30.0,
        );
        assert!(noted, "the fake sandbox never reported its task done");
        signal_child(&proc, libc::SIGTERM);
        let (code, out) = communicate(&mut proc, 30.0);
        assert_eq!(code, 0, "{}", mode);
        let payload: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(payload["outcome"], "task", "{}", mode);
        assert_eq!(payload["proc_mounted"], expected, "{}", mode);
        assert!(glob_run_dirs(&root_dir).is_empty());
        assert!(glob_clean_markers(&root_dir).is_empty());
    }
}

#[test]
fn a_held_status_pipe_costs_the_run_no_descriptor() {
    // A fake unshare whose own process exits at once while a
    // forked child holds the status pipe open for eight
    // seconds, its copies of the output pipes closed first.
    // The status drain cannot end until the child lets go, so
    // the runner's first drain wait times out at five seconds
    // with the drain thread still blocked on the descriptor it
    // owns inside its File. The files this test opens across
    // that window take the descriptor numbers the process has
    // free. A runner that closes the drain's descriptor under
    // its thread frees the number at the timeout, one of the
    // files below takes it, and the File's own close when the
    // drain ends closes that file under the test, which the
    // closing assertions read. A runner that leaves the
    // descriptor to its one owner closes nothing of the
    // test's, and every sentinel stands open at the end.
    ensure_helper();
    let tmp = Tmp::new("status-held");
    let fake = write_fake_unshare(&tmp.path().join("fakebin"));
    let root_dir = tmp.path().join("root");
    std::fs::create_dir_all(&root_dir).unwrap();
    let backend = NamespaceBackend::with_unshare_path(Some(fake));
    let mut task_spec = spec(py_cmd("pass"), &root_dir);
    task_spec.allow_env = vec![os("MAYFLY_FAKE_MODE")];
    task_spec.caller_env = vec![(os("MAYFLY_FAKE_MODE"), os("status-held"))];
    let (tx, rx) = std::sync::mpsc::channel();
    let began = Instant::now();
    std::thread::spawn(move || {
        let report = backend.run(&task_spec);
        let _ = tx.send(report);
    });
    std::thread::sleep(Duration::from_millis(5500));
    let mut sentinels: Vec<std::fs::File> = Vec::new();
    for _ in 0..9 {
        sentinels.push(std::fs::File::open(tmp.path()).unwrap());
        std::thread::sleep(Duration::from_millis(250));
    }
    let report = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the run did not finish behind its held status pipe");
    let elapsed = began.elapsed();
    assert_eq!(report.outcome, Outcome::Task);
    assert_eq!(report.task_exit_code, Some(0));
    // The held pipe is what makes this pin a pin. A run that
    // finished before the drain wait could time out never
    // stood in the window at all, and the sentinel assertions
    // below would hold vacuously.
    assert!(
        elapsed >= Duration::from_secs(6),
        "the run finished in {:?} without the held pipe engaging",
        elapsed
    );
    for sentinel in &sentinels {
        use std::os::unix::io::AsRawFd;
        let rc = unsafe { libc::fcntl(sentinel.as_raw_fd(), libc::F_GETFD) };
        assert!(rc != -1, "a sentinel descriptor was closed under the test");
    }
}

const FAKE_UNSHARE_NOFORK: &str = r#"#!/usr/bin/env python3
import os
import sys

argv = sys.argv[1:]
index = argv.index("__init")
argv[index - 1] = "{SHIM}"
os.execve("{UNSHARE}", ["{UNSHARE}"] + argv, os.environ)
"#;

const INIT_SHIM_NOFORK: &str = r#"#!/usr/bin/env python3
import os
import resource
import sys

limits = resource.getrlimit(resource.RLIMIT_NPROC)
resource.setrlimit(resource.RLIMIT_NPROC, (0, limits[1]))
os.execve("{HELPER}", ["{HELPER}"] + sys.argv[1:], os.environ)
"#;

fn write_fake_unshare_nofork(fakebin: &Path) -> PathBuf {
    std::fs::create_dir_all(fakebin).unwrap();
    let shim = fakebin.join("init-shim");
    let shim_text = INIT_SHIM_NOFORK
        .replacen("#!/usr/bin/env python3", &format!("#!{}", PY.display()), 1)
        .replace("{HELPER}", &mayfly_bin().display().to_string());
    std::fs::write(&shim, shim_text).unwrap();
    let fake_text = FAKE_UNSHARE_NOFORK
        .replacen("#!/usr/bin/env python3", &format!("#!{}", PY.display()), 1)
        .replace("{SHIM}", &shim.display().to_string())
        .replace("{UNSHARE}", &unshare_path().display().to_string());
    let fake = fakebin.join("unshare");
    std::fs::write(&fake, fake_text).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    fake
}

#[test]
fn an_init_whose_fork_fails_reports_the_errno_it_met() {
    // The fake unshare execs the real unshare with a shim
    // standing where the init stands. The shim sets the
    // process count limit to zero in its own process and
    // execs the real init, so the init assembles its root
    // and then meets a fork that fails with EAGAIN at the
    // entry of supervise. The report must carry that errno
    // in the start error words. Against an init that
    // returns from that seat in silence, the report carries
    // the runner's generic fallback instead, and this pin
    // fails on the words.
    if !require_sandbox() {
        return;
    }
    ensure_helper();
    let tmp = Tmp::new("init-fork-fails");
    let fake = write_fake_unshare_nofork(&tmp.path().join("fakebin"));
    let root_dir = tmp.path().join("root");
    std::fs::create_dir_all(&root_dir).unwrap();
    let backend = NamespaceBackend::with_unshare_path(Some(fake));
    let report = backend.run(&spec(py_cmd("pass"), &root_dir));
    assert_eq!(report.outcome, Outcome::HarnessError);
    assert_eq!(
        report.error.as_deref(),
        Some("could not start the task, could not fork the task process, 11 Resource temporarily unavailable")
    );
    assert_eq!(report.task_exit_code, None);
    assert!(glob_run_dirs(&root_dir).is_empty());
}

#[test]
fn a_slow_janitor_on_the_clean_path_is_reaped_by_a_detached_waiter() {
    // A janitor kept at its cleanup past the runner's ten
    // second stand down must still be reaped. This stages one
    // by planting a marker the janitor cannot authenticate,
    // so it takes its cleanup path, and a file at the sandbox
    // path in the moment the runner's teardown removes the
    // directory, which the janitor's removal cannot take
    // away, so its retry loop holds it for the full thirty
    // seconds, long past the stand down. The run still ends
    // clean, because the teardown removed the real sandbox
    // before the file was planted. An attempt counts only
    // where the report stands clean and the janitor is still
    // alive when the run returns, and up to three attempts
    // run, because the plant races the janitor's first look
    // and a lost race hands the janitor its five second watch
    // instead. On a counted attempt the janitor is this
    // process's child, and when it finally exits a reaped
    // child leaves no entry in the process table, while a
    // dropped one stands there as a zombie.
    if !require_sandbox() {
        return;
    }
    ensure_helper();
    for attempt in 0..3 {
        let tmp = Tmp::new("slow-janitor");
        let root_dir = tmp.path().join("root");
        std::fs::create_dir_all(&root_dir).unwrap();
        let run_id = format!("a5a5a5a5a5{:02}", attempt);
        let sandbox = root_dir.join(format!("run-{}", run_id));
        let marker = root_dir.join(format!(".mayfly-clean-{}", run_id));
        std::os::unix::fs::symlink(root_dir.join("no-such-target"), &marker).unwrap();
        let planted = Arc::new(AtomicBool::new(false));
        let planter = {
            let sandbox = sandbox.clone();
            let planted = Arc::clone(&planted);
            std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(60);
                while Instant::now() < deadline && !sandbox.exists() {
                    std::thread::sleep(Duration::from_millis(1));
                }
                while Instant::now() < deadline && sandbox.exists() {
                    std::thread::sleep(Duration::from_millis(1));
                }
                if Instant::now() < deadline {
                    let _ = std::fs::write(&sandbox, "planted in the teardown's place");
                    planted.store(true, Ordering::SeqCst);
                }
            })
        };
        let mut backend = NamespaceBackend::new();
        backend.run_id_override = Some(run_id);
        let report = backend.run(&spec(py_cmd("pass"), &root_dir));
        let _ = planter.join();
        if report.outcome != Outcome::Task || report.sandbox_removed != Some(true) {
            continue;
        }
        if !planted.load(Ordering::SeqCst) {
            continue;
        }
        let janitors = pids_running_helper("__janitor", Some(sandbox.as_os_str().as_bytes()));
        if janitors.len() != 1 {
            continue;
        }
        let janitor_pid = janitors[0];
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", janitor_pid)).unwrap();
        let fields: Vec<&str> = stat
            .rsplit(')')
            .next()
            .unwrap()
            .split_whitespace()
            .collect();
        assert_eq!(fields[1].parse::<u32>().unwrap(), std::process::id());
        let deadline = Instant::now() + Duration::from_secs(45);
        let mut zombie_since: Option<Instant> = None;
        let mut reaped = false;
        while Instant::now() < deadline {
            match std::fs::read_to_string(format!("/proc/{}/stat", janitor_pid)) {
                Ok(stat) => {
                    let state = stat
                        .rsplit(')')
                        .next()
                        .unwrap()
                        .split_whitespace()
                        .next()
                        .unwrap_or("");
                    if state == "Z" {
                        let since = zombie_since.get_or_insert_with(Instant::now);
                        assert!(
                            since.elapsed() < Duration::from_secs(3),
                            "the janitor stands as a zombie child of this process"
                        );
                    } else {
                        zombie_since = None;
                    }
                }
                Err(_) => {
                    reaped = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        assert!(reaped, "the janitor's process table entry never cleared");
        return;
    }
    panic!("no attempt staged a slow janitor, so the reaping pin did not engage");
}

// ---------- commands no exec can carry ----------

#[test]
fn a_command_with_a_none_element_is_a_harness_error() {
    // A Python command list can carry a none element, and the
    // harness refused it at spawn. A Rust argument list cannot
    // carry none, and the element no exec can carry here is
    // one holding a NUL byte, which the harness refuses at
    // the same place with the same report.
    let tmp = Tmp::new("none-element");
    let bad = OsString::from_vec(b"bad\0arg".to_vec());
    // On hosts without a native unshare binary on PATH, supply the
    // fake unshare path so the run reaches the embedded NUL check in
    // run_supervised rather than stopping early on missing unshare.
    ensure_helper();
    let backend = match NamespaceBackend::new().unshare_path {
        Some(path) => NamespaceBackend::with_unshare_path(Some(path)),
        None => NamespaceBackend::with_unshare_path(Some(write_fake_unshare(
            &tmp.path().join("fakebin"),
        ))),
    };
    let report = backend.run(&spec(vec![py(), bad], tmp.path()));
    assert_eq!(report.outcome, Outcome::HarnessError);
    assert_eq!(report.task_exit_code, None);
    assert_eq!(report.task_signal, None);
    assert!(report.error.is_some());
    assert!(
        report
            .error
            .as_deref()
            .unwrap_or("")
            .contains("could not launch unshare"),
        "{:?}",
        report.error
    );
    assert_eq!(report.sandbox_removed, Some(true));
    assert!(glob_run_dirs(tmp.path()).is_empty());
}

#[test]
fn a_nul_byte_in_an_allowlisted_environment_value_is_a_harness_error() {
    // The environment the runner hands the spawn is built
    // from pairs a library caller supplies, and a value
    // carrying a NUL byte can never reach an exec, so the
    // harness refuses it at the same place and in the same
    // words as an argument carrying the byte.
    let tmp = Tmp::new("env-nul-value");
    ensure_helper();
    let backend = match NamespaceBackend::new().unshare_path {
        Some(path) => NamespaceBackend::with_unshare_path(Some(path)),
        None => NamespaceBackend::with_unshare_path(Some(write_fake_unshare(
            &tmp.path().join("fakebin"),
        ))),
    };
    let mut task_spec = spec(vec![py()], tmp.path());
    task_spec.allow_env = vec![os("MAYFLY_KEEP_ME")];
    task_spec.caller_env = vec![(
        os("MAYFLY_KEEP_ME"),
        OsString::from_vec(b"a kept\0value".to_vec()),
    )];
    let report = backend.run(&task_spec);
    assert_eq!(report.outcome, Outcome::HarnessError);
    assert_eq!(report.task_exit_code, None);
    assert_eq!(report.task_signal, None);
    assert_eq!(
        report.error.as_deref(),
        Some("could not launch unshare, embedded null byte")
    );
    assert_eq!(report.sandbox_removed, Some(true));
    assert!(glob_run_dirs(tmp.path()).is_empty());
}

#[test]
fn cli_reports_a_none_command_element_as_a_harness_error() {
    // The command list is handed to the command line entry
    // the way a caller of the library builds it, because
    // shell arguments cannot carry the element through a
    // process boundary either. On hosts without a native
    // unshare binary on PATH, stage the fake unshare under
    // ENV_LOCK so cli_main reaches the embedded NUL check.
    ensure_helper();
    let tmp = Tmp::new("cli-none-element");
    let bad = OsString::from_vec(b"bad\0arg".to_vec());
    let args = vec![
        os("run"),
        os("--root"),
        tmp.path().as_os_str().to_os_string(),
        os("--"),
        py(),
        bad,
    ];
    let mut out: Vec<u8> = Vec::new();
    let mut err: Vec<u8> = Vec::new();
    let code = if NamespaceBackend::new().unshare_path.is_some() {
        cli_main(&args, &mut out, &mut err)
    } else {
        let _guard = ENV_LOCK.lock().unwrap();
        let fakebin = tmp.path().join("fakebin");
        write_fake_unshare(&fakebin);
        let old_path = std::env::var_os("PATH");
        let mut paths = vec![fakebin];
        if let Some(ref existing) = old_path {
            paths.extend(std::env::split_paths(existing));
        }
        std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
        let result = cli_main(&args, &mut out, &mut err);
        match old_path {
            Some(value) => std::env::set_var("PATH", value),
            None => std::env::remove_var("PATH"),
        }
        result
    };
    let payload: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out)).unwrap();
    assert_eq!(code, HARNESS_EXIT_CODE);
    assert_eq!(payload["outcome"], "harness_error");
    assert_eq!(payload["task_exit_code"], serde_json::Value::Null);
    assert_eq!(payload["task_signal"], serde_json::Value::Null);
    assert!(
        payload["error"]
            .as_str()
            .unwrap_or("")
            .contains("embedded null byte"),
        "{}",
        payload
    );
}

struct ReportSinkFails;

impl Write for ReportSinkFails {
    fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "the report reader is gone",
        ))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn cli_answers_a_harness_error_when_the_report_cannot_be_written() {
    // The report is the run's one product and the exit code
    // mirrors it. A writer that refuses every write stands
    // for a reader that closed the pipe or a target that is
    // gone, and a run whose task exited 0 must then answer
    // 125, because the report its code mirrors was never
    // delivered. Against code that discards the write's
    // result the call returns the task's 0, and this pin
    // fails on that code.
    if !require_sandbox() {
        return;
    }
    ensure_helper();
    let tmp = Tmp::new("cli-report-sink");
    let args = cli_args(tmp.path(), &py_cmd("pass"));
    let mut out = ReportSinkFails;
    let mut err: Vec<u8> = Vec::new();
    let code = cli_main(&args, &mut out, &mut err);
    assert_eq!(code, HARNESS_EXIT_CODE);
}

#[test]
fn cli_treats_an_option_shaped_token_after_a_value_option_as_a_missing_value() {
    // The argparse command line of Round 1 refused an option
    // shaped token where a value belongs, and the Rust parser
    // keeps the refusal. An option followed by another option
    // or by the separator is missing its value, the entry
    // exits 2 with the missing value error, and nothing runs
    // and nothing is prepared anywhere.
    ensure_helper();
    let tmp = Tmp::new("cli-missing-value");
    let shapes: Vec<(Vec<OsString>, &str)> = vec![
        (
            vec![
                os("run"),
                os("--root"),
                os("--net"),
                os("--"),
                py(),
                os("-c"),
                os("pass"),
            ],
            "mayfly run needs a value after --root",
        ),
        (
            vec![
                os("run"),
                os("--root"),
                tmp.path().as_os_str().to_os_string(),
                os("--allow-env"),
                os("--"),
                py(),
                os("-c"),
                os("pass"),
            ],
            "mayfly run needs a value after --allow-env",
        ),
        (
            vec![
                os("run"),
                os("--root"),
                tmp.path().as_os_str().to_os_string(),
                os("--backend"),
                os("--"),
                py(),
                os("-c"),
                os("pass"),
            ],
            "mayfly run needs a value after --backend",
        ),
    ];
    for (args, message) in shapes {
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        let code = cli_main(&args, &mut out, &mut err);
        assert_eq!(code, 2, "{}", String::from_utf8_lossy(&err));
        assert!(
            String::from_utf8_lossy(&err).contains(message),
            "{}",
            String::from_utf8_lossy(&err)
        );
        assert!(out.is_empty());
    }
    assert!(glob_run_dirs(tmp.path()).is_empty());
}

#[test]
fn cli_refuses_the_malformed_call_shapes_at_2() {
    // The missing value refusal is pinned, and the bound
    // refusals are pinned in their own entries. The four
    // dispatch refusals stood in no pin: no arguments and a
    // wrong subcommand answer the usage line alone, an
    // unknown option and a missing root answer their
    // sentence with the usage line, and a missing command
    // answers its sentence alone. The unknown option arm is
    // the load bearing one, because it is what keeps a
    // hyphen led token in option position from falling
    // through to the arm that takes the rest of the line as
    // the command and runs it.
    ensure_helper();
    let tmp = Tmp::new("cli-malformed");
    // Mechanical update for obligation MF-14 adding [--ro-bind HOST:SANDBOX]... to USAGE.
    let usage = "usage: mayfly run --root DIR [--backend NAME] [--allow-env KEY]... [--ro-bind HOST:SANDBOX]... [--net] [--timeout SECONDS] [--max-cpu SECONDS] [--max-memory BYTES] -- COMMAND...\n";
    let root = tmp.path().as_os_str().to_os_string();
    let shapes: Vec<(Vec<OsString>, String)> = vec![
        (vec![], usage.to_string()),
        (vec![os("bogus")], usage.to_string()),
        (
            vec![
                os("run"),
                os("--root"),
                root.clone(),
                os("--bogus"),
                os("--"),
                os("/bin/true"),
            ],
            format!("mayfly run does not know the option --bogus\n{}", usage),
        ),
        (
            vec![os("run"), os("--"), os("/bin/true")],
            format!("mayfly run needs a --root directory\n{}", usage),
        ),
        (
            vec![os("run"), os("--root"), root, os("--")],
            "mayfly run needs a command after --\n".to_string(),
        ),
    ];
    for (args, expected) in shapes {
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        let code = cli_main(&args, &mut out, &mut err);
        assert_eq!(code, 2, "{}", String::from_utf8_lossy(&err));
        assert_eq!(String::from_utf8(err).unwrap(), expected);
        assert!(out.is_empty());
    }
    assert!(dir_names(tmp.path()).is_empty());
}

#[test]
fn cli_refuses_a_bound_option_with_no_value() {
    // MF-A81. MF-A51's record names the missing value pin
    // above as the bound family's missing value coverage,
    // but that pin drives the --root and --allow-env string
    // arms only, and the bound options ride bound_value,
    // whose own absence match no drive reached: a bound_value
    // that defaulted a missing value, or unwrapped it, would
    // pass every pin. Each bound option stands here in two
    // shapes, followed by the separator and standing last in
    // the call, and each refuses at exit 2 with the missing
    // value sentence naming the option, nothing on standard
    // output, and nothing prepared under the root.
    ensure_helper();
    let tmp = Tmp::new("cli-bound-missing-value");
    let mut shapes: Vec<Vec<OsString>> = Vec::new();
    for name in ["--timeout", "--max-cpu", "--max-memory"] {
        shapes.push(vec![
            os("run"),
            os("--root"),
            tmp.path().as_os_str().to_os_string(),
            os(name),
            os("--"),
            os("/bin/echo"),
            os("hi"),
        ]);
        shapes.push(vec![
            os("run"),
            os("--root"),
            tmp.path().as_os_str().to_os_string(),
            os(name),
        ]);
    }
    for args in &shapes {
        let name = args[3].to_string_lossy().into_owned();
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        let code = cli_main(args, &mut out, &mut err);
        assert_eq!(code, 2, "{}", String::from_utf8_lossy(&err));
        assert!(
            String::from_utf8_lossy(&err)
                .contains(&format!("mayfly run needs a value after {}", name)),
            "{}",
            String::from_utf8_lossy(&err)
        );
        assert!(out.is_empty());
    }
    assert!(glob_run_dirs(tmp.path()).is_empty());
}

// ---------- the init driven directly ----------

fn unshare_path() -> PathBuf {
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join("unshare");
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from("unshare")
}

#[test]
fn the_assemble_root_drive_refuses_without_the_internal_token() {
    // The hidden assembly drive performs its mounts in the
    // mount namespace of whatever invokes it, so it refuses
    // to run at all without the internal run token in its
    // environment, and a refusal performs no assembly and
    // creates nothing. Driven directly here, on the host and
    // outside any namespace, with the token key removed from
    // the environment the child inherits.
    let tmp = Tmp::new("assemble-refusal");
    let sandbox = tmp.path().join("run-refuse000001");
    let rootfs = sandbox.join("rootfs");
    let output = Command::new(mayfly_bin())
        .arg("__assemble-root")
        .arg("--rootfs")
        .arg(rootfs.as_os_str())
        .env_remove(mayfly::INTERNAL_TOKEN_ENV)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("refuses to run without the internal run token"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!rootfs.exists());
    assert!(!sandbox.exists());
}

#[test]
fn the_init_drive_refuses_without_the_internal_token() {
    // The hidden init performs the assembly the drive
    // performs and then sets the hostname, pivots the root,
    // and supervises the command, strictly more than the
    // drive does on a direct invocation, and it carries the
    // same token refusal the drive carries, checked before
    // anything is armed or attempted. Driven directly here,
    // on the host and outside any namespace, with the token
    // key removed from the environment the child inherits
    // and the status descriptor standing at the child's own
    // error stream, so a start that got past the refusal
    // would write its failure event where this pin reads.
    // Against the code before the refusal the drive answers
    // the assembly's failure at the harness exit in the
    // prepared root words with no refusal on its error
    // stream, and the pin fails on the exit code, the words,
    // and the event alike.
    let tmp = Tmp::new("init-refusal");
    let sandbox = tmp.path().join("run-refuse000002");
    let rootfs = sandbox.join("rootfs");
    let output = Command::new(mayfly_bin())
        .arg("__init")
        .arg("--rootfs")
        .arg(rootfs.as_os_str())
        .arg("--status-fd")
        .arg("2")
        .arg("--")
        .arg("/bin/true")
        .env_remove(mayfly::INTERNAL_TOKEN_ENV)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("refuses to run without the internal run token"),
        "{}",
        stderr
    );
    assert!(!stderr.contains("the prepared root failed"), "{}", stderr);
    assert!(!rootfs.exists());
    assert!(!sandbox.exists());
}

#[test]
fn the_hidden_refusals_exit_at_1_when_standard_error_cannot_be_written() {
    // The hidden entry points report their refusals on
    // standard error and exit at 1. A print that panics on a
    // failed write would trade that exit for a panic at 101
    // where the error stream itself is broken, so both
    // refusals are driven here with standard error pointed at
    // /dev/full, where every write fails, and the pin reads
    // the exit alone. Against a panicking print both drives
    // die at 101 and the pin fails for that reason. The
    // staging is proved by common::full_device before the
    // drives, and the pin skips where the host's /dev/full
    // is not the failing device.
    let tmp = Tmp::new("refusal-full-stderr");
    let rootfs = tmp.path().join("run-refuse000003").join("rootfs");
    let Some(full) = common::full_device() else {
        return;
    };
    for subcommand in ["__init", "__assemble-root"] {
        let status = Command::new(mayfly_bin())
            .arg(subcommand)
            .arg("--rootfs")
            .arg(rootfs.as_os_str())
            .env_remove(mayfly::INTERNAL_TOKEN_ENV)
            .stdout(Stdio::null())
            .stderr(Stdio::from(full.try_clone().unwrap()))
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(1), "{}", subcommand);
    }
    assert!(!rootfs.exists());
}

#[test]
fn the_hidden_entry_points_refuse_a_token_of_the_wrong_shape() {
    // The hidden entry points refuse to run unless the value
    // under the internal token key has the token shape, thirty
    // two lowercase hexadecimal characters, and that gate is
    // the only thing standing between a direct invocation and
    // the assembly's mounts performed on the host. The standing
    // refusal pins remove the key, so they judge the gate on
    // the key's absence and cannot tell a true shape test from
    // a weakened one. This pin presents values that stand in
    // the key and fail the shape in each of its terms, and
    // each must draw the refusal in the entry point's own
    // words. The malformed drives withhold each entry point's
    // next required argument, the drive its rootfs argument
    // and the init its separator, so a gate weakened enough
    // to admit a value stops at the missing argument, at exit
    // 1 with no refusal in its words, and never reaches the
    // assembly: the pin is safe to run against the weakened
    // forms it exists to catch, and the refusal words are
    // what discriminate them. A genuine token with the next
    // required argument withheld must pass the gate and stop
    // at the missing argument instead, so the pin also fails
    // on a gate that refuses everything.
    let tmp = Tmp::new("token-shape");
    let rootfs = tmp.path().join("run-refuse000004").join("rootfs");
    let genuine = "ab".repeat(16);
    let malformed: Vec<(&str, String)> = vec![
        ("empty", String::new()),
        ("short", "ab".to_string()),
        ("thirty-one", "ab".repeat(15) + "a"),
        ("long", format!("{}c", genuine)),
        ("uppercase", "AB".repeat(16)),
        ("outside the alphabet", "gh".repeat(16)),
    ];
    for (name, value) in &malformed {
        let drive = Command::new(mayfly_bin())
            .arg("__assemble-root")
            .env(mayfly::INTERNAL_TOKEN_ENV, value)
            .output()
            .unwrap();
        assert_eq!(drive.status.code(), Some(1), "__assemble-root {}", name);
        let stderr = String::from_utf8_lossy(&drive.stderr);
        assert!(
            stderr.contains("refuses to run without the internal run token"),
            "__assemble-root {} {}",
            name,
            stderr
        );
        assert!(
            !stderr.contains("the prepared root failed"),
            "__assemble-root {} {}",
            name,
            stderr
        );
        let init = Command::new(mayfly_bin())
            .arg("__init")
            .arg("--rootfs")
            .arg(rootfs.as_os_str())
            .env(mayfly::INTERNAL_TOKEN_ENV, value)
            .output()
            .unwrap();
        assert_eq!(init.status.code(), Some(1), "__init {}", name);
        let stderr = String::from_utf8_lossy(&init.stderr);
        assert!(
            stderr.contains("refuses to run without the internal run token"),
            "__init {} {}",
            name,
            stderr
        );
        assert!(
            !stderr.contains("the prepared root failed"),
            "__init {} {}",
            name,
            stderr
        );
    }
    assert!(!rootfs.exists());
    // The opening direction. The drive with a genuine token
    // and no rootfs argument stops at the missing argument,
    // and the init with a genuine token and no separator
    // stops the same way, each at exit 1 with no refusal on
    // its error stream and nothing created, because the entry
    // point passed its gate before any host dependent act.
    let drive = Command::new(mayfly_bin())
        .arg("__assemble-root")
        .env(mayfly::INTERNAL_TOKEN_ENV, &genuine)
        .output()
        .unwrap();
    assert_eq!(drive.status.code(), Some(1));
    assert!(!String::from_utf8_lossy(&drive.stderr)
        .contains("refuses to run without the internal run token"));
    let init = Command::new(mayfly_bin())
        .arg("__init")
        .arg("--rootfs")
        .arg(rootfs.as_os_str())
        .env(mayfly::INTERNAL_TOKEN_ENV, &genuine)
        .output()
        .unwrap();
    assert_eq!(init.status.code(), Some(1));
    assert!(!String::from_utf8_lossy(&init.stderr)
        .contains("refuses to run without the internal run token"));
    assert!(!rootfs.exists());
}

#[test]
fn the_mount_point_parse_decodes_the_kernel_octal_escapes() {
    // The mounts table escapes a space in a mount point as a
    // backslash followed by three octal digits, and likewise
    // a tab and a newline and a backslash itself. The fallback
    // walk compares decoded paths against the real target, so
    // a tree bound under an escaped path is found and
    // remounted read only instead of passing unremounted
    // under a report of success. A backslash that does not
    // lead three octal digits stands as itself.
    assert_eq!(
        mayfly::init::decode_mount_point(r"/tmp/mayfly\040root/run-a/rootfs/usr"),
        "/tmp/mayfly root/run-a/rootfs/usr"
    );
    assert_eq!(
        mayfly::init::decode_mount_point(r"/a\011b\012c\134d"),
        "/a\tb\nc\\d"
    );
    assert_eq!(
        mayfly::init::decode_mount_point("/plain/path"),
        "/plain/path"
    );
    assert_eq!(
        mayfly::init::decode_mount_point(r"/short\04x"),
        r"/short\04x"
    );
    assert_eq!(
        mayfly::init::decode_mount_point(r"/digit\089"),
        r"/digit\089"
    );
    // An escape whose first digit stands above 3 names no
    // byte, because the widest byte is \377, and it stands as
    // itself like anything that is not an escape. The decode
    // once computed in u8 arithmetic, where a first digit of
    // 4 or more overflowed the product, panicking in a checked
    // build and wrapping in a release one, so these two cases
    // fail against that arithmetic for the real reason. The
    // widest escape that does name a byte still decodes, and
    // its byte is not valid text on its own, so the lossy
    // decode shows the replacement character for it.
    assert_eq!(
        mayfly::init::decode_mount_point(r"/wide\400x"),
        r"/wide\400x"
    );
    assert_eq!(
        mayfly::init::decode_mount_point(r"/wide\777x"),
        r"/wide\777x"
    );
    assert_eq!(
        mayfly::init::decode_mount_point(r"/edge\377x"),
        "/edge\u{fffd}x"
    );
    // The end of the field is the guard's own seat, where
    // its length comparison is the term that decides. A
    // backslash at the very end stands as itself, and so does
    // a backslash with one digit after it or with two,
    // because none of them leads three octal digits; a guard
    // written one comparison off in the admitting direction
    // reads one byte past the end on the two digit shape and
    // panics inside the assembly the fallback walk serves. A
    // complete escape at the exact end still decodes, because
    // a mount point can end in an escaped space, and a guard
    // written one comparison off in the refusing direction
    // leaves it standing undecoded, so the walk would miss a
    // tree bound under such a path.
    assert_eq!(mayfly::init::decode_mount_point(r"/trail\"), r"/trail\");
    assert_eq!(mayfly::init::decode_mount_point(r"/trail\0"), r"/trail\0");
    assert_eq!(mayfly::init::decode_mount_point(r"/trail\04"), r"/trail\04");
    assert_eq!(mayfly::init::decode_mount_point(r"/trail\040"), "/trail ");
}

#[test]
fn the_init_leaves_provenance_beside_an_assembled_root() {
    if !require_sandbox() {
        return;
    }
    // Drive the real assembly inside a real namespace with
    // no runner involved, so no runner write can account for
    // the provenance. The residue a killed prepare leaves is
    // whatever this assembly created, and it must carry the
    // provenance the sweep trusts or no sweep will collect it.
    // The drive presents the internal run token in its
    // environment, the way the supervisor presents it to the
    // children it spawns, because the drive refuses to run
    // without it.
    let tmp = Tmp::new("init-provenance");
    let sandbox = tmp.path().join("run-seam00000001");
    let rootfs = sandbox.join("rootfs");
    let output = Command::new(unshare_path())
        .args([
            os("--user"),
            os("--map-root-user"),
            os("--mount"),
            os("--pid"),
            os("--uts"),
            os("--ipc"),
            os("--fork"),
        ])
        .arg(mayfly_bin())
        .arg("__assemble-root")
        .arg("--rootfs")
        .arg(rootfs.as_os_str())
        .env(mayfly::INTERNAL_TOKEN_ENV, "ab".repeat(16))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let provenance = sandbox.join(".mayfly-provenance");
    assert!(provenance.is_file());
    assert_eq!(
        std::fs::read_to_string(&provenance).unwrap().trim(),
        "seam00000001"
    );
    assert!(rootfs.join("usr").is_dir());
    for name in ["etc", "dev", "proc", "sys", "work", "tmp"] {
        assert!(rootfs.join(name).is_dir());
    }
}

fn tmp_provenance_name() -> String {
    format!(".mayfly-provenance.tmp-{}", std::process::id())
}

#[test]
fn the_init_provenance_write_leaves_the_born_copy_standing() {
    // The init rewrites the provenance the runner wrote at
    // creation. A death inside that write must leave the born
    // copy standing whole, because a truncated provenance
    // names nothing and no sweep would ever collect the
    // directory it stands in. The Python pin drove the write
    // with a file handle whose write dies. The Rust seam
    // plants a directory where the write's temporary file
    // must land, so the write dies before any byte moves,
    // and the assembly runs in this process with its mount
    // layer stubbed, which is safe precisely because no
    // mount call then reaches the host.
    let tmp = Tmp::new("init-dying-write");
    let sandbox = tmp.path().join("run-born00000001");
    let rootfs = sandbox.join("rootfs");
    std::fs::create_dir_all(&rootfs).unwrap();
    let provenance = sandbox.join(".mayfly-provenance");
    std::fs::write(&provenance, "born00000001\n").unwrap();
    let blocker = sandbox.join(tmp_provenance_name());
    std::fs::create_dir_all(&blocker).unwrap();
    let noop_mount =
        |_: Option<&str>, _: &str, _: Option<&str>, _: u64| -> Result<(), String> { Ok(()) };
    let assembled = mayfly::init::assemble_root_with(&rootfs, &noop_mount);
    assert!(assembled.is_ok());
    assert_eq!(
        std::fs::read_to_string(&provenance).unwrap(),
        "born00000001\n"
    );
    let mut names = dir_names(&sandbox);
    names.sort();
    let mut expected = vec![
        ".mayfly-provenance".to_string(),
        tmp_provenance_name(),
        "rootfs".to_string(),
    ];
    expected.sort();
    assert_eq!(names, expected);
}

#[test]
fn the_init_writes_provenance_before_the_first_assembly_step() {
    // The provenance write is the first step of the assembly,
    // so a removal that lands before the assembly starts meets
    // the write rather than only the end of assembly one. This
    // drives the real assembly with its mount layer made to
    // die at the first mount call, the step the write now
    // precedes, and checks the provenance already stands while
    // no rootfs directory has been created. The seam is the
    // ordering inside the assembly itself, proven by the
    // state the dying assembly leaves behind.
    let tmp = Tmp::new("init-order");
    let sandbox = tmp.path().join("run-order0000001");
    let rootfs = sandbox.join("rootfs");
    std::fs::create_dir_all(&rootfs).unwrap();
    let calls = std::cell::Cell::new(0usize);
    let dying_mount =
        |_: Option<&str>, target: &str, _: Option<&str>, _: u64| -> Result<(), String> {
            let index = calls.get();
            calls.set(index + 1);
            if index == 0 {
                return Err(format!(
                    "[Errno {}] {}: '{}'",
                    libc::EPERM,
                    mayfly::strerror(libc::EPERM),
                    target
                ));
            }
            Ok(())
        };
    let assembled = mayfly::init::assemble_root_with(&rootfs, &dying_mount);
    assert!(assembled.is_err());
    let provenance = sandbox.join(".mayfly-provenance");
    assert!(provenance.is_file());
    assert_eq!(
        std::fs::read_to_string(&provenance).unwrap().trim(),
        "order0000001"
    );
    assert!(!rootfs.join("usr").exists());
}

#[test]
fn the_runner_provenance_write_leaves_no_partial_file_behind() {
    // The runner writes the provenance when it creates the
    // sandbox directory. A death inside that write must leave
    // the provenance path absent at creation, and a death
    // inside a rewrite must leave the complete file standing,
    // never a truncated one. The dying write is a directory
    // planted where the write's temporary file must land.
    let tmp = Tmp::new("runner-write");
    let sandbox = tmp.path().join("run-born00000002");
    std::fs::create_dir_all(&sandbox).unwrap();
    let provenance = sandbox.join(".mayfly-provenance");
    let blocker = sandbox.join(tmp_provenance_name());
    std::fs::create_dir_all(&blocker).unwrap();
    assert!(mayfly::write_provenance(&sandbox, "born00000002").is_err());
    assert!(!provenance.exists());
    std::fs::remove_dir_all(&blocker).unwrap();
    assert!(dir_names(&sandbox).is_empty());
    mayfly::write_provenance(&sandbox, "born00000002").unwrap();
    assert_eq!(
        std::fs::read_to_string(&provenance).unwrap(),
        "born00000002\n"
    );
    std::fs::create_dir_all(&blocker).unwrap();
    assert!(mayfly::write_provenance(&sandbox, "born00000003").is_err());
    assert_eq!(
        std::fs::read_to_string(&provenance).unwrap(),
        "born00000002\n"
    );
    std::fs::remove_dir_all(&blocker).unwrap();
    assert_eq!(dir_names(&sandbox), vec![".mayfly-provenance".to_string()]);
}

// ---------- killed inside the born write ----------

fn born_write_driver_child() {
    let root_dir = PathBuf::from(std::env::var_os("MAYFLY_TEST_DRIVER_ROOT").unwrap());
    let flag = PathBuf::from(std::env::var_os("MAYFLY_TEST_DRIVER_FLAG").unwrap());
    let bin = PathBuf::from(std::env::var_os("MAYFLY_TEST_DRIVER_BIN").unwrap());
    mayfly::set_helper_exe(&bin);
    let writer: Arc<mayfly::namespace::ProvenanceWriter> =
        Arc::new(move |sandbox: &Path, run_id: &str| -> std::io::Result<()> {
            let tmp = sandbox.join(format!(".mayfly-provenance.tmp-{}", std::process::id()));
            std::fs::write(&tmp, format!("{}\n", run_id))?;
            std::fs::write(&flag, "inside")?;
            std::thread::sleep(Duration::from_secs(120));
            Ok(())
        });
    let mut backend = NamespaceBackend::new();
    backend.provenance_writer = Some(writer);
    let _ = backend.run(&spec(py_cmd("pass"), &root_dir));
}

#[test]
fn a_supervisor_killed_inside_the_born_write_is_removed_by_the_janitor() {
    // The runner spawns the janitor before the born provenance
    // write, so a supervisor killed inside that write already
    // has a janitor watching the directory it leaves behind.
    // This drives a real run in a child process with the born
    // write replaced by one that opens the temporary file the
    // real write opens and writes the same complete bytes and
    // then hangs, and kills the child inside the write with
    // the temporary file standing and no provenance landed.
    // No later run is started, so no sweep can account for
    // the directory going away, and the only layer left that
    // can remove it is the janitor the run spawned before the
    // write began. Against the order that wrote the provenance
    // first, no janitor exists at the kill and the directory
    // stands, and this pin fails on its wait. The child is
    // this test binary re-invoked on this one test, with the
    // driver switch in its environment.
    if std::env::var_os("MAYFLY_TEST_DRIVER_BORN_WRITE").is_some() {
        born_write_driver_child();
        return;
    }
    let tmp = Tmp::new("born-write-kill");
    let root_dir = tmp.path().join("root");
    std::fs::create_dir_all(&root_dir).unwrap();
    let flag = tmp.path().join("inside-the-write");
    let mut proc = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "a_supervisor_killed_inside_the_born_write_is_removed_by_the_janitor",
            "--nocapture",
        ])
        .env("MAYFLY_TEST_DRIVER_BORN_WRITE", "1")
        .env("MAYFLY_TEST_DRIVER_ROOT", &root_dir)
        .env("MAYFLY_TEST_DRIVER_FLAG", &flag)
        .env("MAYFLY_TEST_DRIVER_BIN", mayfly_bin())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    assert!(wait_until(|| flag.exists(), 30.0));
    let standing = glob_run_dirs(&root_dir);
    assert_eq!(standing.len(), 1);
    let sandbox = standing[0].clone();
    assert!(
        dir_names(&sandbox)
            .iter()
            .any(|name| name.starts_with(".mayfly-provenance.tmp-")),
        "{:?}",
        dir_names(&sandbox)
    );
    assert!(!sandbox.join(".mayfly-provenance").exists());
    signal_child(&proc, libc::SIGKILL);
    wait_child(&mut proc, 30.0);
    assert!(
        wait_until(|| !sandbox.exists(), 30.0),
        "{}",
        sandbox.display()
    );
    assert!(wait_until(
        || match pids_referencing(&sandbox) {
            None => true,
            Some(pids) => pids.is_empty(),
        },
        30.0
    ));
    assert!(dir_names(&root_dir).is_empty());
}

#[test]
fn a_run_killed_inside_prepare_leaves_nothing_unsweepable() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("prepare-kills");
    let root_dir = tmp.path().join("root");
    std::fs::create_dir_all(&root_dir).unwrap();
    let marker = format!("prepare-window-{}", std::process::id());
    for _trial in 0..8 {
        let script = format!("import time; time.sleep(30)  # {}", marker);
        let args = cli_args(&root_dir, &py_cmd(&script));
        let mut proc = Command::new(mayfly_bin())
            .args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if proc.try_wait().ok().flatten().is_some() {
                break;
            }
            let under_way = glob_run_dirs(&root_dir)
                .iter()
                .any(|dir| dir.join("rootfs").join("usr").exists());
            if under_way {
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        if proc.try_wait().ok().flatten().is_none() {
            signal_child(&proc, libc::SIGKILL);
        }
        wait_child(&mut proc, 30.0);
    }
    let quiet = |marker: &str, root_dir: &Path| -> bool {
        if !host_pids_with_marker(marker).is_empty() {
            return false;
        }
        for leftover in glob_run_dirs(root_dir) {
            match pids_referencing(&leftover) {
                None => {}
                Some(pids) => {
                    if !pids.is_empty() {
                        return false;
                    }
                }
            }
        }
        true
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut quiet_seen = false;
    while Instant::now() < deadline {
        if quiet(&marker, &root_dir) {
            std::thread::sleep(Duration::from_millis(500));
            if quiet(&marker, &root_dir) {
                quiet_seen = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(quiet_seen, "the killed runs never went quiet");
    let leftovers = glob_run_dirs(&root_dir);
    for leftover in &leftovers {
        let run_id = leftover.file_name().unwrap().to_string_lossy().into_owned();
        let run_id = run_id.strip_prefix("run-").unwrap().to_string();
        let provenance = leftover.join(".mayfly-provenance");
        assert!(provenance.is_file(), "{}", leftover.display());
        assert_eq!(std::fs::read_to_string(&provenance).unwrap().trim(), run_id);
        age_directory(leftover, 60.0);
    }
    for clean_marker in glob_clean_markers(&root_dir) {
        age_directory(&clean_marker, 60.0);
    }
    for _attempt in 0..3 {
        if glob_run_dirs(&root_dir).is_empty() && glob_clean_markers(&root_dir).is_empty() {
            break;
        }
        let report = run_task(py_cmd("pass"), &root_dir);
        assert_eq!(report.outcome, Outcome::Task);
    }
    assert!(glob_run_dirs(&root_dir).is_empty());
    assert!(glob_clean_markers(&root_dir).is_empty());
}

// ---------- the bounds of MF-13 ----------

const BUSY_LOOP: &str = "exec('while True:' + chr(10) + '    pass')";

#[test]
fn timeout_kills_a_task_past_its_deadline_and_reports_a_harness_error() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("bound-timeout");
    let mut task_spec = spec(py_cmd("import time; time.sleep(30)"), tmp.path());
    task_spec.timeout_seconds = Some(1);
    let began = Instant::now();
    let report = run_task_spec(&task_spec);
    let elapsed = began.elapsed();
    assert_eq!(report.outcome, Outcome::HarnessError);
    assert_eq!(
        report.error.as_deref(),
        Some(
            "the task did not finish within the timeout of 1 seconds and the sandbox was torn down"
        )
    );
    assert_eq!(report.task_exit_code, None);
    assert_eq!(report.task_signal, None);
    assert_eq!(report.sandbox_removed, Some(true));
    assert!(
        elapsed < Duration::from_secs(25),
        "the run stood for {:?} against a one second timeout",
        elapsed
    );
}

#[test]
fn a_second_timeout_value_kills_at_its_own_deadline_and_reports_a_harness_error() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("bound-timeout-second");
    let mut task_spec = spec(py_cmd("import time; time.sleep(30)"), tmp.path());
    task_spec.timeout_seconds = Some(2);
    let began = Instant::now();
    let report = run_task_spec(&task_spec);
    let elapsed = began.elapsed();
    assert_eq!(report.outcome, Outcome::HarnessError);
    assert_eq!(
        report.error.as_deref(),
        Some(
            "the task did not finish within the timeout of 2 seconds and the sandbox was torn down"
        )
    );
    assert_eq!(report.task_exit_code, None);
    assert_eq!(report.task_signal, None);
    assert_eq!(report.sandbox_removed, Some(true));
    assert!(
        elapsed < Duration::from_secs(25),
        "the run stood for {:?} against a two second timeout",
        elapsed
    );
}

#[test]
fn a_task_still_running_past_a_close_deadline_is_killed_at_the_deadline() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("bound-timeout-close");
    let mut task_spec = spec(py_cmd("import time; time.sleep(3)"), tmp.path());
    task_spec.timeout_seconds = Some(2);
    let began = Instant::now();
    let report = run_task_spec(&task_spec);
    let elapsed = began.elapsed();
    assert_eq!(report.outcome, Outcome::HarnessError);
    assert_eq!(
        report.error.as_deref(),
        Some(
            "the task did not finish within the timeout of 2 seconds and the sandbox was torn down"
        )
    );
    assert_eq!(report.task_exit_code, None);
    assert_eq!(report.task_signal, None);
    assert_eq!(report.sandbox_removed, Some(true));
    assert!(
        elapsed < Duration::from_millis(2750),
        "the run stood for {:?} against a two second deadline",
        elapsed
    );
}

#[test]
fn the_timeout_words_name_the_seconds_the_caller_set() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("bound-timeout-words");
    let mut task_spec = spec(py_cmd("import time; time.sleep(30)"), tmp.path());
    task_spec.timeout_seconds = Some(2);
    let report = run_task_spec(&task_spec);
    assert_eq!(report.outcome, Outcome::HarnessError);
    assert_eq!(
        report.error.as_deref(),
        Some(
            "the task did not finish within the timeout of 2 seconds and the sandbox was torn down"
        )
    );
}

#[test]
fn a_task_done_before_the_deadline_keeps_its_task_outcome() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("bound-timeout-done");
    let mut task_spec = spec(
        py_cmd("import time, sys; time.sleep(0.2); sys.exit(7)"),
        tmp.path(),
    );
    task_spec.timeout_seconds = Some(30);
    let report = run_task_spec(&task_spec);
    assert_eq!(report.outcome, Outcome::Task);
    assert_eq!(report.task_exit_code, Some(7));
    assert_eq!(report.error, None);
    assert!(report.timings_ms.get(Phase::Task).is_some());
}

#[test]
fn a_busy_task_dies_by_the_processor_limit_signal_at_its_cpu_ceiling() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("bound-cpu-soft");
    let mut task_spec = spec(py_cmd(BUSY_LOOP), tmp.path());
    task_spec.max_cpu_seconds = Some(1);
    let report = run_task_spec(&task_spec);
    assert_eq!(report.outcome, Outcome::Task);
    assert_eq!(report.task_signal, Some(24));
    assert_eq!(report.task_exit_code, None);
}

#[test]
fn a_busy_task_under_a_timeout_dies_by_its_cpu_ceiling_before_the_timeout() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("bound-cpu-bounded");
    let mut task_spec = spec(py_cmd(BUSY_LOOP), tmp.path());
    task_spec.max_cpu_seconds = Some(1);
    task_spec.timeout_seconds = Some(10);
    let began = Instant::now();
    let report = run_task_spec(&task_spec);
    let elapsed = began.elapsed();
    assert_eq!(report.outcome, Outcome::Task);
    assert_eq!(report.task_signal, Some(24));
    assert_eq!(report.task_exit_code, None);
    assert!(
        elapsed < Duration::from_secs(9),
        "the run stood for {:?} against its one second processor ceiling",
        elapsed
    );
}

#[test]
fn a_task_that_ignores_the_processor_signal_dies_by_the_kill_signal_at_the_hard_limit() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("bound-cpu-hard");
    let script = format!(
        "import signal; signal.signal(signal.SIGXCPU, signal.SIG_IGN); {}",
        BUSY_LOOP
    );
    let mut task_spec = spec(py_cmd(&script), tmp.path());
    task_spec.max_cpu_seconds = Some(1);
    let report = run_task_spec(&task_spec);
    assert_eq!(report.outcome, Outcome::Task);
    assert_eq!(report.task_signal, Some(9));
    assert_eq!(report.task_exit_code, None);
}

#[test]
fn a_task_under_a_memory_ceiling_it_never_reaches_runs_unchanged() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("bound-memory-unreached");
    let mut task_spec = spec(
        py_cmd("import sys; print('one quiet line'); sys.exit(5)"),
        tmp.path(),
    );
    task_spec.max_memory_bytes = Some(1073741824);
    let report = run_task_spec(&task_spec);
    assert_eq!(report.outcome, Outcome::Task);
    assert_eq!(report.task_exit_code, Some(5));
    assert!(report.stdout_tail.contains("one quiet line"));
}

#[test]
fn a_task_that_allocates_past_its_memory_ceiling_answers_for_the_refusal_itself() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("bound-memory-refusal");
    let script = "import sys\ntry:\n    blob = bytearray(3 * 268435456)\n    sys.exit(0)\nexcept MemoryError:\n    sys.exit(42)";
    let mut task_spec = spec(py_cmd(script), tmp.path());
    task_spec.max_memory_bytes = Some(268435456);
    let report = run_task_spec(&task_spec);
    assert_eq!(report.outcome, Outcome::Task);
    assert_eq!(report.task_exit_code, Some(42));
    assert_eq!(report.error, None);
}

#[test]
fn a_bound_under_its_floor_is_refused_before_anything_is_prepared() {
    ensure_helper();
    let tmp = Tmp::new("bound-floor");
    // A sweepable orphan stands in the root before the refused
    // calls, planted the way the sweep pins plant theirs. The
    // floors are enforced ahead of the orphan sweep, so the
    // orphan still stands after every refusal. A validation
    // seated after the sweep would take the orphan with it and
    // the closing assertions would read an empty root either
    // way, so the orphan is what makes the seat observable.
    let orphan = plant_sweepable(tmp.path(), "aaaa11111111");
    // The refused calls run through a backend whose provenance
    // writer records every call it receives and writes nothing.
    // A preparation the cleanup hid would still pass through
    // the writer on its way to the sandbox, so the empty record
    // at the close is what proves no call prepared anything.
    // The writer also names no directory and writes no file, so
    // the root the closing assertions read carries only what
    // the calls themselves left standing.
    let preparations: Arc<Mutex<Vec<PathBuf>>> = Arc::new(Mutex::new(Vec::new()));
    let record = preparations.clone();
    let mut backend = NamespaceBackend::new();
    backend.provenance_writer = Some(Arc::new(move |dir: &Path, _run_id: &str| {
        record.lock().unwrap().push(dir.to_path_buf());
        Ok(())
    }));
    let mut timeout_spec = spec(py_cmd("pass"), tmp.path());
    timeout_spec.timeout_seconds = Some(0);
    let report = backend.run(&timeout_spec);
    assert_eq!(report.outcome, Outcome::HarnessError);
    assert_eq!(
        report.error.as_deref(),
        Some("the timeout of 0 seconds stands under the floor of 1 second")
    );
    let mut cpu_spec = spec(py_cmd("pass"), tmp.path());
    cpu_spec.max_cpu_seconds = Some(0);
    let report = backend.run(&cpu_spec);
    assert_eq!(report.outcome, Outcome::HarnessError);
    assert_eq!(
        report.error.as_deref(),
        Some("the processor ceiling of 0 seconds stands under the floor of 1 second")
    );
    let mut memory_spec = spec(py_cmd("pass"), tmp.path());
    memory_spec.max_memory_bytes = Some(1024);
    let report = backend.run(&memory_spec);
    assert_eq!(report.outcome, Outcome::HarnessError);
    assert_eq!(
        report.error.as_deref(),
        Some("the address space ceiling of 1024 bytes stands under the floor of 16777216 bytes")
    );
    // No refused call reached the provenance writer, so no call
    // prepared a sandbox the cleanup could have hidden.
    assert!(preparations.lock().unwrap().is_empty());
    // The orphan the calls found standing is the only entry
    // the root holds, and it is the orphan itself, untouched.
    assert_eq!(glob_run_dirs(tmp.path()), vec![orphan.clone()]);
    assert_eq!(dir_names(tmp.path()), vec!["run-aaaa11111111".to_string()]);
    assert_eq!(
        std::fs::read_to_string(orphan.join(".mayfly-provenance")).unwrap(),
        "aaaa11111111\n"
    );
}

#[test]
fn a_processor_ceiling_at_the_widest_value_is_refused_before_anything_is_prepared() {
    ensure_helper();
    let tmp = Tmp::new("bound-widest");
    // The orphan and the recording writer stand as they stand
    // in the floor pin, so the seat of this refusal is
    // observable the same way. The widest value names a hard
    // limit one second past itself, and no count holds that
    // limit, so the value is refused where the floors are
    // refused. Against code that forms the pair unchecked the
    // run reaches the forked child instead, and its answer is
    // a start failure in a release build and the child's own
    // death reported as a task outcome in a checked build, and
    // neither answer is the refusal this pin reads.
    let orphan = plant_sweepable(tmp.path(), "bbbb22222222");
    let preparations: Arc<Mutex<Vec<PathBuf>>> = Arc::new(Mutex::new(Vec::new()));
    let record = preparations.clone();
    let mut backend = NamespaceBackend::new();
    backend.provenance_writer = Some(Arc::new(move |dir: &Path, _run_id: &str| {
        record.lock().unwrap().push(dir.to_path_buf());
        Ok(())
    }));
    let mut widest_spec = spec(py_cmd("pass"), tmp.path());
    widest_spec.max_cpu_seconds = Some(u64::MAX);
    let report = backend.run(&widest_spec);
    assert_eq!(report.outcome, Outcome::HarnessError);
    assert_eq!(
        report.error.as_deref(),
        Some("the processor ceiling of 18446744073709551615 seconds stands over the highest formable ceiling of 18446744073709551614 seconds")
    );
    assert!(preparations.lock().unwrap().is_empty());
    assert_eq!(glob_run_dirs(tmp.path()), vec![orphan.clone()]);
    assert_eq!(dir_names(tmp.path()), vec!["run-bbbb22222222".to_string()]);
    assert_eq!(
        std::fs::read_to_string(orphan.join(".mayfly-provenance")).unwrap(),
        "bbbb22222222\n"
    );
}

#[test]
fn cli_refuses_the_widest_processor_ceiling_at_2() {
    ensure_helper();
    let tmp = Tmp::new("bound-cli-widest");
    let mut out: Vec<u8> = Vec::new();
    let mut err: Vec<u8> = Vec::new();
    let mut args = vec![
        os("run"),
        os("--root"),
        tmp.path().as_os_str().to_os_string(),
        os("--max-cpu"),
        os("18446744073709551615"),
        os("--"),
    ];
    args.extend(py_cmd("pass"));
    let code = cli_main(&args, &mut out, &mut err);
    assert_eq!(code, 2);
    assert!(out.is_empty());
    assert_eq!(
        String::from_utf8(err).unwrap(),
        "mayfly run needs --max-cpu of at most 18446744073709551614 seconds\n"
    );
    assert_eq!(dir_names(tmp.path()), Vec::<String>::new());
}

#[test]
fn a_processor_ceiling_at_its_highest_value_is_accepted_on_both_surfaces() {
    // The ceiling's refusal side stands pinned at both
    // judgement seats, at the widest value, and the admitted
    // edge itself, the highest formable ceiling of
    // 18446744073709551614 seconds, stands driven on neither
    // surface: a ceiling comparison written on the wrong side
    // of the edge, refusing at or over the ceiling, would
    // refuse a value the contract accepts and no pin would
    // fail. On the command line the call names the edge value
    // beside an unknown backend, and the unknown backend
    // report at 125 proves the value passed the ceiling check
    // on its way to the backend resolution, where the wrong
    // comparison answers 2 in the ceiling words with no
    // report printed. On the library surface the spec carries
    // the edge value through the recording writer backend of
    // the floor pins, its unshare missing the way the missing
    // unshare pin leaves it, and the run reaches the writer
    // and answers with the launch failure of its missing
    // unshare, where the wrong comparison refuses at the
    // bound check in the ceiling sentence with the writer
    // never reached.
    ensure_helper();
    let tmp = Tmp::new("bound-cli-ceiling-value");
    let mut out: Vec<u8> = Vec::new();
    let mut err: Vec<u8> = Vec::new();
    let mut args = vec![
        os("run"),
        os("--root"),
        tmp.path().as_os_str().to_os_string(),
        os("--max-cpu"),
        os("18446744073709551614"),
        os("--backend"),
        os("nosuch"),
        os("--"),
    ];
    args.extend(cmd(&["/bin/echo", "landed"]));
    let code = cli_main(&args, &mut out, &mut err);
    assert_eq!(code, HARNESS_EXIT_CODE);
    assert!(err.is_empty());
    let payload: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(payload["backend"], "nosuch");
    assert_eq!(payload["outcome"], "harness_error");
    assert_eq!(payload["error"], "unknown backend, nosuch");
    assert_eq!(dir_names(tmp.path()), Vec::<String>::new());
    let tmp = Tmp::new("bound-ceiling-value");
    let preparations: Arc<Mutex<Vec<PathBuf>>> = Arc::new(Mutex::new(Vec::new()));
    let record = preparations.clone();
    let mut backend =
        NamespaceBackend::with_unshare_path(Some(PathBuf::from("/nonexistent/unshare")));
    backend.provenance_writer = Some(Arc::new(move |dir: &Path, _run_id: &str| {
        record.lock().unwrap().push(dir.to_path_buf());
        Ok(())
    }));
    let mut ceiling_spec = spec(py_cmd("pass"), tmp.path());
    ceiling_spec.max_cpu_seconds = Some(u64::MAX - 1);
    let report = backend.run(&ceiling_spec);
    assert_eq!(report.outcome, Outcome::HarnessError);
    assert_eq!(
        report.error.as_deref(),
        Some(
            "could not launch unshare, [Errno 2] No such file or directory: '/nonexistent/unshare'"
        )
    );
    assert_eq!(preparations.lock().unwrap().len(), 1);
    assert_eq!(report.sandbox_removed, Some(true));
    assert!(dir_names(tmp.path()).is_empty());
}

#[test]
fn cli_refuses_a_bound_value_under_its_floor_at_2() {
    // The library floor refusals are pinned word for word
    // through the backend, and the command line's missing
    // value, whole number, past range, and ceiling refusals
    // are pinned in their own words. The floor refusal is
    // the one arm no pin drove: a parser that lost its floor
    // check would hand the value to the backend, which
    // refuses it as a harness error at 125 with a report
    // printed, where this arm refuses at 2 in the bound's
    // own words with no report and nothing prepared.
    ensure_helper();
    let tmp = Tmp::new("bound-cli-floor");
    let cases: [(&str, &str, &str); 4] = [
        (
            "--timeout",
            "0",
            "mayfly run needs --timeout of at least 1 second\n",
        ),
        (
            "--max-cpu",
            "0",
            "mayfly run needs --max-cpu of at least 1 second\n",
        ),
        (
            "--max-memory",
            "0",
            "mayfly run needs --max-memory of at least 16777216 bytes\n",
        ),
        (
            "--max-memory",
            "16777215",
            "mayfly run needs --max-memory of at least 16777216 bytes\n",
        ),
    ];
    for (name, value, expected) in cases {
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        let mut args = vec![
            os("run"),
            os("--root"),
            tmp.path().as_os_str().to_os_string(),
            os(name),
            os(value),
            os("--"),
        ];
        args.extend(py_cmd("pass"));
        let code = cli_main(&args, &mut out, &mut err);
        assert_eq!(code, 2, "{}", name);
        assert_eq!(String::from_utf8(err).unwrap(), expected, "{}", name);
        assert!(out.is_empty(), "{}", name);
    }
    assert_eq!(dir_names(tmp.path()), Vec::<String>::new());
}

#[test]
fn a_bound_value_at_its_floor_is_accepted_on_both_surfaces() {
    // MF-13's floor rule refuses a value under the floor, so
    // the floor value itself stands accepted, and that edge
    // stands pinned for the timeout and the processor ceiling
    // at exactly one second. The address space ceiling stood
    // driven under its floor on both surfaces and above it,
    // but at exactly 16777216 bytes on neither: a floor
    // comparison written on the wrong side of the comparison,
    // refusing the floor value itself, would refuse a value
    // the contract accepts and no pin would fail. On the
    // command line the call names the floor value beside an
    // unknown backend, and the unknown backend report at 125
    // proves the value passed the floor check on its way to
    // the backend resolution, where the wrong comparison
    // answers 2 in the floor words with no report printed. On
    // the library surface the spec carries the floor value
    // through the recording writer backend of the floor pin
    // above, its unshare missing the way the missing unshare
    // pin leaves it, and the run reaches the writer and
    // answers with the launch failure of its missing unshare,
    // where the wrong comparison refuses at the bound check
    // in the floor sentence with the writer never reached.
    ensure_helper();
    let tmp = Tmp::new("bound-cli-floor-value");
    let mut out: Vec<u8> = Vec::new();
    let mut err: Vec<u8> = Vec::new();
    let mut args = vec![
        os("run"),
        os("--root"),
        tmp.path().as_os_str().to_os_string(),
        os("--max-memory"),
        os("16777216"),
        os("--backend"),
        os("nosuch"),
        os("--"),
    ];
    args.extend(cmd(&["/bin/echo", "landed"]));
    let code = cli_main(&args, &mut out, &mut err);
    assert_eq!(code, HARNESS_EXIT_CODE);
    assert!(err.is_empty());
    let payload: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(payload["backend"], "nosuch");
    assert_eq!(payload["outcome"], "harness_error");
    assert_eq!(payload["error"], "unknown backend, nosuch");
    assert_eq!(dir_names(tmp.path()), Vec::<String>::new());
    let tmp = Tmp::new("bound-floor-value");
    let preparations: Arc<Mutex<Vec<PathBuf>>> = Arc::new(Mutex::new(Vec::new()));
    let record = preparations.clone();
    let mut backend =
        NamespaceBackend::with_unshare_path(Some(PathBuf::from("/nonexistent/unshare")));
    backend.provenance_writer = Some(Arc::new(move |dir: &Path, _run_id: &str| {
        record.lock().unwrap().push(dir.to_path_buf());
        Ok(())
    }));
    let mut memory_spec = spec(py_cmd("pass"), tmp.path());
    memory_spec.max_memory_bytes = Some(16777216);
    let report = backend.run(&memory_spec);
    assert_eq!(report.outcome, Outcome::HarnessError);
    assert_eq!(
        report.error.as_deref(),
        Some(
            "could not launch unshare, [Errno 2] No such file or directory: '/nonexistent/unshare'"
        )
    );
    assert_eq!(preparations.lock().unwrap().len(), 1);
    assert_eq!(report.sandbox_removed, Some(true));
    assert!(dir_names(tmp.path()).is_empty());
}

#[test]
fn the_bounds_never_reach_the_task_environment() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("bound-env");
    let script = "import json, os; print(json.dumps(dict(os.environ)))";
    let mut task_spec = spec(py_cmd(script), tmp.path());
    task_spec.caller_env = vec![
        (os("PATH"), os("/usr/bin:/bin")),
        (os("MAYFLY_LEAK_PROBE"), os("a secret value")),
    ];
    task_spec.timeout_seconds = Some(60);
    task_spec.max_cpu_seconds = Some(60);
    task_spec.max_memory_bytes = Some(1073741824);
    let report = run_task_spec(&task_spec);
    let seen = last_json(&report);
    assert_eq!(
        seen,
        serde_json::json!({
            "PATH": "/usr/bin:/bin",
            "MAYFLY_SANDBOX": "namespace",
            "PYTHONCOERCECLOCALE": "0",
        })
    );
}

#[test]
fn cli_timeout_reports_a_harness_error_at_125() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("bound-cli-timeout");
    let mut args = vec![
        os("run"),
        os("--root"),
        tmp.path().as_os_str().to_os_string(),
        os("--timeout"),
        os("1"),
        os("--"),
    ];
    args.extend(py_cmd("import time; time.sleep(30)"));
    let result = run_cli(&args, &[]);
    assert_eq!(result.code, 125);
    let payload: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert_eq!(payload["outcome"], "harness_error");
    assert_eq!(
        payload["error"],
        "the task did not finish within the timeout of 1 seconds and the sandbox was torn down"
    );
    assert_eq!(payload["task_exit_code"], serde_json::Value::Null);
}

#[test]
fn cli_cpu_ceiling_reports_the_signal_death_at_128_plus_the_signal() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("bound-cli-cpu");
    let mut args = vec![
        os("run"),
        os("--root"),
        tmp.path().as_os_str().to_os_string(),
        os("--max-cpu"),
        os("1"),
        os("--"),
    ];
    args.extend(py_cmd(BUSY_LOOP));
    let result = run_cli(&args, &[]);
    assert_eq!(result.code, 152);
    let payload: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert_eq!(payload["outcome"], "task");
    assert_eq!(payload["task_signal"], 24);
    assert_eq!(payload["task_exit_code"], serde_json::Value::Null);
}

#[test]
fn cli_rejects_a_bound_value_that_is_not_a_whole_number() {
    let tmp = Tmp::new("bound-cli-number");
    let mut args = vec![
        os("run"),
        os("--root"),
        tmp.path().as_os_str().to_os_string(),
        os("--timeout"),
        os("soon"),
        os("--"),
    ];
    args.extend(py_cmd("pass"));
    let result = run_cli(&args, &[]);
    assert_eq!(result.code, 2);
    assert!(result
        .stderr
        .contains("mayfly run needs a whole number after --timeout"));
    assert!(result.stdout.is_empty());
}

#[test]
fn cli_refuses_a_bound_value_past_the_widest_number_in_range_words() {
    // A value written in digits alone is a whole number at any
    // width, and a digit string past the widest number the
    // parser carries fails the parse on its width alone. The
    // whole number refusal would state a falsehood for it, so
    // it refuses in range words instead: the ceiling words of
    // its bound where the bound carries a ceiling, and the
    // widest number words where it carries none. The refusal
    // stands at exit 2, with nothing on standard output and
    // nothing prepared in the root.
    let tmp = Tmp::new("bound-cli-past-range");
    let cases: [(&str, &str); 3] = [
        (
            "--timeout",
            "mayfly run needs a value after --timeout no larger than 18446744073709551615\n",
        ),
        (
            "--max-cpu",
            "mayfly run needs --max-cpu of at most 18446744073709551614 seconds\n",
        ),
        (
            "--max-memory",
            "mayfly run needs a value after --max-memory no larger than 18446744073709551615\n",
        ),
    ];
    for (name, expected) in cases {
        let mut args = vec![
            os("run"),
            os("--root"),
            tmp.path().as_os_str().to_os_string(),
            os(name),
            os("99999999999999999999999999"),
            os("--"),
        ];
        args.extend(py_cmd("pass"));
        let result = run_cli(&args, &[]);
        assert_eq!(result.code, 2, "{}", name);
        assert_eq!(result.stderr, expected, "{}", name);
        assert!(result.stdout.is_empty(), "{}", name);
    }
    assert_eq!(dir_names(tmp.path()), Vec::<String>::new());
}

#[test]
fn an_unbounded_runs_report_stands_as_landed() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("bound-unbounded");
    let result = run_cli(&cli_args(tmp.path(), &cmd(&["/bin/echo", "landed"])), &[]);
    assert_eq!(result.code, 0);
    let payload: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    let fields: Vec<&str> = payload
        .as_object()
        .unwrap()
        .keys()
        .map(|key| key.as_str())
        .collect();
    assert_eq!(fields, REPORT_FIELDS);
    let timing_fields: Vec<&str> = payload["timings_ms"]
        .as_object()
        .unwrap()
        .keys()
        .map(|key| key.as_str())
        .collect();
    assert_eq!(timing_fields, PHASE_NAMES);
    assert_eq!(payload["backend"], "namespace");
    assert_eq!(
        payload["command"],
        serde_json::json!(["/bin/echo", "landed"])
    );
    assert_eq!(payload["outcome"], "task");
    assert_eq!(payload["task_exit_code"], 0);
    assert_eq!(payload["task_signal"], serde_json::Value::Null);
    assert_eq!(payload["error"], serde_json::Value::Null);
    assert_eq!(payload["sandbox_removed"], true);
    assert_eq!(payload["stdout_tail"], "landed\n");
    assert_eq!(payload["stderr_tail"], "");
    let run_id = payload["run_id"].as_str().unwrap();
    assert_eq!(run_id.len(), 12);
    assert!(run_id
        .chars()
        .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)));
    assert_eq!(
        payload["sandbox_dir"],
        serde_json::json!(format!("{}/run-{}", tmp.path().display(), run_id))
    );
    for phase in PHASE_NAMES {
        assert!(
            payload["timings_ms"][phase].is_number(),
            "the {} timing did not stand as a number",
            phase
        );
    }
}

#[test]
fn json_value_debug_and_clone_stand_with_the_other_public_types() {
    let value = mayfly::JsonValue::Object(vec![
        (
            "backend".to_string(),
            mayfly::JsonValue::Str("namespace".to_string()),
        ),
        ("task_exit_code".to_string(), mayfly::JsonValue::Int(7)),
        (
            "flags".to_string(),
            mayfly::JsonValue::List(vec![mayfly::JsonValue::Bool(true), mayfly::JsonValue::Null]),
        ),
    ]);
    let cloned = value.clone();
    assert_eq!(format!("{:?}", cloned), format!("{:?}", value));
    assert!(format!("{:?}", value).contains("Int(7)"));
}

#[test]
fn cli_unknown_backend_reports_the_full_report_shape() {
    let tmp = Tmp::new("cli-unknown-backend");
    let mut args = vec![
        os("run"),
        os("--root"),
        tmp.path().as_os_str().to_os_string(),
        os("--backend"),
        os("nosuch"),
        os("--"),
    ];
    args.extend(cmd(&["/bin/echo", "landed"]));
    let result = run_cli(&args, &[]);
    assert_eq!(result.code, 125);
    let payload: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    let fields: Vec<&str> = payload
        .as_object()
        .unwrap()
        .keys()
        .map(|key| key.as_str())
        .collect();
    assert_eq!(fields, REPORT_FIELDS);
    assert_eq!(payload["backend"], "nosuch");
    assert_eq!(payload["run_id"], "");
    assert_eq!(payload["sandbox_dir"], "");
    assert_eq!(
        payload["command"],
        serde_json::json!(["/bin/echo", "landed"])
    );
    assert_eq!(payload["outcome"], "harness_error");
    assert_eq!(payload["task_exit_code"], serde_json::Value::Null);
    assert_eq!(payload["task_signal"], serde_json::Value::Null);
    assert_eq!(payload["error"], "unknown backend, nosuch");
    assert_eq!(payload["proc_mounted"], false);
    assert_eq!(payload["sandbox_removed"], true);
    assert_eq!(payload["stdout_tail"], "");
    assert_eq!(payload["stderr_tail"], "");
    let timing_fields: Vec<&str> = payload["timings_ms"]
        .as_object()
        .unwrap()
        .keys()
        .map(|key| key.as_str())
        .collect();
    assert_eq!(timing_fields, PHASE_NAMES);
    for phase in PHASE_NAMES {
        assert_eq!(payload["timings_ms"][phase], serde_json::Value::Null);
    }
}

#[test]
fn the_report_writer_escapes_non_ascii_and_control_text_the_python_way() {
    // Every string in a printed report passes through the
    // one JSON writer, whose stated contract is the Python
    // harness's escaping, every non-ascii character written
    // as a unicode escape. The suite's other report pins
    // parse the printed JSON back into values, and parsing
    // cannot tell the escaped form from the raw character,
    // so the writer's branches stood unexercised. This pin
    // reads the printed text itself for a tail carrying one
    // character per branch.
    let mut report = RunReport::new(
        "namespace",
        "abcdef012345",
        "/nonexistent",
        cmd(&["/bin/echo"]),
        Outcome::Task,
    );
    report.stdout_tail =
        "caf\u{e9} \u{20ac} \u{1f600} \u{1} \"quoted\" back\\slash\n\u{7f}".to_string();
    let printed = report.to_json();
    for escaped in [
        "caf\\u00e9",
        "\\u20ac",
        "\\ud83d\\ude00",
        "\\u0001",
        "\\\"quoted\\\"",
        "back\\\\slash",
        "slash\\n",
        "\\u007f",
    ] {
        assert!(printed.contains(escaped), "{}", escaped);
    }
    let parsed: serde_json::Value = serde_json::from_str(&printed).unwrap();
    assert_eq!(parsed["stdout_tail"], report.stdout_tail);
}

#[test]
fn backend_names_names_exactly_the_backends_get_backend_resolves() {
    // The registry's enumeration and its lookup must move
    // together, as backend_names' own doc states. A name
    // added to the list, dropped from it, or renamed on
    // either side leaves the enumeration misstating the
    // registry it enumerates, and no other pin reads the
    // enumeration at all. The one form this pin cannot see
    // is a name added to the lookup alone, which no finite
    // probe of the list can enumerate; that form stands
    // guarded by the doc itself and by the second backend
    // event MF-A14 waits on, when an addition must land on
    // both sides at once. The list stands at exactly the
    // one registered name, and every name it carries
    // resolves through get_backend to a backend reporting
    // that same name.
    let names = mayfly::backend_names();
    assert_eq!(names, vec!["namespace"]);
    for name in &names {
        let backend = mayfly::get_backend(name).unwrap();
        assert_eq!(backend.name(), *name);
    }
}

#[test]
fn the_report_writer_prints_a_whole_float_with_its_decimal_point() {
    // py_float prints a float the way Python prints one in
    // json output, with a trailing .0 on a whole value and
    // the shortest round trip decimal otherwise. Every pin
    // that reads a report parses the printed json back into
    // values, and parsing cannot tell 104 from 104.0, while
    // a consumer shaped like the Python harness the writer
    // mirrors can tell them apart at once. A writer that
    // dropped the decimal point would reshape every report
    // whose timing lands on a whole millisecond, and this
    // pin is what fails there, at the function itself and
    // through one report's printed text.
    assert_eq!(mayfly::py_float(104.0), "104.0");
    assert_eq!(mayfly::py_float(0.5), "0.5");
    assert_eq!(mayfly::py_float(104.36), "104.36");
    let mut report = RunReport::new(
        "namespace",
        "abc000000001",
        "/root/run-abc000000001",
        cmd(&["/bin/echo"]),
        Outcome::Task,
    );
    report.timings_ms.total = Some(104.0);
    let text = report.to_json();
    assert!(text.contains("\"total\": 104.0"), "{}", text);
}

struct FailingWriter;

impl Write for FailingWriter {
    fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "simulated broken output",
        ))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "simulated broken output",
        ))
    }
}

#[test]
fn mf14_cli_help_and_version_flags_and_broken_stdout() {
    let expected_usage = "usage: mayfly run --root DIR [--backend NAME] [--allow-env KEY]... [--ro-bind HOST:SANDBOX]... [--net] [--timeout SECONDS] [--max-cpu SECONDS] [--max-memory BYTES] -- COMMAND...\n";
    let expected_version = format!("mayfly {}\n", env!("CARGO_PKG_VERSION"));

    for flag in ["-h", "--help"] {
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        let code = cli_main(&[os(flag)], &mut out, &mut err);
        assert_eq!(code, 0, "flag {flag}");
        assert_eq!(String::from_utf8(out).unwrap(), expected_usage);
        assert!(err.is_empty());
    }
    for flag in ["-V", "--version"] {
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        let code = cli_main(&[os(flag)], &mut out, &mut err);
        assert_eq!(code, 0, "flag {flag}");
        assert_eq!(String::from_utf8(out).unwrap(), expected_version);
        assert!(err.is_empty());
    }
    for bad in [
        vec![os("-h"), os("run")],
        vec![os("--help"), os("extra")],
        vec![os("-V"), os("extra")],
        vec![os("--version"), os("extra")],
        vec![os("--version=1")],
    ] {
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        let code = cli_main(&bad, &mut out, &mut err);
        assert_eq!(code, 2, "{bad:?}");
        assert!(out.is_empty());
        assert_eq!(String::from_utf8(err).unwrap(), expected_usage);
    }
    for flag in ["-h", "--help", "-V", "--version"] {
        let mut out = FailingWriter;
        let mut err: Vec<u8> = Vec::new();
        let code = cli_main(&[os(flag)], &mut out, &mut err);
        assert_eq!(code, HARNESS_EXIT_CODE, "flag {flag}");
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "mayfly could not write its output\n"
        );
    }
}

#[test]
fn mf14_ro_bind_refuses_bad_cli_syntax_and_bad_sources_and_destinations_before_prepare() {
    ensure_helper();
    let tmp = Tmp::new("mf14-refusals");
    let run_root = tmp.path().join("runs");
    std::fs::create_dir_all(&run_root).unwrap();
    let orphan = plant_sweepable(&run_root, "dddd44444444");
    let real_dir = tmp.path().join("host-dir");
    std::fs::create_dir_all(&real_dir).unwrap();
    let real_file = tmp.path().join("host-file.txt");
    std::fs::write(&real_file, "synthetic input\n").unwrap();
    let dir_str = real_dir.to_string_lossy().into_owned();
    let file_str = real_file.to_string_lossy().into_owned();
    let root_os = run_root.as_os_str().to_os_string();

    // Parse-time CLI refusals at exit 2 with no report printed and zero run directories created.
    let bad_cli_cases: Vec<(Vec<OsString>, &str)> = vec![
        (
            vec![
                os("run"),
                os("--root"),
                root_os.clone(),
                os("--ro-bind"),
                os("--"),
                os("/bin/true"),
            ],
            "mayfly run needs a value after --ro-bind",
        ),
        (
            vec![
                os("run"),
                os("--root"),
                root_os.clone(),
                os("--ro-bind"),
                os(&dir_str),
                os("--"),
                os("/bin/true"),
            ],
            "mayfly run needs --ro-bind in HOST:SANDBOX form",
        ),
        (
            vec![
                os("run"),
                os("--root"),
                root_os.clone(),
                os("--ro-bind"),
                os(":/input"),
                os("--"),
                os("/bin/true"),
            ],
            "mayfly run needs --ro-bind in HOST:SANDBOX form",
        ),
        (
            vec![
                os("run"),
                os("--root"),
                root_os.clone(),
                os("--ro-bind"),
                os(&format!("{dir_str}:")),
                os("--"),
                os("/bin/true"),
            ],
            "mayfly run needs --ro-bind in HOST:SANDBOX form",
        ),
        (
            vec![
                os("run"),
                os("--root"),
                root_os.clone(),
                os("--ro-bind"),
                os(&format!("{dir_str}:relative")),
                os("--"),
                os("/bin/true"),
            ],
            "must start with /",
        ),
        (
            vec![
                os("run"),
                os("--root"),
                root_os.clone(),
                os("--ro-bind"),
                os(&format!("{dir_str}:/input/./a")),
                os("--"),
                os("/bin/true"),
            ],
            "cannot contain dot or parent segments",
        ),
        (
            vec![
                os("run"),
                os("--root"),
                root_os.clone(),
                os("--ro-bind"),
                os(&format!("{dir_str}:/input/../a")),
                os("--"),
                os("/bin/true"),
            ],
            "cannot contain dot or parent segments",
        ),
        (
            vec![
                os("run"),
                os("--root"),
                root_os.clone(),
                os("--ro-bind"),
                os(&format!("{dir_str}:/")),
                os("--"),
                os("/bin/true"),
            ],
            "destination / is reserved",
        ),
        (
            vec![
                os("run"),
                os("--root"),
                root_os.clone(),
                os("--ro-bind"),
                os(&format!("{dir_str}:/work")),
                os("--"),
                os("/bin/true"),
            ],
            "destination /work is reserved",
        ),
        (
            vec![
                os("run"),
                os("--root"),
                root_os.clone(),
                os("--ro-bind"),
                os(&format!("{dir_str}:/inputs")),
                os("--ro-bind"),
                os(&format!("{file_str}:/inputs/")),
                os("--"),
                os("/bin/true"),
            ],
            "named more than once",
        ),
        (
            vec![
                os("run"),
                os("--root"),
                root_os.clone(),
                os("--ro-bind"),
                os(&format!("{dir_str}:/inputs")),
                os("--ro-bind"),
                os(&format!("{file_str}:/inputs/sub")),
                os("--"),
                os("/bin/true"),
            ],
            "overlap",
        ),
        (
            vec![
                os("run"),
                os("--root"),
                root_os.clone(),
                os("--ro-bind"),
                os(&format!("{file_str}:/inputs/sub")),
                os("--ro-bind"),
                os(&format!("{dir_str}:/inputs")),
                os("--"),
                os("/bin/true"),
            ],
            "overlap",
        ),
    ];
    for (args, expected) in bad_cli_cases {
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        let code = cli_main(&args, &mut out, &mut err);
        assert_eq!(code, 2, "{args:?} err {}", String::from_utf8_lossy(&err));
        assert!(out.is_empty());
        assert!(
            String::from_utf8_lossy(&err).contains(expected),
            "expected {expected} in {}",
            String::from_utf8_lossy(&err)
        );
    }

    for reserved in [
        "/.oldroot",
        "/proc",
        "/proc/self",
        "/dev",
        "/dev/null",
        "/sys",
        "/usr",
        "/usr/local",
        "/etc",
        "/etc/hosts",
        "/bin",
        "/sbin",
        "/lib",
        "/lib64",
        "/tmp",
        "/tmp/sub",
    ] {
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        let code = cli_main(
            &[
                os("run"),
                os("--root"),
                root_os.clone(),
                os("--ro-bind"),
                os(&format!("{dir_str}:{reserved}")),
                os("--"),
                os("/bin/true"),
            ],
            &mut out,
            &mut err,
        );
        assert_eq!(code, 2, "reserved {reserved}");
        assert!(out.is_empty());
        assert!(String::from_utf8_lossy(&err).contains("reserved"));
    }
    assert_eq!(glob_run_dirs(&run_root), vec![orphan.clone()]);

    // Pre-prepare NamespaceBackend refusals on missing, symlink, trailing-slash symlink, FIFO, and NUL/structural specs.
    let link_to_dir = tmp.path().join("link-dir");
    std::os::unix::fs::symlink(&real_dir, &link_to_dir).unwrap();
    let link_to_dir_slash = PathBuf::from(format!("{}/", link_to_dir.display()));
    let link_to_dir_dot = PathBuf::from(format!("{}/.", link_to_dir.display()));
    let link_to_dir_dotdot = PathBuf::from(format!("{}/..", link_to_dir.display()));
    let link_to_file = tmp.path().join("link-file");
    std::os::unix::fs::symlink(&real_file, &link_to_file).unwrap();
    let link_to_file_slash = PathBuf::from(format!("{}/", link_to_file.display()));
    let real_file_slash = PathBuf::from(format!("{}/", real_file.display()));
    let fifo_path = tmp.path().join("pipe-src");
    mkfifo(&fifo_path);
    let missing_path = tmp.path().join("does-not-exist");

    let preparations: Arc<Mutex<Vec<PathBuf>>> = Arc::new(Mutex::new(Vec::new()));
    let record = preparations.clone();
    let mut backend = NamespaceBackend::new();
    backend.provenance_writer = Some(Arc::new(move |dir: &Path, _run_id: &str| {
        record.lock().unwrap().push(dir.to_path_buf());
        Ok(())
    }));
    let bad_specs: Vec<(Vec<(PathBuf, PathBuf)>, &str)> = vec![
        (
            vec![(missing_path.clone(), PathBuf::from("/inputs"))],
            "does not exist",
        ),
        (
            vec![(real_file.join("child"), PathBuf::from("/inputs"))],
            "cannot be inspected",
        ),
        (
            vec![(link_to_dir, PathBuf::from("/inputs"))],
            "is a symbolic link",
        ),
        (
            vec![(link_to_dir_slash, PathBuf::from("/inputs"))],
            "is a symbolic link",
        ),
        (
            vec![(link_to_dir_dot, PathBuf::from("/inputs"))],
            "is a symbolic link",
        ),
        (
            vec![(link_to_dir_dotdot, PathBuf::from("/inputs"))],
            "is a symbolic link",
        ),
        (
            vec![(link_to_file, PathBuf::from("/inputs"))],
            "is a symbolic link",
        ),
        (
            vec![(link_to_file_slash, PathBuf::from("/inputs"))],
            "is a symbolic link",
        ),
        (
            vec![(real_file_slash, PathBuf::from("/inputs"))],
            "names a directory form on a non-directory",
        ),
        (
            vec![(fifo_path, PathBuf::from("/inputs"))],
            "not a regular file or directory",
        ),
        (
            vec![(PathBuf::new(), PathBuf::from("/inputs"))],
            "source path is empty",
        ),
        (
            vec![(
                PathBuf::from(OsString::from_vec(b"/tmp/a\0b".to_vec())),
                PathBuf::from("/inputs"),
            )],
            "contains a NUL byte",
        ),
        (
            vec![(
                real_dir.clone(),
                PathBuf::from(OsString::from_vec(b"/inputs\0bad".to_vec())),
            )],
            "contains a NUL byte",
        ),
        (
            vec![(real_dir.clone(), PathBuf::from("rel"))],
            "must start with /",
        ),
    ];
    for (binds, expected) in bad_specs {
        let mut s = RunSpec::new(cmd(&["/bin/true"]), run_root.clone());
        s.ro_binds = binds;
        let rep = backend.run(&s);
        assert_eq!(rep.outcome, Outcome::HarnessError);
        assert_eq!(rep.run_id, "");
        assert_eq!(rep.sandbox_dir, "");
        assert!(rep.timings_ms.prepare.is_none());
        let err_text = rep.error.unwrap();
        assert!(!err_text.starts_with("prepare"), "{err_text}");
        assert!(
            err_text.contains(expected),
            "expected {expected} in {err_text}"
        );
    }
    assert!(preparations.lock().unwrap().is_empty());

    // CLI invocation with a missing host source answers exit 125 with an early HarnessError JSON report and leaves orphan untouched.
    let mut cli_out: Vec<u8> = Vec::new();
    let mut cli_err: Vec<u8> = Vec::new();
    let cli_code = cli_main(
        &[
            os("run"),
            os("--root"),
            root_os,
            os("--ro-bind"),
            os(&format!("{}:/inputs", missing_path.display())),
            os("--"),
            os("/bin/true"),
        ],
        &mut cli_out,
        &mut cli_err,
    );
    assert_eq!(cli_code, HARNESS_EXIT_CODE);
    assert!(cli_err.is_empty());
    let parsed_rep: serde_json::Value = serde_json::from_slice(&cli_out).unwrap();
    assert_eq!(parsed_rep["outcome"], "harness_error");
    assert_eq!(parsed_rep["run_id"], "");
    assert!(parsed_rep["timings_ms"]["prepare"].is_null());
    assert!(parsed_rep["error"]
        .as_str()
        .unwrap()
        .contains("does not exist"));
    assert_eq!(glob_run_dirs(&run_root), vec![orphan.clone()]);
    assert_eq!(
        std::fs::read_to_string(orphan.join(".mayfly-provenance")).unwrap(),
        "dddd44444444\n"
    );
}

#[test]
fn mf14_assemble_root_with_binds_mounts_tmp_tmpfs_and_read_only_binds_in_order() {
    let tmp = Tmp::new("mf14-assemble-order");
    let sandbox = tmp.path().join("run-mf1400000001");
    let rootfs = sandbox.join("rootfs");
    std::fs::create_dir_all(&rootfs).unwrap();
    let host_dir = tmp.path().join("src-dir");
    std::fs::create_dir_all(&host_dir).unwrap();
    let host_file = tmp.path().join("src-file.txt");
    std::fs::write(&host_file, "payload\n").unwrap();

    type MountCall = (Option<String>, String, Option<String>, u64, Option<String>);
    let log: std::cell::RefCell<Vec<MountCall>> = std::cell::RefCell::new(Vec::new());
    let record_mount =
        |src: Option<&str>, target: &str, fstype: Option<&str>, flags: u64| -> Result<(), String> {
            log.borrow_mut().push((
                src.map(str::to_string),
                target.to_string(),
                fstype.map(str::to_string),
                flags,
                None,
            ));
            Ok(())
        };
    let record_mount_opts = |src: Option<&str>,
                             target: &str,
                             fstype: Option<&str>,
                             flags: u64,
                             data: Option<&str>|
     -> Result<(), String> {
        log.borrow_mut().push((
            src.map(str::to_string),
            target.to_string(),
            fstype.map(str::to_string),
            flags,
            data.map(str::to_string),
        ));
        Ok(())
    };

    let binds = vec![
        (
            host_dir.join("..").join("src-dir"),
            PathBuf::from("/inputs/tree"),
        ),
        (
            host_dir.join(".").join("..").join("src-file.txt"),
            PathBuf::from("/work/readonly.cfg"),
        ),
    ];
    let res =
        mayfly::init::assemble_root_with_binds(&rootfs, &binds, &record_mount, &record_mount_opts);
    assert!(res.is_ok(), "{res:?}");
    assert!(rootfs.join("inputs").join("tree").is_dir());
    assert!(rootfs.join("work").join("readonly.cfg").is_file());

    let entries = log.into_inner();
    let work_target = rootfs.join("work").to_string_lossy().into_owned();
    let tmp_target = rootfs.join("tmp").to_string_lossy().into_owned();
    let dir_target = rootfs.join("inputs/tree").to_string_lossy().into_owned();
    let file_target = rootfs
        .join("work/readonly.cfg")
        .to_string_lossy()
        .into_owned();

    let work_idx = entries
        .iter()
        .position(|(src, tgt, fs, _, _)| {
            src.as_deref() == Some("tmpfs") && tgt == &work_target && fs.as_deref() == Some("tmpfs")
        })
        .expect("work tmpfs mount");
    let tmp_idx = entries
        .iter()
        .position(|(src, tgt, fs, flags, data)| {
            src.as_deref() == Some("tmpfs")
                && tgt == &tmp_target
                && fs.as_deref() == Some("tmpfs")
                && *flags == 0
                && data.as_deref() == Some("mode=1777")
        })
        .expect("tmp tmpfs mount with mode=1777");
    let clean_dir = std::path::absolute(host_dir.join("..").join("src-dir")).unwrap();
    let clean_file =
        std::path::absolute(host_dir.join(".").join("..").join("src-file.txt")).unwrap();
    let dir_bind_idx = entries
        .iter()
        .position(|(src, tgt, _, flags, _)| {
            src.as_deref() == Some(clean_dir.to_string_lossy().as_ref())
                && tgt == &dir_target
                && *flags == (mayfly::init::MS_BIND | mayfly::init::MS_REC)
        })
        .expect("directory recursive bind mount");
    let file_bind_idx = entries
        .iter()
        .position(|(src, tgt, _, flags, _)| {
            src.as_deref() == Some(clean_file.to_string_lossy().as_ref())
                && tgt == &file_target
                && *flags == mayfly::init::MS_BIND
        })
        .expect("regular file bind mount");

    assert!(work_idx < tmp_idx);
    assert!(tmp_idx < dir_bind_idx);
    assert!(dir_bind_idx < file_bind_idx);

    // Symlink targets at parent, directory leaf, and file leaf positions as well as pre-existing file targets are refused without touching outside paths.
    let outside_dir = tmp.path().join("outside-dir");
    std::fs::create_dir_all(&outside_dir).unwrap();
    let outside_file = tmp.path().join("outside-file.txt");
    std::fs::write(&outside_file, "untouched\n").unwrap();

    let noop_m =
        |_: Option<&str>, _: &str, _: Option<&str>, _: u64| -> Result<(), String> { Ok(()) };
    let noop_mo = |_: Option<&str>,
                   _: &str,
                   _: Option<&str>,
                   _: u64,
                   _: Option<&str>|
     -> Result<(), String> { Ok(()) };

    // Planted symlink at intermediate parent directory inside rootfs.
    let r_parent = tmp.path().join("run-mf1400000002").join("rootfs");
    std::fs::create_dir_all(&r_parent).unwrap();
    std::os::unix::fs::symlink(&outside_dir, r_parent.join("inputs")).unwrap();
    let err_parent = mayfly::init::assemble_root_with_binds(
        &r_parent,
        &[(host_dir.clone(), PathBuf::from("/inputs/tree"))],
        &noop_m,
        &noop_mo,
    )
    .unwrap_err();
    assert!(err_parent.contains("Errno"), "{err_parent}");
    assert!(!outside_dir.join("tree").exists());

    // Planted symlink at directory target leaf inside rootfs.
    let r_dir_leaf = tmp.path().join("run-mf1400000003").join("rootfs");
    std::fs::create_dir_all(r_dir_leaf.join("inputs")).unwrap();
    std::os::unix::fs::symlink(&outside_dir, r_dir_leaf.join("inputs/tree")).unwrap();
    let err_dir_leaf = mayfly::init::assemble_root_with_binds(
        &r_dir_leaf,
        &[(host_dir.clone(), PathBuf::from("/inputs/tree"))],
        &noop_m,
        &noop_mo,
    )
    .unwrap_err();
    assert!(err_dir_leaf.contains("Errno"), "{err_dir_leaf}");

    // Planted symlink at regular-file target leaf inside rootfs.
    let r_file_leaf = tmp.path().join("run-mf1400000004").join("rootfs");
    std::fs::create_dir_all(r_file_leaf.join("inputs")).unwrap();
    std::os::unix::fs::symlink(&outside_file, r_file_leaf.join("inputs/cfg")).unwrap();
    let err_file_leaf = mayfly::init::assemble_root_with_binds(
        &r_file_leaf,
        &[(host_file.clone(), PathBuf::from("/inputs/cfg"))],
        &noop_m,
        &noop_mo,
    )
    .unwrap_err();
    assert!(err_file_leaf.contains("Errno"), "{err_file_leaf}");
    assert_eq!(
        std::fs::read_to_string(&outside_file).unwrap(),
        "untouched\n"
    );

    // Pre-existing regular file at regular-file target leaf is refused rather than reused.
    let r_preexist = tmp.path().join("run-mf1400000005").join("rootfs");
    std::fs::create_dir_all(r_preexist.join("inputs")).unwrap();
    std::fs::write(r_preexist.join("inputs/cfg"), "planted\n").unwrap();
    let err_preexist = mayfly::init::assemble_root_with_binds(
        &r_preexist,
        &[(host_file.clone(), PathBuf::from("/inputs/cfg"))],
        &noop_m,
        &noop_mo,
    )
    .unwrap_err();
    assert!(err_preexist.contains("Errno"), "{err_preexist}");

    // Trailing-slash directory symlink passed directly to assemble_root_with_binds is refused.
    let sym_dir = tmp.path().join("sym-src-dir");
    std::os::unix::fs::symlink(&host_dir, &sym_dir).unwrap();
    let r_sym_src = tmp.path().join("run-mf1400000006").join("rootfs");
    std::fs::create_dir_all(&r_sym_src).unwrap();
    let err_sym_src = mayfly::init::assemble_root_with_binds(
        &r_sym_src,
        &[(
            PathBuf::from(format!("{}/", sym_dir.display())),
            PathBuf::from("/inputs/tree"),
        )],
        &noop_m,
        &noop_mo,
    )
    .unwrap_err();
    assert!(err_sym_src.contains("symbolic link"), "{err_sym_src}");

    // Sequential option parser on __assemble-root refuses orphan --ro-bind-dst without touching rootfs.
    let r_orphan_opt = tmp.path().join("run-mf1400000007").join("rootfs");
    let genuine = "ab".repeat(16);
    let orphan_out = Command::new(mayfly_bin())
        .arg("__assemble-root")
        .arg("--rootfs")
        .arg(r_orphan_opt.as_os_str())
        .arg("--ro-bind-dst")
        .arg("/inputs")
        .env(mayfly::INTERNAL_TOKEN_ENV, &genuine)
        .output()
        .unwrap();
    assert_eq!(orphan_out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&orphan_out.stderr).contains("unknown assemble option"),
        "{}",
        String::from_utf8_lossy(&orphan_out.stderr)
    );
    assert!(!r_orphan_opt.exists());
}

#[test]
fn mf14_linux_tmp_tmpfs_and_ro_bind_directory_and_file_end_to_end() {
    if !require_sandbox() {
        return;
    }
    let tmp = Tmp::new("mf14-linux-live");
    let run_root = tmp.path().join("runs");
    std::fs::create_dir_all(&run_root).unwrap();

    let host_dir = tmp.path().join("corpus");
    std::fs::create_dir_all(host_dir.join("sub")).unwrap();
    std::fs::write(host_dir.join("sub/item.txt"), "alpha-item\n").unwrap();

    let host_sibling = tmp.path().join("corpus-extra");
    std::fs::create_dir_all(&host_sibling).unwrap();
    std::fs::write(host_sibling.join("extra.txt"), "extra-item\n").unwrap();

    let host_file = tmp.path().join("policy.json");
    std::fs::write(&host_file, "{\"mode\":\"strict\"}\n").unwrap();

    // Non-UTF-8 regular file path alongside a U+FFFD symlink decoy proves byte-exact mounting without lossy substitution.
    let raw_host_file = tmp
        .path()
        .join(std::ffi::OsStr::from_bytes(b"raw_\xff.txt"));
    std::fs::write(&raw_host_file, "raw-bytes-ok\n").unwrap();
    let decoy_target = tmp.path().join("decoy-secret.txt");
    std::fs::write(&decoy_target, "DECOY_LEAKED\n").unwrap();
    let decoy_link = tmp.path().join("raw_\u{fffd}.txt");
    std::os::unix::fs::symlink(&decoy_target, &decoy_link).unwrap();
    let mut raw_bind_arg = raw_host_file.as_os_str().as_bytes().to_vec();
    raw_bind_arg.extend_from_slice(b":/work/ro/raw_\xff.txt");

    let dir_before = snapshot_tree(&host_dir);
    let file_before = std::fs::read(&host_file).unwrap();
    let raw_before = std::fs::read(&raw_host_file).unwrap();
    let mounts_before = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();

    let probe_py = r#"
import errno, json, os, stat
root_st = os.stat('/')
work_st = os.stat('/work')
tmp_st = os.stat('/tmp')
with open('/tmp/ephemeral.txt', 'w') as fh:
    fh.write('scratch\n')
with open('/work/writable.txt', 'w') as fh:
    fh.write('work-ok\n')
item = open('/inputs/sub/item.txt').read().strip()
extra = open('/inputs_extra/extra.txt').read().strip()
policy = open('/work/ro/policy.json').read().strip()
raw_val = open(b'/work/ro/raw_\xff.txt', 'rb').read().decode('utf-8').strip()

def write_errno(path):
    try:
        with open(path, 'wb') as fh:
            fh.write(b'tamper')
        return 0
    except OSError as exc:
        return exc.errno

print(json.dumps({
    'tmp_distinct_root': tmp_st.st_dev != root_st.st_dev,
    'tmp_distinct_work': tmp_st.st_dev != work_st.st_dev,
    'tmp_mode': stat.S_IMODE(tmp_st.st_mode),
    'item': item,
    'extra': extra,
    'policy': policy,
    'raw_val': raw_val,
    'err_dir_new': write_errno(b'/inputs/new.txt'),
    'err_dir_mod': write_errno(b'/inputs/sub/item.txt'),
    'err_file_mod': write_errno(b'/work/ro/policy.json'),
    'err_raw_mod': write_errno(b'/work/ro/raw_\xff.txt'),
}))
"#;

    let mut out: Vec<u8> = Vec::new();
    let mut err: Vec<u8> = Vec::new();
    let code = cli_main(
        &[
            os("run"),
            os("--root"),
            run_root.as_os_str().to_os_string(),
            os("--ro-bind"),
            os(&format!("{}:/inputs", host_dir.display())),
            os("--ro-bind"),
            os(&format!("{}:/inputs_extra", host_sibling.display())),
            os("--ro-bind"),
            os(&format!("{}:/work/ro/policy.json", host_file.display())),
            os("--ro-bind"),
            OsString::from_vec(raw_bind_arg),
            os("--"),
            py(),
            os("-c"),
            os(probe_py),
        ],
        &mut out,
        &mut err,
    );
    assert_eq!(code, 0, "stderr {}", String::from_utf8_lossy(&err));
    assert!(err.is_empty());
    let report: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(report["outcome"], "task");
    assert_eq!(report["sandbox_removed"], true);
    let inner: serde_json::Value =
        serde_json::from_str(report["stdout_tail"].as_str().unwrap().trim()).unwrap();
    assert_eq!(inner["tmp_distinct_root"], true);
    assert_eq!(inner["tmp_distinct_work"], true);
    assert_eq!(inner["tmp_mode"], 0o1777);
    assert_eq!(inner["item"], "alpha-item");
    assert_eq!(inner["extra"], "extra-item");
    assert_eq!(inner["policy"], "{\"mode\":\"strict\"}");
    assert_eq!(inner["raw_val"], "raw-bytes-ok");
    assert_eq!(inner["err_dir_new"], libc::EROFS);
    assert_eq!(inner["err_dir_mod"], libc::EROFS);
    assert_eq!(inner["err_file_mod"], libc::EROFS);
    assert_eq!(inner["err_raw_mod"], libc::EROFS);

    assert_eq!(snapshot_tree(&host_dir), dir_before);
    assert_eq!(std::fs::read(&host_file).unwrap(), file_before);
    assert_eq!(std::fs::read(&raw_host_file).unwrap(), raw_before);
    assert_eq!(
        std::fs::read_to_string(&decoy_target).unwrap(),
        "DECOY_LEAKED\n"
    );
    let mounts_after = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
    assert_eq!(mounts_before, mounts_after);
    assert!(glob_run_dirs(&run_root).is_empty());
}
