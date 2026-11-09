//! mayfly, a disposable sandbox harness for agent tasks.
//!
//! A backend is anything that takes a RunSpec, runs one task in a
//! disposable sandbox, and returns a RunReport. The runner and the
//! command line talk only to this interface, so a new backend joins
//! by registering a name and implementing run. The lifecycle phases
//! every backend reports timings for are prepare, start, task, and
//! teardown.

pub mod cli;
pub mod init;
pub mod janitor;
pub mod namespace;

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

pub use namespace::NamespaceBackend;

pub const HARNESS_EXIT_CODE: i32 = 125;
pub const TAIL_CHARS: usize = 4000;
pub const TAIL_BYTES: usize = 1 << 16;
pub const PHASES: [&str; 4] = ["prepare", "start", "task", "teardown"];

/// The lowest value each bound of MF-13 accepts. A value under
/// its floor is refused as a caller error before any sandbox is
/// prepared, on the command line at parse time and at the head
/// of the backend run method on the library surface, so both
/// surfaces refuse the same values.
pub const TIMEOUT_FLOOR_SECONDS: u64 = 1;
pub const MAX_CPU_FLOOR_SECONDS: u64 = 1;
pub const MAX_MEMORY_FLOOR_BYTES: u64 = 16777216;

/// The highest processor ceiling whose limit pair the harness
/// can form. The ceiling of MF-13 stands as a soft limit at the
/// seconds the caller set and a hard limit one second past it,
/// so the one value past this ceiling names a hard limit no
/// count can hold. That value is refused as a caller error
/// before any sandbox is prepared, on both surfaces, in the
/// refusal shapes the floors use.
pub const MAX_CPU_CEILING_SECONDS: u64 = u64::MAX - 1;

/// The one monotonic clock every stamp in a run is taken from, in
/// seconds. The runner, the init event stamps, and the interrupt
/// receipt stamps all read this clock, so stamps taken in
/// different processes of one run stay comparable.
pub fn now() -> f64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // The destination is a fully initialised timespec owned by
    // this call, and clock_gettime writes only through it.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1_000_000_000.0
}

/// Round to three decimals, the rounding the report timings carry.
pub fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

/// Milliseconds between a past stamp on the shared clock and now,
/// rounded the way the Python harness rounded them.
pub fn ms_since(since: f64) -> f64 {
    round3((now() - since) * 1000.0)
}

/// Format an OS error the way Python formats the str of an
/// OSError, so error text the report carries reads the same in
/// both implementations.
pub fn py_os_error(err: &io::Error) -> String {
    match err.raw_os_error() {
        Some(errno) => format!("[Errno {}] {}", errno, strerror(errno)),
        None => err.to_string(),
    }
}

/// The same formatting with the filename Python appends when the
/// failing call carried one.
pub fn py_os_error_path(err: &io::Error, path: &Path) -> String {
    match err.raw_os_error() {
        Some(errno) => format!(
            "[Errno {}] {}: '{}'",
            errno,
            strerror(errno),
            path.display()
        ),
        None => err.to_string(),
    }
}

pub fn strerror(errno: i32) -> String {
    // strerror returns a pointer to a static string or null for
    // an errno it does not know, and the CStr read copies the
    // bytes out before anything can move them.
    unsafe {
        let ptr = libc::strerror(errno);
        if ptr.is_null() {
            return format!("Unknown error {}", errno);
        }
        std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned()
    }
}

/// The harness failed before or around the task, not in the task.
#[derive(Debug)]
pub struct HarnessError(pub String);

impl fmt::Display for HarnessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HarnessError {}

/// One task to run in one fresh sandbox.
#[derive(Debug, Clone)]
pub struct RunSpec {
    pub command: Vec<OsString>,
    pub root: PathBuf,
    pub allow_env: Vec<OsString>,
    pub network: bool,
    pub pass_fds: Vec<std::os::unix::io::RawFd>,
    /// A snapshot of the caller environment the scrub reads, taken
    /// by the caller. Where this is empty the live process
    /// environment is read at run time instead, which is what the
    /// command line and the ordinary library calls do.
    pub caller_env: Vec<(OsString, OsString)>,
    /// The wall clock timeout of the task phase in whole
    /// seconds, where the caller sets one, under MF-13. None
    /// runs under no timeout.
    pub timeout_seconds: Option<u64>,
    /// The processor time ceiling of the task in whole
    /// seconds, where the caller sets one, under MF-13. None
    /// runs under no processor ceiling.
    pub max_cpu_seconds: Option<u64>,
    /// The address space ceiling of the task in bytes, where
    /// the caller sets one, under MF-13. None runs under no
    /// address space ceiling.
    pub max_memory_bytes: Option<u64>,
}

impl RunSpec {
    pub fn new(command: Vec<OsString>, root: PathBuf) -> RunSpec {
        RunSpec {
            command,
            root,
            allow_env: vec![OsString::from("PATH")],
            network: false,
            pass_fds: Vec::new(),
            caller_env: Vec::new(),
            timeout_seconds: None,
            max_cpu_seconds: None,
            max_memory_bytes: None,
        }
    }
}

/// The lifecycle phase timings of one run, in the order the
/// report prints them. A phase a run ends before completing is
/// None, never a measured-looking zero.
#[derive(Debug, Clone, Default)]
pub struct Timings {
    pub prepare: Option<f64>,
    pub start: Option<f64>,
    pub task: Option<f64>,
    pub teardown: Option<f64>,
    pub total: Option<f64>,
}

/// One lifecycle phase a run reports a timing for. The name is
/// the key the report prints for the phase, and the phases are
/// keyed by this type rather than by their name strings, so a
/// mistyped phase is a compile error and never a missing key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Prepare,
    Start,
    Task,
    Teardown,
    Total,
}

impl Phase {
    pub const ALL: [Phase; 5] = [
        Phase::Prepare,
        Phase::Start,
        Phase::Task,
        Phase::Teardown,
        Phase::Total,
    ];

    pub fn name(&self) -> &'static str {
        match self {
            Phase::Prepare => "prepare",
            Phase::Start => "start",
            Phase::Task => "task",
            Phase::Teardown => "teardown",
            Phase::Total => "total",
        }
    }
}

impl Timings {
    pub fn get(&self, phase: Phase) -> Option<f64> {
        match phase {
            Phase::Prepare => self.prepare,
            Phase::Start => self.start,
            Phase::Task => self.task,
            Phase::Teardown => self.teardown,
            Phase::Total => self.total,
        }
    }

    pub fn set(&mut self, phase: Phase, value: Option<f64>) {
        match phase {
            Phase::Prepare => self.prepare = value,
            Phase::Start => self.start = value,
            Phase::Task => self.task = value,
            Phase::Teardown => self.teardown = value,
            Phase::Total => self.total = value,
        }
    }
}

/// The outcome a run ends in, one of the two values the report
/// prints for its outcome field. A report carries this type
/// rather than the printed string, so an outcome the contract
/// does not name cannot be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Task,
    HarnessError,
}

impl Outcome {
    /// The text the report prints for this outcome.
    pub fn as_str(&self) -> &'static str {
        match self {
            Outcome::Task => "task",
            Outcome::HarnessError => "harness_error",
        }
    }
}

/// The record of one run, printed by the command line as json.
#[derive(Debug, Clone)]
pub struct RunReport {
    pub backend: String,
    pub run_id: String,
    pub sandbox_dir: String,
    pub command: Vec<OsString>,
    pub outcome: Outcome,
    pub task_exit_code: Option<i32>,
    pub task_signal: Option<i32>,
    pub error: Option<String>,
    pub proc_mounted: Option<bool>,
    pub sandbox_removed: Option<bool>,
    pub timings_ms: Timings,
    pub stdout_tail: String,
    pub stderr_tail: String,
}

impl RunReport {
    pub fn new(
        backend: &str,
        run_id: &str,
        sandbox_dir: &str,
        command: Vec<OsString>,
        outcome: Outcome,
    ) -> RunReport {
        RunReport {
            backend: backend.to_string(),
            run_id: run_id.to_string(),
            sandbox_dir: sandbox_dir.to_string(),
            command,
            outcome,
            task_exit_code: None,
            task_signal: None,
            error: None,
            proc_mounted: None,
            sandbox_removed: None,
            timings_ms: Timings::default(),
            stdout_tail: String::new(),
            stderr_tail: String::new(),
        }
    }

    pub fn to_json(&self) -> String {
        let mut timings: Vec<(String, JsonValue)> = Vec::new();
        for phase in Phase::ALL {
            timings.push((phase.name().into(), opt_float(self.timings_ms.get(phase))));
        }
        let fields: Vec<(String, JsonValue)> = vec![
            ("backend".into(), JsonValue::Str(self.backend.clone())),
            ("run_id".into(), JsonValue::Str(self.run_id.clone())),
            (
                "sandbox_dir".into(),
                JsonValue::Str(self.sandbox_dir.clone()),
            ),
            (
                "command".into(),
                JsonValue::List(
                    self.command
                        .iter()
                        .map(|arg| JsonValue::Str(arg.to_string_lossy().into_owned()))
                        .collect(),
                ),
            ),
            (
                "outcome".into(),
                JsonValue::Str(self.outcome.as_str().to_string()),
            ),
            ("task_exit_code".into(), opt_int(self.task_exit_code)),
            ("task_signal".into(), opt_int(self.task_signal)),
            ("error".into(), opt_str(self.error.as_deref())),
            ("proc_mounted".into(), opt_bool(self.proc_mounted)),
            ("sandbox_removed".into(), opt_bool(self.sandbox_removed)),
            ("timings_ms".into(), JsonValue::Object(timings)),
            (
                "stdout_tail".into(),
                JsonValue::Str(self.stdout_tail.clone()),
            ),
            (
                "stderr_tail".into(),
                JsonValue::Str(self.stderr_tail.clone()),
            ),
        ];
        dump_pretty(&JsonValue::Object(fields))
    }
}

fn opt_int(value: Option<i32>) -> JsonValue {
    match value {
        Some(v) => JsonValue::Int(v.into()),
        None => JsonValue::Null,
    }
}

fn opt_str(value: Option<&str>) -> JsonValue {
    match value {
        Some(v) => JsonValue::Str(v.to_string()),
        None => JsonValue::Null,
    }
}

fn opt_bool(value: Option<bool>) -> JsonValue {
    match value {
        Some(v) => JsonValue::Bool(v),
        None => JsonValue::Null,
    }
}

fn opt_float(value: Option<f64>) -> JsonValue {
    match value {
        Some(v) => JsonValue::Float(v),
        None => JsonValue::Null,
    }
}

/// A minimal json value used only to print reports the way the
/// Python harness printed them, with two space indentation and
/// non-ascii text escaped the way Python escapes it.
pub enum JsonValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<JsonValue>),
    Object(Vec<(String, JsonValue)>),
}

pub fn dump_pretty(value: &JsonValue) -> String {
    let mut out = String::new();
    write_value(value, 0, &mut out);
    out
}

fn write_indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn write_value(value: &JsonValue, depth: usize, out: &mut String) {
    match value {
        JsonValue::Null => out.push_str("null"),
        JsonValue::Bool(true) => out.push_str("true"),
        JsonValue::Bool(false) => out.push_str("false"),
        JsonValue::Int(v) => out.push_str(&v.to_string()),
        JsonValue::Float(v) => out.push_str(&py_float(*v)),
        JsonValue::Str(s) => write_json_string(s, out),
        JsonValue::List(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push_str("[\n");
            for (index, item) in items.iter().enumerate() {
                write_indent(out, depth + 1);
                write_value(item, depth + 1, out);
                if index + 1 < items.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            write_indent(out, depth);
            out.push(']');
        }
        JsonValue::Object(fields) => {
            if fields.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push_str("{\n");
            for (index, (key, item)) in fields.iter().enumerate() {
                write_indent(out, depth + 1);
                write_json_string(key, out);
                out.push_str(": ");
                write_value(item, depth + 1, out);
                if index + 1 < fields.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            write_indent(out, depth);
            out.push('}');
        }
    }
}

/// Format a float the way Python prints one in json output, with
/// a trailing .0 on a whole value and the shortest round trip
/// decimal otherwise.
pub fn py_float(value: f64) -> String {
    if value == value.trunc() && value.abs() < 1e16 && value.is_finite() {
        format!("{:.1}", value)
    } else {
        format!("{}", value)
    }
}

/// Escape a string the way Python json escapes it, with every
/// non-ascii character written as a unicode escape.
pub fn write_json_string(text: &str, out: &mut String) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c if (c as u32) < 0x80 => out.push(c),
            c => {
                let n = c as u32;
                if n <= 0xFFFF {
                    out.push_str(&format!("\\u{:04x}", n));
                } else {
                    let v = n - 0x10000;
                    out.push_str(&format!(
                        "\\u{:04x}\\u{:04x}",
                        0xD800 + (v >> 10),
                        0xDC00 + (v & 0x3FF)
                    ));
                }
            }
        }
    }
    out.push('"');
}

/// The backend registry. Backends register by name here and
/// nowhere else, so the runner and the command line resolve a
/// name to a backend in exactly one place.
pub fn get_backend(name: &str) -> Result<NamespaceBackend, HarnessError> {
    match name {
        "namespace" => Ok(NamespaceBackend::new()),
        _ => Err(HarnessError(format!("unknown backend, {}", name))),
    }
}

pub fn backend_names() -> Vec<&'static str> {
    vec!["namespace"]
}

/// The runner entry point the command line and the tests share.
pub fn run_task(
    command: Vec<OsString>,
    root: &Path,
    backend: &str,
) -> Result<RunReport, HarnessError> {
    let spec = RunSpec::new(command, root.to_path_buf());
    Ok(get_backend(backend)?.run(&spec))
}

/// The path of the mayfly binary the backend re-invokes for its
/// init and janitor. The binary itself is the default. A test
/// harness whose own executable is not the mayfly binary points
/// this at the built binary once, through set_helper_exe.
static HELPER_EXE: OnceLock<PathBuf> = OnceLock::new();

pub fn helper_exe() -> PathBuf {
    if let Some(path) = HELPER_EXE.get() {
        return path.clone();
    }
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("mayfly"))
}

pub fn set_helper_exe(path: &Path) {
    let _ = HELPER_EXE.set(path.to_path_buf());
}

pub const PROVENANCE_NAME: &str = ".mayfly-provenance";
pub const PROVENANCE_READ_LIMIT: usize = 4096;

/// The environment key the supervisor uses to hand the per run
/// token to the internal children it spawns, the unshare process
/// and through it the init. The task never carries it, because
/// the environment the supervisor builds is scrubbed to the
/// allowlist and the init removes this key from its own
/// environment before it supervises the task. The hidden
/// assembly entry point refuses to run without a value of the
/// token shape under this key, so it cannot be invoked directly
/// on a host by accident and perform its mounts there.
pub const INTERNAL_TOKEN_ENV: &str = "MAYFLY_INTERNAL_TOKEN";

/// Whether a value has the shape of a per run token, thirty two
/// lowercase hexadecimal characters, the shape the token
/// generator produces.
pub fn token_shape_ok(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The run id a directory's provenance file records, or None.
///
/// Every failure shape reads as no provenance at all, because
/// each one means the directory carries nothing the sweep may
/// trust. A path that is not a regular file names nothing, and
/// neither do bytes that are not text and a read that fails.
/// The check follows a symlink to the file it names, so a
/// provenance that is a symlink to a regular file reads
/// through to that file and the file itself is never touched.
/// The open cannot block and the read is bounded, so a pipe
/// planted at the provenance path or a file without end can
/// neither hang the sweep nor fill its memory, and the strict
/// decode lives inside the guard, so bytes that are not text
/// raise nothing out of the sweep or the run behind it.
pub fn read_provenance(entry: &Path) -> Option<String> {
    use std::io::Read;
    let path = entry.join(PROVENANCE_NAME);
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&path)
        .ok()?;
    let mut chunks: Vec<u8> = Vec::new();
    let mut buf = [0u8; PROVENANCE_READ_LIMIT];
    loop {
        if chunks.len() > PROVENANCE_READ_LIMIT {
            return None;
        }
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => chunks.extend_from_slice(&buf[..n]),
            Err(_) => return None,
        }
    }
    if chunks.len() > PROVENANCE_READ_LIMIT {
        return None;
    }
    let text = std::str::from_utf8(&chunks).ok()?;
    Some(text.trim().to_string())
}

/// Write the run provenance inside a fresh sandbox directory.
///
/// The write lands through a temporary file in the same
/// directory that is renamed over the provenance path, so a
/// reader sees either no provenance at all or the complete
/// file, and a death inside the write leaves no truncated
/// file behind that names nothing and no sweep would ever
/// collect. A failure raises an io error, which the caller
/// reports as a prepare failure, and the temporary file is
/// removed on the way out.
pub fn write_provenance(sandbox_dir: &Path, run_id: &str) -> io::Result<()> {
    let target = sandbox_dir.join(PROVENANCE_NAME);
    let tmp = sandbox_dir.join(format!("{}.tmp-{}", PROVENANCE_NAME, std::process::id()));
    let result = (|| -> io::Result<()> {
        std::fs::write(&tmp, format!("{}\n", run_id))?;
        std::fs::rename(&tmp, &target)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Host pids whose command line names the given path, read from
/// the given process table directory.
///
/// A live run always has processes that name its sandbox
/// directory, the janitor through its sandbox argument and the
/// init through its rootfs argument, so a sandbox directory no
/// live process names belongs to a run that is over. Returns
/// None when the process table cannot be listed at all, and
/// also when it lists but not one command line in it can be
/// read, because a table that cannot be read in substance says
/// nothing about which runs are live and must never read as an
/// empty one. A listing that fails part way is unknown in the
/// same way, because the entries it never reached may name the
/// path and a partial list treated as complete could read a
/// live run as dead. A read that fails because the process vanished
/// between the listing and the read is ordinary and is passed
/// over. A read refused for any other reason also makes the
/// answer None, because the one entry withheld may be the one
/// that names the path.
pub fn pids_referencing_in(proc_dir: &Path, path: &Path) -> Option<Vec<u32>> {
    let needle = path.as_os_str().as_bytes();
    let mut found = Vec::new();
    let entries = std::fs::read_dir(proc_dir).ok()?;
    let own = std::process::id();
    let mut reads = 0usize;
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => return None,
        };
        let name = entry.file_name();
        let name_bytes = name.as_bytes();
        if name_bytes.is_empty() || !name_bytes.iter().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let pid: u32 = match name.to_str().and_then(|s| s.parse().ok()) {
            Some(p) => p,
            None => continue,
        };
        if pid == own {
            continue;
        }
        let cmdline_path = entry.path().join("cmdline");
        match std::fs::read(&cmdline_path) {
            Ok(cmdline) => {
                reads += 1;
                if contains_subslice(&cmdline, needle) {
                    found.push(pid);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        }
    }
    if reads == 0 {
        return None;
    }
    Some(found)
}

pub fn pids_referencing(path: &Path) -> Option<Vec<u32>> {
    pids_referencing_in(Path::new("/proc"), path)
}

pub fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > haystack.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

pub const ORPHAN_GRACE_SECONDS: f64 = 5.0;

/// Remove sandbox directories under root whose runs are dead.
///
/// A directory is swept only when the provenance file inside
/// it names the same run id as the directory itself, so the
/// sweep can tell the directories mayfly created from any
/// other directory carrying the same name shape, an
/// identification that guards against accidental collision
/// and name confusion and is not authentication and no
/// boundary against a writer to the run root, and only when it is
/// older than the grace period, so a run still being prepared
/// by a concurrent process is never touched, and only when no
/// live process names it, so a live run of any age keeps its
/// sandbox. A directory position is judged by the entry standing
/// there itself and never through a link it may be, so a symlink
/// planted at a run directory position is neither read through
/// nor removed, because the sweep never decides from data
/// outside the run root. Where the process table cannot be listed, liveness
/// is unknown and nothing is swept. A clean marker whose run
/// directory is already gone is swept on the same terms. A
/// sweep failure never breaks the run that triggered it.
pub fn sweep_orphans_in(root: &Path, proc_dir: &Path) {
    let entries: Vec<PathBuf> = match std::fs::read_dir(root) {
        Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).collect(),
        Err(_) => return,
    };
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    for entry in entries {
        sweep_one(root, proc_dir, &entry, now_secs);
    }
}

fn sweep_one(root: &Path, proc_dir: &Path, entry: &Path, now_secs: f64) {
    let name = match entry.file_name().and_then(|n| n.to_str()) {
        Some(n) => n.to_string(),
        None => return,
    };
    // Only a real directory standing at the position counts. A
    // symlink there is skipped whole, its target never read and
    // the link itself never removed, whatever stands behind it.
    let is_real_dir = std::fs::symlink_metadata(entry)
        .map(|meta| meta.file_type().is_dir())
        .unwrap_or(false);
    if is_real_dir && name.starts_with("run-") {
        let mtime = match entry.metadata().and_then(|m| m.modified()) {
            Ok(t) => t
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0),
            Err(_) => return,
        };
        if now_secs - mtime < ORPHAN_GRACE_SECONDS {
            return;
        }
        let run_id = &name["run-".len()..];
        if read_provenance(entry).as_deref() != Some(run_id) {
            return;
        }
        match pids_referencing_in(proc_dir, entry) {
            None => return,
            Some(pids) if !pids.is_empty() => return,
            _ => {}
        }
        let _ = std::fs::remove_dir_all(entry);
        let marker = root.join(format!(".mayfly-clean-{}", run_id));
        let _ = std::fs::remove_file(marker);
    } else if entry.is_file() && name.starts_with(".mayfly-clean-") {
        let run_dir = root.join(format!("run-{}", &name[".mayfly-clean-".len()..]));
        if run_dir.exists() {
            return;
        }
        let mtime = match entry.metadata().and_then(|m| m.modified()) {
            Ok(t) => t
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0),
            Err(_) => return,
        };
        if now_secs - mtime < ORPHAN_GRACE_SECONDS {
            return;
        }
        let _ = std::fs::remove_file(entry);
    }
}

pub fn sweep_orphans(root: &Path) {
    sweep_orphans_in(root, Path::new("/proc"));
}

/// Record SIGTERM and SIGINT instead of dying on them mid run.
///
/// The handler only records the first signal number and stamps
/// the moment it was received on the shared monotonic clock, so
/// the outcome logic can compare the receipt of an interrupt
/// against the task done stamp rather than against when the
/// runner happened to drain the event. Handlers exist only on
/// the main thread, so a run driven from a worker thread gets
/// none and keeps the default behaviour.
pub struct SignalGuard {
    installed: bool,
    prev_int: Option<libc::sigaction>,
    prev_term: Option<libc::sigaction>,
}

static SIGNAL_COUNT: AtomicUsize = AtomicUsize::new(0);
static SIGNAL_FIRST: AtomicI32 = AtomicI32::new(0);
static SIGNAL_FIRST_TIME: AtomicU64 = AtomicU64::new(0);

extern "C" fn record_signal(signum: i32) {
    // The snapshot is stored before the count moves, and the
    // count store releases it, so a reader that acquires a
    // nonzero count always sees the first signal and its stamp
    // whole, and never a count standing over an unstamped zero.
    if SIGNAL_COUNT.load(Ordering::SeqCst) == 0 {
        SIGNAL_FIRST.store(signum, Ordering::Release);
        SIGNAL_FIRST_TIME.store(now().to_bits(), Ordering::Release);
    }
    SIGNAL_COUNT.fetch_add(1, Ordering::Release);
}

fn is_main_thread() -> bool {
    // gettid and getpid are plain queries with no pointer or
    // lifetime contract, and their results are compared as
    // integers only.
    unsafe { libc::syscall(libc::SYS_gettid) == libc::getpid() as i64 }
}

impl SignalGuard {
    pub fn install() -> SignalGuard {
        let mut guard = SignalGuard {
            installed: false,
            prev_int: None,
            prev_term: None,
        };
        if !is_main_thread() {
            return guard;
        }
        SIGNAL_COUNT.store(0, Ordering::SeqCst);
        SIGNAL_FIRST.store(0, Ordering::SeqCst);
        SIGNAL_FIRST_TIME.store(0, Ordering::SeqCst);
        // The action struct is fully initialised before the
        // call, the handler is a plain extern function that
        // touches only the atomics above, and the previous
        // actions are written into owned storage the guard
        // keeps for its restore.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = record_signal as usize;
            libc::sigemptyset(&mut action.sa_mask);
            action.sa_flags = 0;
            let mut prev_int: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(libc::SIGINT, &action, &mut prev_int) == 0 {
                guard.prev_int = Some(prev_int);
            }
            let mut prev_term: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(libc::SIGTERM, &action, &mut prev_term) == 0 {
                guard.prev_term = Some(prev_term);
            }
        }
        guard.installed = true;
        guard
    }

    pub fn interrupted(&self) -> bool {
        self.installed && SIGNAL_COUNT.load(Ordering::Acquire) > 0
    }

    pub fn first(&self) -> Option<i32> {
        if self.interrupted() {
            Some(SIGNAL_FIRST.load(Ordering::Acquire))
        } else {
            None
        }
    }

    pub fn first_time(&self) -> Option<f64> {
        if self.interrupted() {
            Some(f64::from_bits(SIGNAL_FIRST_TIME.load(Ordering::Acquire)))
        } else {
            None
        }
    }
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        if !self.installed {
            return;
        }
        // The restored actions are the exact structs sigaction
        // handed back at install time, kept unmodified in the
        // guard, so restoring them returns the thread to the
        // handlers it had before the guard.
        unsafe {
            if let Some(prev) = self.prev_int.take() {
                libc::sigaction(libc::SIGINT, &prev, std::ptr::null_mut());
            }
            if let Some(prev) = self.prev_term.take() {
                libc::sigaction(libc::SIGTERM, &prev, std::ptr::null_mut());
            }
        }
    }
}

/// A bounded tail of one stream, kept as bytes and decoded late.
pub struct TailBuffer {
    buf: Mutex<Vec<u8>>,
    limit: usize,
}

impl TailBuffer {
    pub fn new() -> TailBuffer {
        TailBuffer {
            buf: Mutex::new(Vec::new()),
            limit: TAIL_BYTES,
        }
    }

    pub fn feed(&self, chunk: &[u8]) {
        let mut buf = self.buf.lock().unwrap();
        buf.extend_from_slice(chunk);
        if buf.len() > self.limit {
            let overflow = buf.len() - self.limit;
            buf.drain(..overflow);
        }
    }

    pub fn text(&self) -> String {
        let raw = {
            let buf = self.buf.lock().unwrap();
            buf.clone()
        };
        let decoded = String::from_utf8_lossy(&raw).into_owned();
        let count = decoded.chars().count();
        if count <= TAIL_CHARS {
            decoded
        } else {
            decoded.chars().skip(count - TAIL_CHARS).collect()
        }
    }
}

impl Default for TailBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// A live view of environment pairs as ordered bytes, so tests
/// can build synthetic caller environments without mutating the
/// process environment.
pub fn current_env() -> Vec<(OsString, OsString)> {
    std::env::vars_os().collect()
}

pub fn env_lookup<'a>(env: &'a [(OsString, OsString)], key: &OsStr) -> Option<&'a OsStr> {
    env.iter()
        .find(|(k, _)| k.as_os_str() == key)
        .map(|(_, v)| v.as_os_str())
}
