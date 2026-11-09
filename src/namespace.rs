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
    write_provenance, Outcome, RunReport, RunSpec, SignalGuard, TailBuffer, Timings,
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
    // The array is owned by this call and holds exactly the two
    // descriptors pipe2 promises to write, and nothing reads
    // them back unless the call reports success.
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((fds[0], fds[1]))
}

fn clear_cloexec(fd: RawFd) {
    // The descriptor is one the caller owns and holds open for
    // the child being spawned, and fcntl only reads and rewrites
    // that descriptor's own flags.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
        }
    }
}

fn close_fd(fd: RawFd) {
    // The descriptor is owned by the caller and closed exactly
    // once on each path that reaches here.
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

/// Draw bytes from the system random source, carrying a
/// failure as the io error the callers report.
fn fill_random(bytes: &mut [u8]) -> io::Result<()> {
    getrandom::fill(bytes).map_err(|err| match err.raw_os_error() {
        Some(code) => io::Error::from_raw_os_error(code),
        None => io::Error::other("the system random source failed"),
    })
}

/// A fresh run id from the system random source. A failure of
/// that source is an error the caller reports, never a zeroed
/// buffer quietly standing in for an id, because two runs
/// handed the same zeroed id would collide on one directory
/// and a zeroed token would authenticate a planted marker.
fn new_run_id() -> io::Result<String> {
    let mut bytes = [0u8; 6];
    fill_random(&mut bytes)?;
    Ok(hex_bytes(&bytes))
}

/// A fresh per run token from the same source, under the same
/// rule. The token is the only thing that authenticates a
/// clean marker to the janitor, so a token that was never
/// random must never be issued at all.
fn new_token() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    fill_random(&mut bytes)?;
    Ok(hex_bytes(&bytes))
}

/// Leave the clean marker carrying this run's token.
///
/// A non regular object planted at the marker path is
/// unlinked first, because opening a planted pipe for writing
/// would block the finish past the end of the task. The
/// planted object itself is all the unlink ever touches, the
/// file a planted symlink names is never opened or removed.
/// A symlink planted at the path is not unlinked either. It
/// is left standing and the write is skipped, because the
/// open refuses to follow it, so nothing at the path can make
/// the write land on a file outside the run root, and the
/// janitor removes the link when it completes the cleanup in
/// the runner's place. Where the planted object cannot be
/// removed, the write is skipped and the run still finishes
/// cleanly, and the janitor completes the cleanup in the
/// runner's place when it sees its supervisor gone with no
/// valid marker, removing a sandbox that is already gone as a
/// no-op. The write itself cannot block either, so a pipe
/// swapped into the path after the check fails the open
/// instead of hanging on it.
pub fn write_clean_marker(marker: &Path, token: &str) {
    use std::os::unix::fs::OpenOptionsExt;
    if let Ok(meta) = std::fs::symlink_metadata(marker) {
        let file_type = meta.file_type();
        if file_type.is_symlink() {
            return;
        }
        if !file_type.is_file() && std::fs::remove_file(marker).is_err() {
            return;
        }
    }
    let opened = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
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

/// Everything one run hands down its own pipeline, gathered
/// into one value so the phases pass the run between them
/// instead of threading the same parameters through every call.
struct RunContext<'a> {
    spec: &'a RunSpec,
    sandbox_dir: &'a Path,
    report: &'a mut RunReport,
    timings: &'a mut Timings,
    marker: &'a Path,
    guard: &'a SignalGuard,
    run_id: &'a str,
    token: &'a str,
    /// The stamp the prepare phase is timed from.
    phase: f64,
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
        // The two ceilings of MF-13 ride the argument vector
        // the harness alone builds, after the status descriptor
        // option and before the separator, and only when set.
        // They never travel through the environment, because the
        // environment is the caller's scrubbed surface and a
        // bound carried there would stand where the task itself
        // could read it. The timeout never travels, because the
        // supervisor that enforces it sits on the host.
        if let Some(seconds) = spec.max_cpu_seconds {
            argv.push(OsString::from("--max-cpu"));
            argv.push(OsString::from(seconds.to_string()));
        }
        if let Some(bytes) = spec.max_memory_bytes {
            argv.push(OsString::from("--max-memory"));
            argv.push(OsString::from(bytes.to_string()));
        }
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
        // The closure runs in the forked child between fork and
        // exec, where it only calls async signal safe functions
        // on the descriptor it was handed, which the parent
        // created for exactly this child and no one else holds.
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
        let wall = now();
        // The floors of MF-13 are enforced here, in one place
        // for both surfaces, ahead of the orphan sweep and
        // ahead of anything created under the root, so a bound
        // under its floor prepares nothing at all. The command
        // line refuses the same values at parse time in its own
        // class and never reaches this answer.
        if let Some(error) = floor_breach(spec) {
            let mut report = RunReport::new(
                self.name(),
                "",
                "",
                spec.command.clone(),
                Outcome::HarnessError,
            );
            report.error = Some(error);
            report.proc_mounted = Some(false);
            report.sandbox_removed = Some(true);
            report.timings_ms = Timings {
                total: Some(ms_since(wall)),
                ..Timings::default()
            };
            return report;
        }
        sweep_orphans(&spec.root);
        let mut timings = Timings::default();
        // The run id and the token are drawn before anything is
        // created under the root, so a failure of the random
        // source ends the run as a preparation failure with no
        // directory standing, and no id or token is ever issued
        // from a buffer the source never filled.
        let drawn = match &self.run_id_override {
            Some(id) => new_token().map(|token| (id.clone(), token)),
            None => new_run_id().and_then(|id| new_token().map(|token| (id, token))),
        };
        let (run_id, token) = match drawn {
            Ok(pair) => pair,
            Err(err) => {
                let mut report = RunReport::new(
                    self.name(),
                    "",
                    "",
                    spec.command.clone(),
                    Outcome::HarnessError,
                );
                report.error = Some(format!("prepare failed, {}", py_os_error(&err)));
                report.proc_mounted = Some(false);
                report.sandbox_removed = Some(true);
                timings.total = Some(ms_since(wall));
                report.timings_ms = timings;
                return report;
            }
        };
        let sandbox_dir = spec.root.join(format!("run-{}", run_id));
        let marker = spec.root.join(format!(".mayfly-clean-{}", run_id));
        let mut report = RunReport::new(
            self.name(),
            &run_id,
            &sandbox_dir.to_string_lossy(),
            spec.command.clone(),
            Outcome::HarnessError,
        );
        let guard = SignalGuard::install();
        self.run_inner(&mut RunContext {
            spec,
            sandbox_dir: &sandbox_dir,
            report: &mut report,
            timings: &mut timings,
            marker: &marker,
            guard: &guard,
            run_id: &run_id,
            token: &token,
            phase: now(),
        });
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

    fn run_inner(&self, ctx: &mut RunContext<'_>) {
        if let Err(err) = self.create_run_dir(ctx.sandbox_dir) {
            // The run directory is created as its own first
            // step, apart from everything prepared inside it,
            // so a failure here ends the run before it owns
            // anything. Where the directory already stands,
            // planted in the run root or met by the rare
            // collision on a run id, it is not this run's to
            // remove, and no teardown runs against it. The run
            // reports the failure and leaves the standing tree
            // exactly as it found it.
            ctx.report.error = Some(format!(
                "prepare failed, {}",
                py_os_error_path(&err, ctx.sandbox_dir)
            ));
            ctx.timings.prepare = Some(ms_since(ctx.phase));
            return;
        }
        if let Err(err) = std::fs::create_dir(ctx.sandbox_dir.join("rootfs")) {
            ctx.report.error = Some(format!(
                "prepare failed, {}",
                py_os_error_path(&err, &ctx.sandbox_dir.join("rootfs"))
            ));
            ctx.timings.prepare = Some(ms_since(ctx.phase));
            self.teardown(ctx.sandbox_dir, ctx.report, ctx.timings, None);
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
        let janitor = self.start_janitor(ctx.sandbox_dir, ctx.marker, ctx.token);
        self.run_body(ctx);
        if let Some(janitor) = janitor {
            let clean = ctx.report.sandbox_removed == Some(true);
            if clean {
                write_clean_marker(ctx.marker, ctx.token);
            }
            close_fd(janitor.write_fd);
            let mut child = janitor.child;
            if clean {
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                loop {
                    match child.try_wait() {
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
            } else {
                // The sandbox still stands, so the janitor has
                // its removal and recreation watch ahead of it,
                // work that runs to its own retry and watch
                // windows. The run does not block on it, and a
                // detached waiter reaps the janitor when it
                // exits, so a finished run never leaves the
                // janitor behind as a zombie.
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
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

    fn run_body(&self, ctx: &mut RunContext<'_>) {
        let writer_result = match &self.provenance_writer {
            Some(writer) => writer(ctx.sandbox_dir, ctx.run_id),
            None => write_provenance(ctx.sandbox_dir, ctx.run_id),
        };
        if let Err(err) = writer_result {
            ctx.report.error = Some(format!(
                "prepare failed, {}",
                py_os_error_path(&err, &ctx.sandbox_dir.join(crate::PROVENANCE_NAME))
            ));
            ctx.timings.prepare = Some(ms_since(ctx.phase));
            self.teardown(ctx.sandbox_dir, ctx.report, ctx.timings, None);
            return;
        }
        ctx.timings.prepare = Some(ms_since(ctx.phase));
        if self.unshare_path.is_none() {
            ctx.report.error = Some("unshare is not available on this machine".to_string());
            self.teardown(ctx.sandbox_dir, ctx.report, ctx.timings, None);
            return;
        }
        self.run_supervised(ctx);
    }

    fn run_supervised(&self, ctx: &mut RunContext<'_>) {
        let (status_r, status_w) = match pipe2_cloexec() {
            Ok(pair) => pair,
            Err(err) => {
                ctx.report.error = Some(format!("could not launch unshare, {}", py_os_error(&err)));
                self.teardown(ctx.sandbox_dir, ctx.report, ctx.timings, None);
                return;
            }
        };
        let (program, argv) = self.unshare_argv(ctx.spec, ctx.sandbox_dir, status_w);
        // An argument carrying a NUL byte can never reach an
        // exec, and the Python harness refused it at spawn with
        // this wording, before anything was launched.
        let has_nul =
            program.as_bytes().contains(&0) || argv.iter().any(|arg| arg.as_bytes().contains(&0));
        if has_nul {
            close_fd(status_r);
            close_fd(status_w);
            ctx.report.error = Some("could not launch unshare, embedded null byte".to_string());
            self.teardown(ctx.sandbox_dir, ctx.report, ctx.timings, None);
            return;
        }
        let mut env = self.task_env(ctx.spec);
        // The internal token rides to the unshare process and
        // through it to the init, so the hidden entry points can
        // tell the run's own children from a direct caller. The
        // init removes the key from its own environment before
        // it supervises the task, so the token stands in no
        // environment the task can read, and the scrub that
        // built the rest of this environment never admits it.
        env.push((
            OsString::from(crate::INTERNAL_TOKEN_ENV),
            OsString::from(ctx.token),
        ));
        let pass_fds: Vec<RawFd> = ctx.spec.pass_fds.clone();
        let mut cmd = Command::new(&program);
        cmd.args(&argv)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k.as_os_str(), v.as_os_str())));
        // The closure runs in the forked child between fork and
        // exec, where it only arms the parent death signal and
        // clears the close on exec flag on descriptors this call
        // created or the caller lent for the child, all async
        // signal safe operations on owned descriptors.
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
                ctx.report.error = Some(format!(
                    "could not launch unshare, {}",
                    spawn_error_text(&err, &program)
                ));
                self.teardown(ctx.sandbox_dir, ctx.report, ctx.timings, None);
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
        // The deadline the timeout of MF-13 sets, read from the
        // started stamp the status drain is filling, on the one
        // monotonic clock every stamp in the run shares. Until
        // the started event stands no deadline runs, so the
        // timeout bounds the task phase alone. Where the
        // deadline passes with the task still running, the
        // unshare child is killed through the same kill the
        // interrupt path makes on the same handle, and the
        // outcome is read from the stamps below rather than
        // from the kill itself.
        let mut deadline: Option<f64> = None;
        while status.is_none() {
            match child.try_wait() {
                Ok(Some(exit)) => status = Some(exit),
                Ok(None) => {
                    if ctx.guard.interrupted() {
                        let _ = child.kill();
                        break;
                    }
                    if let Some(timeout) = ctx.spec.timeout_seconds {
                        if deadline.is_none() {
                            deadline = started_stamp(&events).map(|ts| ts + timeout as f64);
                        }
                        if let Some(deadline_ts) = deadline {
                            if now() > deadline_ts {
                                let _ = child.kill();
                                break;
                            }
                        }
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
                    self.teardown(ctx.sandbox_dir, ctx.report, ctx.timings, None);
                    return;
                }
            },
        };
        // The moment the supervised wait ended, taken before
        // the drain waits below, so the deadline comparison in
        // the outcome reading is made against the end of the
        // run and not against whenever the drains finished.
        let wait_ended = now();
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
        ctx.report.stdout_tail = out_tail.text();
        ctx.report.stderr_tail = err_tail.text();

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
            ctx.report.proc_mounted = Some(
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
            ctx.timings.start = Some(round3((ts - spawn).max(0.0) * 1000.0));
        }
        // The deadline the timeout sets, the started stamp plus
        // the seconds the caller set. A run whose started stamp
        // never stood has no deadline, because the timeout
        // bounds the task phase alone.
        let deadline_ts = match (started_ts, ctx.spec.timeout_seconds) {
            (Some(started), Some(timeout)) => Some(started + timeout as f64),
            _ => None,
        };
        let sig_ts = ctx.guard.first_time();
        // An interrupt governs the outcome where it was received
        // at or before the deadline, and the timeout governs
        // where the deadline passed first, because where an
        // interrupt and the deadline both stand over one run
        // the earlier stamp governs.
        let interrupt_before_deadline = ctx.guard.interrupted()
            && match deadline_ts {
                Some(deadline) => sig_ts.is_none() || sig_ts.unwrap() <= deadline,
                None => true,
            };
        let mut ended_ts: Option<f64> = None;
        if let Some((receipt, done)) = by_name.get("task_done") {
            if started_ts.is_none() {
                started_ts = Some(spawn);
            }
            let done_ts = done.get("ts").and_then(|v| v.as_f64()).unwrap_or(*receipt);
            ended_ts = Some(done_ts);
            ctx.timings.task = Some(round3((done_ts - started_ts.unwrap()).max(0.0) * 1000.0));
            // An interrupt received before the task done stamp
            // makes the run a harness error even where the task
            // done event reaches the runner only after the sandbox
            // has been killed, and an interrupt received after the
            // stamp leaves the task outcome standing. The stamps
            // are compared, not the order the runner drained the
            // events in, and MF-4 carries the rule. The deadline
            // stands beside the interrupt on the same terms,
            // under MF-13. Where the task done stamp stands at
            // or before the deadline the task outcome stands,
            // and where no task done stamp stands at or before
            // the deadline the run is a harness error in the
            // timeout words, unless an interrupt received before
            // the deadline stands, whose words govern as the
            // earlier stamp.
            let timed_out = match deadline_ts {
                Some(deadline) => {
                    done_ts > deadline
                        && !(ctx.guard.interrupted()
                            && (sig_ts.is_none() || sig_ts.unwrap() <= deadline))
                }
                None => false,
            };
            if timed_out {
                ctx.report.error = Some(timeout_words(ctx.spec.timeout_seconds.unwrap_or(0)));
            } else if ctx.guard.interrupted() && (sig_ts.is_none() || sig_ts.unwrap() <= done_ts) {
                ctx.report.error = Some(format!(
                    "the run was interrupted by signal {} and the sandbox was torn down",
                    ctx.guard.first().unwrap_or(0)
                ));
            } else {
                ctx.report.outcome = Outcome::Task;
                if let Some(signal) = done.get("signal").and_then(|v| v.as_i64()) {
                    ctx.report.task_signal = Some(signal as i32);
                } else if let Some(code) = done.get("exit_code").and_then(|v| v.as_i64()) {
                    ctx.report.task_exit_code = Some(code as i32);
                }
            }
        } else if let Some((receipt, error_event)) = by_name.get("start_error") {
            ctx.report.error = Some(
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
        } else if interrupt_before_deadline {
            ctx.report.error = Some(format!(
                "the run was interrupted by signal {} and the sandbox was torn down",
                ctx.guard.first().unwrap_or(0)
            ));
        } else if deadline_ts.is_some() && wait_ended >= deadline_ts.unwrap() {
            // The deadline passed with no task done stamp at or
            // before it and no interrupt received before the
            // deadline standing, so the run is a harness error
            // in the timeout words.
            ctx.report.error = Some(timeout_words(ctx.spec.timeout_seconds.unwrap_or(0)));
        } else if ctx.guard.interrupted() {
            ctx.report.error = Some(format!(
                "the run was interrupted by signal {} and the sandbox was torn down",
                ctx.guard.first().unwrap_or(0)
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
            let trimmed = ctx.report.stderr_tail.trim();
            if !trimmed.is_empty() {
                let tail: String = {
                    let chars: Vec<char> = trimmed.chars().collect();
                    let start = chars.len().saturating_sub(300);
                    chars[start..].iter().collect()
                };
                detail.push_str(&format!(", sandbox stderr tail, {}", tail));
            }
            ctx.report.error = Some(detail);
        }

        self.teardown(ctx.sandbox_dir, ctx.report, ctx.timings, ended_ts);
        if ctx.report.sandbox_removed != Some(true) {
            if ctx.report.outcome == Outcome::Task {
                ctx.report.outcome = Outcome::HarnessError;
                ctx.report.task_exit_code = None;
                ctx.report.task_signal = None;
                ctx.report.error =
                    Some("the sandbox directory could not be removed after the run".to_string());
            } else if let Some(error) = &ctx.report.error {
                ctx.report.error = Some(format!(
                    "{}, and the sandbox directory could not be removed",
                    error
                ));
            } else {
                ctx.report.error =
                    Some("the sandbox directory could not be removed after the run".to_string());
            }
        }
    }
}

/// The refusal a run answers with where one of its bounds
/// stands outside the span MF-13 accepts for it, naming the
/// bound and the floor it stands under or the highest formable
/// ceiling it stands over, in the shape the timeout breach
/// sets for each bound. The wording carries no prepare
/// prefix, because nothing was prepared. None where every
/// bound stands unset or inside its span.
fn floor_breach(spec: &RunSpec) -> Option<String> {
    if let Some(seconds) = spec.timeout_seconds {
        if seconds < crate::TIMEOUT_FLOOR_SECONDS {
            return Some(format!(
                "the timeout of {} seconds stands under the floor of {} second",
                seconds,
                crate::TIMEOUT_FLOOR_SECONDS
            ));
        }
    }
    if let Some(seconds) = spec.max_cpu_seconds {
        if seconds < crate::MAX_CPU_FLOOR_SECONDS {
            return Some(format!(
                "the processor ceiling of {} seconds stands under the floor of {} second",
                seconds,
                crate::MAX_CPU_FLOOR_SECONDS
            ));
        }
        if seconds > crate::MAX_CPU_CEILING_SECONDS {
            return Some(format!(
                "the processor ceiling of {} seconds stands over the highest formable ceiling of {} seconds",
                seconds,
                crate::MAX_CPU_CEILING_SECONDS
            ));
        }
    }
    if let Some(bytes) = spec.max_memory_bytes {
        if bytes < crate::MAX_MEMORY_FLOOR_BYTES {
            return Some(format!(
                "the address space ceiling of {} bytes stands under the floor of {} bytes",
                bytes,
                crate::MAX_MEMORY_FLOOR_BYTES
            ));
        }
    }
    None
}

/// The started stamp the status drain has recorded so far,
/// read the way the outcome code reads it, the stamp the init
/// put on its started event or the moment the runner received
/// that event where the event carries no stamp. None where no
/// started event stands yet.
fn started_stamp(events: &Arc<Mutex<Vec<(f64, serde_json::Value)>>>) -> Option<f64> {
    let snapshot = events.lock().unwrap();
    for (receipt, event) in snapshot.iter() {
        if event.get("event").and_then(|v| v.as_str()) == Some("started") {
            return Some(event.get("ts").and_then(|v| v.as_f64()).unwrap_or(*receipt));
        }
    }
    None
}

/// The words a run the timeout ended answers with, naming the
/// seconds the caller set. They fire only where the task in
/// fact did not finish by the deadline the timeout set.
fn timeout_words(seconds: u64) -> String {
    format!(
        "the task did not finish within the timeout of {} seconds and the sandbox was torn down",
        seconds
    )
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
    // The descriptor is the read end of this run's status pipe,
    // created by the caller for this drain alone, and the File
    // built from it takes the one ownership of it.
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
