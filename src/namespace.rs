//! The namespace backend.
//!
//! One run is one unshare call. The unshare process creates a user
//! namespace with the caller mapped to root, plus mount, pid, uts, and
//! ipc namespaces, and a fresh network namespace unless the run opted
//! in to the host network. Inside, the mayfly init assembles a
//! prepared root, pivots into it, and supervises one task. The runner
//! side here owns the phases outside the sandbox, prepare, start,
//! task, and teardown, and times the start and task phases from the
//! monotonic stamps the init puts on its own lifecycle events.
//!
//! The unshare process is spawned with a parent death signal, so a
//! supervisor killed outright takes the sandbox tree down with it,
//! and the init arms the same signal against unshare. A janitor
//! process watches each run and removes the sandbox directory when
//! the supervisor dies without removing it, and it trusts only a
//! clean marker carrying the per run token it was handed over its
//! standard input at spawn. A run also sweeps orphaned sandbox
//! directories left under its root by runs whose supervisor and
//! janitor both died, before it prepares its own. A supervisor
//! interrupted by SIGTERM or SIGINT still tears the sandbox down
//! and reports the run as a harness error.
//!
//! This backend is a Linux namespace sandbox. It is not a micro-VM
//! and nothing in this repo calls it one.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{FromRawFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::{
    helper_exe, ms_since, now, py_os_error, py_os_error_path, round3, strerror, sweep_orphans,
    write_provenance, RunReport, RunSpec, SignalGuard, TailBuffer, Timings,
};

pub type ProvenanceWriter = dyn Fn(&Path, &str) -> io::Result<()> + Send + Sync;

pub struct NamespaceBackend {
    pub unshare_path: Option<PathBuf>,
    /// A fixed run id in place of a fresh random one. Library
    /// callers leave this empty. Tests set it to plant a run at a
    /// known id against a standing directory.
    pub run_id_override: Option<String>,
    /// The provenance writer seam. The default writes through
    /// crate::write_provenance. Tests substitute a writer that
    /// hangs or fails to drive the prepare failure shapes.
    pub provenance_writer: Option<Arc<ProvenanceWriter>>,
}

struct JanitorHandle {
    child: Child,
    write_fd: RawFd,
}

fn pipe2_cloexec() -> io::Result<(RawFd, RawFd)> {
    let mut fds: [RawFd; 2] = [0, 0];
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((fds[0], fds[1]))
}

fn clear_cloexec(fd: RawFd) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
        }
    }
}

fn close_fd(fd: RawFd) {
    unsafe {
        libc::close(fd);
    }
}

fn which(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let path = PathBuf::from(program);
        return if is_executable(&path) {
            Some(path)
        } else {
            None
        };
    }
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(program);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{:02x}", byte));
    }
    out
}

fn new_run_id() -> String {
    let mut bytes = [0u8; 6];
    let _ = getrandom::fill(&mut bytes);
    hex_bytes(&bytes)
}

fn new_token() -> String {
    let mut bytes = [0u8; 16];
    let _ = getrandom::fill(&mut bytes);
    hex_bytes(&bytes)
}

/// Leave the clean marker carrying this run's token.
///
/// A non regular object planted at the marker path is
/// unlinked first, because opening a planted pipe for writing
/// would block the finish past the end of the task. The
/// planted object itself is all the unlink ever touches, the
/// file a planted symlink names is never opened or removed.
/// Where the planted object cannot be removed, the write is
/// skipped and the run still finishes cleanly, and the janitor
/// completes the cleanup in the runner's place when it sees
/// its supervisor gone with no valid marker, removing a
/// sandbox that is already gone as a no-op. The write itself
/// cannot block either, so a pipe swapped into the path after
/// the check fails the open instead of hanging on it.
fn write_clean_marker(marker: &Path, token: &str) {
    use std::os::unix::fs::OpenOptionsExt;
    if let Ok(meta) = std::fs::symlink_metadata(marker) {
        if !meta.file_type().is_file() && std::fs::remove_file(marker).is_err() {
            return;
        }
    }
    let opened = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_NONBLOCK)
        .mode(0o644)
        .open(marker);
    if let Ok(mut file) = opened {
        let _ = file.write_all(format!("{}\n", token).as_bytes());
    }
}

impl Default for NamespaceBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl NamespaceBackend {
    pub fn new() -> NamespaceBackend {
        NamespaceBackend {
            unshare_path: which("unshare"),
            run_id_override: None,
            provenance_writer: None,
        }
    }

    pub fn with_unshare_path(unshare_path: Option<PathBuf>) -> NamespaceBackend {
        NamespaceBackend {
            unshare_path,
            run_id_override: None,
            provenance_writer: None,
        }
    }

    pub fn name(&self) -> &'static str {
        "namespace"
    }

    fn task_env(&self, spec: &RunSpec) -> Vec<(OsString, OsString)> {
        let source: Vec<(OsString, OsString)> = if spec.caller_env.is_empty() {
            crate::current_env()
        } else {
            spec.caller_env.clone()
        };
        let mut env: Vec<(OsString, OsString)> = Vec::new();
        for key in &spec.allow_env {
            if let Some(value) = crate::env_lookup(&source, key) {
                env.push((key.clone(), value.to_os_string()));
            }
        }
        env.push((
            OsString::from("MAYFLY_SANDBOX"),
            OsString::from(self.name()),
        ));
        // The init and the task are Python processes in the
        // reference drives, and Python coerces a C locale by
        // injecting LC_CTYPE into its own environment. The
        // coercion guard is a harness key of the shared
        // environment itself, named in the contract, and it
        // keeps every interpreter in the run from adding
        // variables the scrub did not produce. A caller who
        // allowlists the key keeps the caller value in its place.
        if crate::env_lookup(&env, OsString::from("PYTHONCOERCECLOCALE").as_os_str()).is_none() {
            env.push((OsString::from("PYTHONCOERCECLOCALE"), OsString::from("0")));
        }
        env
    }

    fn unshare_argv(
        &self,
        spec: &RunSpec,
        sandbox_dir: &Path,
        status_fd: RawFd,
    ) -> (OsString, Vec<OsString>) {
        let mut argv: Vec<OsString> = Vec::new();
        for flag in [
            "--user",
            "--map-root-user",
            "--mount",
            "--pid",
            "--uts",
            "--ipc",
            "--fork",
        ] {
            argv.push(OsString::from(flag));
        }
        if !spec.network {
            argv.push(OsString::from("--net"));
        }
        argv.push(helper_exe().into_os_string());
        argv.push(OsString::from("__init"));
        argv.push(OsString::from("--rootfs"));
        argv.push(sandbox_dir.join("rootfs").into_os_string());
        argv.push(OsString::from("--status-fd"));
        argv.push(OsString::from(status_fd.to_string()));
        argv.push(OsString::from("--"));
        for arg in &spec.command {
            argv.push(arg.clone());
        }
        (
            self.unshare_path
                .clone()
                .unwrap_or_default()
                .into_os_string(),
            argv,
        )
    }

    fn start_janitor(
        &self,
        sandbox_dir: &Path,
        marker: &Path,
        token: &str,
    ) -> Option<JanitorHandle> {
        let (read_fd, write_fd) = pipe2_cloexec().ok()?;
        let mut cmd = Command::new(helper_exe());
        cmd.arg("__janitor")
            .arg("--sandbox")
            .arg(sandbox_dir.as_os_str())
            .arg("--marker")
            .arg(marker.as_os_str())
            .arg("--fd")
            .arg(read_fd.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            cmd.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                clear_cloexec(read_fd);
                Ok(())
            });
        }
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(_) => {
                close_fd(read_fd);
                close_fd(write_fd);
                return None;
            }
        };
        close_fd(read_fd);
        // The token reaches the janitor over its standard input,
        // never over its command line, so neither a directory
        // listing nor a process listing reveals it. A janitor that
        // never receives it authenticates nothing and cleans up.
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(format!("{}\n", token).as_bytes());
            drop(stdin);
        }
        Some(JanitorHandle { child, write_fd })
    }

    fn teardown(
        &self,
        sandbox_dir: &Path,
        report: &mut RunReport,
        timings: &mut Timings,
        since: Option<f64>,
    ) {
        let phase = since.unwrap_or_else(now);
        let _ = std::fs::remove_dir_all(sandbox_dir);
        report.sandbox_removed = Some(!sandbox_dir.exists());
        timings.teardown = Some(round3((now() - phase) * 1000.0));
    }

    pub fn run(&self, spec: &RunSpec) -> RunReport {
        sweep_orphans(&spec.root);
        let mut timings = Timings::default();
        let run_id = self.run_id_override.clone().unwrap_or_else(new_run_id);
        let sandbox_dir = spec.root.join(format!("run-{}", run_id));
        let marker = spec.root.join(format!(".mayfly-clean-{}", run_id));
        let mut report = RunReport::new(
            self.name(),
            &run_id,
            &sandbox_dir.to_string_lossy(),
            spec.command.clone(),
            "harness_error",
        );
        let wall = now();
        let guard = SignalGuard::install();
        self.run_inner(
            spec,
            &sandbox_dir,
            &mut report,
            &mut timings,
            &marker,
            &guard,
            &run_id,
        );
        timings.total = Some(ms_since(wall));
        report.timings_ms = timings;
        if report.proc_mounted.is_none() {
            report.proc_mounted = Some(false);
        }
        if report.sandbox_removed.is_none() {
            report.sandbox_removed = Some(!sandbox_dir.exists());
        }
        report
    }

    #[allow(clippy::too_many_arguments)]
    fn run_inner(
        &self,
        spec: &RunSpec,
        sandbox_dir: &Path,
        report: &mut RunReport,
        timings: &mut Timings,
        marker: &Path,
        guard: &SignalGuard,
        run_id: &str,
    ) {
        let phase = now();
        if let Err(err) = self.create_run_dir(sandbox_dir) {
            // The run directory is created as its own first
            // step, apart from everything prepared inside it,
            // so a failure here ends the run before it owns
            // anything. Where the directory already stands,
            // planted in the run root or met by the rare
            // collision on a run id, it is not this run's to
            // remove, and no teardown runs against it. The run
            // reports the failure and leaves the standing tree
            // exactly as it found it.
            report.error = Some(format!(
                "prepare failed, {}",
                py_os_error_path(&err, sandbox_dir)
            ));
            timings.prepare = Some(ms_since(phase));
            return;
        }
        if let Err(err) = std::fs::create_dir(sandbox_dir.join("rootfs")) {
            report.error = Some(format!(
                "prepare failed, {}",
                py_os_error_path(&err, &sandbox_dir.join("rootfs"))
            ));
            timings.prepare = Some(ms_since(phase));
            self.teardown(sandbox_dir, report, timings, None);
            return;
        }
        // The janitor is spawned here, as soon as the run
        // directory exists and before the born provenance
        // write, so a supervisor killed inside that write
        // already has a janitor watching the directory it
        // leaves behind. The janitor sleeps on its watch pipe
        // with no deadline of its own, so the earlier spawn
        // only lengthens its wait and its protocol is
        // unchanged, and the cleanup after the run body below
        // now also stands the janitor down on the prepare failure
        // and the missing unshare paths, where no janitor
        // existed before. What no cleanup layer covers is a
        // death before the born provenance write below lands,
        // whether it falls in the segment between the
        // directory's creation above and this spawn or takes
        // the janitor with the supervisor in a double kill
        // after it, and MF-12 names what such a death can
        // leave standing.
        let token = new_token();
        let janitor = self.start_janitor(sandbox_dir, marker, &token);
        self.run_body(spec, sandbox_dir, report, timings, guard, run_id, phase);
        if let Some(mut janitor) = janitor {
            let clean = report.sandbox_removed == Some(true);
            if clean {
                write_clean_marker(marker, &token);
            }
            close_fd(janitor.write_fd);
            if clean {
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                loop {
                    match janitor.child.try_wait() {
                        Ok(Some(_)) => break,
                        Ok(None) => {
                            if std::time::Instant::now() >= deadline {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
            }
        }
    }

    fn create_run_dir(&self, sandbox_dir: &Path) -> io::Result<()> {
        match std::fs::create_dir(sandbox_dir) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                // The reference harness creates missing parents
                // first. Where the nearest standing ancestor is a
                // file rather than a directory, its mkdir failed
                // with ENOTDIR naming the sandbox path, and so
                // does this one.
                let mut ancestor = sandbox_dir.parent();
                while let Some(path) = ancestor {
                    if path.exists() {
                        if !path.is_dir() {
                            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
                        }
                        break;
                    }
                    ancestor = path.parent();
                }
                if let Some(parent) = sandbox_dir.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::create_dir(sandbox_dir)
            }
            Err(err) => Err(err),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run_body(
        &self,
        spec: &RunSpec,
        sandbox_dir: &Path,
        report: &mut RunReport,
        timings: &mut Timings,
        guard: &SignalGuard,
        run_id: &str,
        phase: f64,
    ) {
        let writer_result = match &self.provenance_writer {
            Some(writer) => writer(sandbox_dir, run_id),
            None => write_provenance(sandbox_dir, run_id),
        };
        if let Err(err) = writer_result {
            report.error = Some(format!(
                "prepare failed, {}",
                py_os_error_path(&err, &sandbox_dir.join(crate::PROVENANCE_NAME))
            ));
            timings.prepare = Some(ms_since(phase));
            self.teardown(sandbox_dir, report, timings, None);
            return;
        }
        timings.prepare = Some(ms_since(phase));
        if self.unshare_path.is_none() {
            report.error = Some("unshare is not available on this machine".to_string());
            self.teardown(sandbox_dir, report, timings, None);
            return;
        }
        self.run_supervised(spec, sandbox_dir, report, timings, guard);
    }

    fn run_supervised(
        &self,
        spec: &RunSpec,
        sandbox_dir: &Path,
        report: &mut RunReport,
        timings: &mut Timings,
        guard: &SignalGuard,
    ) {
        let (status_r, status_w) = match pipe2_cloexec() {
            Ok(pair) => pair,
            Err(err) => {
                report.error = Some(format!("could not launch unshare, {}", py_os_error(&err)));
                self.teardown(sandbox_dir, report, timings, None);
                return;
            }
        };
        let (program, argv) = self.unshare_argv(spec, sandbox_dir, status_w);
        // An argument carrying a NUL byte can never reach an
        // exec, and the Python harness refused it at spawn with
        // this wording, before anything was launched.
        let has_nul =
            program.as_bytes().contains(&0) || argv.iter().any(|arg| arg.as_bytes().contains(&0));
        if has_nul {
            close_fd(status_r);
            close_fd(status_w);
            report.error = Some("could not launch unshare, embedded null byte".to_string());
            self.teardown(sandbox_dir, report, timings, None);
            return;
        }
        let env = self.task_env(spec);
        let pass_fds: Vec<RawFd> = spec.pass_fds.clone();
        let mut cmd = Command::new(&program);
        cmd.args(&argv)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k.as_os_str(), v.as_os_str())));
        unsafe {
            cmd.pre_exec(move || {
                libc::prctl(1, 9, 0, 0, 0);
                clear_cloexec(status_w);
                for fd in &pass_fds {
                    clear_cloexec(*fd);
                }
                Ok(())
            });
        }
        let spawn = now();
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(err) => {
                close_fd(status_r);
                close_fd(status_w);
                report.error = Some(format!(
                    "could not launch unshare, {}",
                    spawn_error_text(&err, &program)
                ));
                self.teardown(sandbox_dir, report, timings, None);
                return;
            }
        };
        close_fd(status_w);
        let events: Arc<Mutex<Vec<(f64, serde_json::Value)>>> = Arc::new(Mutex::new(Vec::new()));
        let (status_done_tx, status_done_rx) = std::sync::mpsc::channel::<()>();
        {
            let events = Arc::clone(&events);
            std::thread::spawn(move || {
                drain_status(status_r, &events);
                let _ = status_done_tx.send(());
            });
        }
        let out_tail = Arc::new(TailBuffer::new());
        let err_tail = Arc::new(TailBuffer::new());
        let (out_done_tx, out_done_rx) = std::sync::mpsc::channel::<()>();
        if let Some(stdout) = child.stdout.take() {
            let out_tail = Arc::clone(&out_tail);
            std::thread::spawn(move || {
                drain_stream(stdout, &out_tail);
                let _ = out_done_tx.send(());
            });
        } else {
            let _ = out_done_tx.send(());
        }
        let (err_done_tx, err_done_rx) = std::sync::mpsc::channel::<()>();
        if let Some(stderr) = child.stderr.take() {
            let err_tail = Arc::clone(&err_tail);
            std::thread::spawn(move || {
                drain_stream(stderr, &err_tail);
                let _ = err_done_tx.send(());
            });
        } else {
            let _ = err_done_tx.send(());
        }
        let mut status = None;
        while status.is_none() {
            match child.try_wait() {
                Ok(Some(exit)) => status = Some(exit),
                Ok(None) => {
                    if guard.interrupted() {
                        let _ = child.kill();
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(_) => break,
            }
        }
        let status = match status {
            Some(exit) => exit,
            None => match child.wait() {
                Ok(exit) => exit,
                Err(_) => {
                    self.teardown(sandbox_dir, report, timings, None);
                    return;
                }
            },
        };
        let returncode = match status.code() {
            Some(code) => code,
            None => -status.signal().unwrap_or(0),
        };
        let _ = out_done_rx.recv_timeout(Duration::from_secs(5));
        let _ = err_done_rx.recv_timeout(Duration::from_secs(5));
        if status_done_rx.recv_timeout(Duration::from_secs(5)).is_err() {
            close_fd(status_r);
            let _ = status_done_rx.recv_timeout(Duration::from_secs(5));
        }
        report.stdout_tail = out_tail.text();
        report.stderr_tail = err_tail.text();

        let events_snapshot = events.lock().unwrap().clone();
        let mut by_name: HashMap<String, (f64, serde_json::Value)> = HashMap::new();
        for (receipt_ts, event) in &events_snapshot {
            if let Some(name) = event.get("event").and_then(|v| v.as_str()) {
                by_name
                    .entry(name.to_string())
                    .or_insert_with(|| (*receipt_ts, event.clone()));
            }
        }
        if let Some((_, ready)) = by_name.get("ready") {
            report.proc_mounted = Some(
                ready
                    .get("proc_mounted")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            );
        }
        let mut started_ts: Option<f64> = None;
        if let Some((receipt, started)) = by_name.get("started") {
            let ts = started
                .get("ts")
                .and_then(|v| v.as_f64())
                .unwrap_or(*receipt);
            started_ts = Some(ts);
            timings.start = Some(round3((ts - spawn).max(0.0) * 1000.0));
        }
        let mut ended_ts: Option<f64> = None;
        if let Some((receipt, done)) = by_name.get("task_done") {
            if started_ts.is_none() {
                started_ts = Some(spawn);
            }
            let done_ts = done.get("ts").and_then(|v| v.as_f64()).unwrap_or(*receipt);
            ended_ts = Some(done_ts);
            timings.task = Some(round3((done_ts - started_ts.unwrap()).max(0.0) * 1000.0));
            // An interrupt received before the task done stamp
            // makes the run a harness error even where the task
            // done event reaches the runner only after the sandbox
            // has been killed, and an interrupt received after the
            // stamp leaves the task outcome standing. The stamps
            // are compared, not the order the runner drained the
            // events in, and MF-4 carries the rule.
            let sig_ts = guard.first_time();
            if guard.interrupted() && (sig_ts.is_none() || sig_ts.unwrap() <= done_ts) {
                report.error = Some(format!(
                    "the run was interrupted by signal {} and the sandbox was torn down",
                    guard.first().unwrap_or(0)
                ));
            } else {
                report.outcome = "task".to_string();
                if let Some(signal) = done.get("signal").and_then(|v| v.as_i64()) {
                    report.task_signal = Some(signal as i32);
                } else if let Some(code) = done.get("exit_code").and_then(|v| v.as_i64()) {
                    report.task_exit_code = Some(code as i32);
                }
            }
        } else if let Some((receipt, error_event)) = by_name.get("start_error") {
            report.error = Some(
                error_event
                    .get("detail")
                    .and_then(|v| v.as_str())
                    .unwrap_or("the sandbox failed to start")
                    .to_string(),
            );
            ended_ts = Some(
                error_event
                    .get("ts")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(*receipt),
            );
        } else if guard.interrupted() {
            report.error = Some(format!(
                "the run was interrupted by signal {} and the sandbox was torn down",
                guard.first().unwrap_or(0)
            ));
        } else {
            let mut detail = if by_name.contains_key("started") {
                "the sandbox exited after the task started, and no task end was reported"
                    .to_string()
            } else {
                "the sandbox exited without starting the task".to_string()
            };
            if returncode != 0 {
                detail.push_str(&format!(", unshare exit {}", returncode));
            }
            let trimmed = report.stderr_tail.trim();
            if !trimmed.is_empty() {
                let tail: String = {
                    let chars: Vec<char> = trimmed.chars().collect();
                    let start = chars.len().saturating_sub(300);
                    chars[start..].iter().collect()
                };
                detail.push_str(&format!(", sandbox stderr tail, {}", tail));
            }
            report.error = Some(detail);
        }

        self.teardown(sandbox_dir, report, timings, ended_ts);
        if report.sandbox_removed != Some(true) {
            if report.outcome == "task" {
                report.outcome = "harness_error".to_string();
                report.task_exit_code = None;
                report.task_signal = None;
                report.error =
                    Some("the sandbox directory could not be removed after the run".to_string());
            } else if let Some(error) = &report.error {
                report.error = Some(format!(
                    "{}, and the sandbox directory could not be removed",
                    error
                ));
            } else {
                report.error =
                    Some("the sandbox directory could not be removed after the run".to_string());
            }
        }
    }
}

fn spawn_error_text(err: &io::Error, program: &OsString) -> String {
    match err.raw_os_error() {
        Some(errno) => format!(
            "[Errno {}] {}: '{}'",
            errno,
            strerror(errno),
            program.to_string_lossy()
        ),
        None => err.to_string(),
    }
}

fn drain_status(fd: RawFd, events: &Arc<Mutex<Vec<(f64, serde_json::Value)>>>) {
    let mut stream = unsafe { std::fs::File::from_raw_fd(fd) };
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                pending.extend_from_slice(&chunk[..n]);
                while let Some(pos) = pending.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = pending.drain(..=pos).collect();
                    push_event_line(&line, events);
                }
            }
            Err(_) => break,
        }
    }
    if !pending.is_empty() {
        push_event_line(&pending, events);
    }
}

fn push_event_line(line: &[u8], events: &Arc<Mutex<Vec<(f64, serde_json::Value)>>>) {
    let text = String::from_utf8_lossy(line);
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text.trim()) {
        events.lock().unwrap().push((now(), value));
    }
}

fn drain_stream<R: Read>(mut stream: R, tail: &Arc<TailBuffer>) {
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => tail.feed(&chunk[..n]),
            Err(_) => break,
        }
    }
}
