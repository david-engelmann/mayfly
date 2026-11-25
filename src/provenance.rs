//! Provenance and the orphan sweep.
//!
//! The provenance a run directory carries, the one reading
//! of the process table the sweep judges liveness against,
//! and the sweep itself.

use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// The name of the provenance file standing inside every
/// sandbox directory, carrying the id of the run that created
/// the directory. The orphan sweep trusts a directory whose
/// provenance names the directory's own run id, an
/// identification the sweep's own doc bounds.
pub const PROVENANCE_NAME: &str = ".mayfly-provenance";

/// The prefix every run directory's name carries, the run
/// id standing after it. The runner builds the directory's
/// name from the id under this prefix, and the sweep and
/// the init read the id back out of the name, so the shape
/// is stated here once and every seat that builds or parses
/// the name takes it from this statement.
pub const RUN_DIR_PREFIX: &str = "run-";
/// The prefix every clean marker's name carries, the run
/// id standing after it, under the same rule as the run
/// directory's prefix. The runner and the sweep build the
/// marker's name from the id, and the sweep reads the id
/// back out of it.
pub const CLEAN_MARKER_PREFIX: &str = ".mayfly-clean-";

/// The name of the sandbox directory a run with this id
/// stands under its root.
pub fn run_dir_name(run_id: &str) -> String {
    format!("{}{}", RUN_DIR_PREFIX, run_id)
}

/// The name of the clean marker a run with this id leaves
/// in its run root beside its run directory.
pub fn clean_marker_name(run_id: &str) -> String {
    format!("{}{}", CLEAN_MARKER_PREFIX, run_id)
}
/// The byte bound the provenance read accumulates against. A
/// provenance standing past it reads as no provenance at all,
/// so an oversize file planted at the path names nothing the
/// sweep can trust.
pub const PROVENANCE_READ_LIMIT: usize = 4096;

/// The environment key the supervisor uses to hand the per run
/// token to the internal children it spawns, the unshare process
/// and through it the init. The task never carries it, because
/// the environment the supervisor builds is scrubbed to the
/// allowlist and the init removes this key from its own
/// environment before it supervises the task. The hidden
/// assembly entry point and the init refuse to run without
/// a value of the token shape under this key, so neither can
/// be invoked directly on a host by accident and perform its
/// mounts there.
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
/// One reading of a process table: the command line of every
/// process in it that could be read, by pid. The orphan sweep
/// reads the table once and tests every candidate against this
/// one reading, so a sweep over k stale candidates pays one
/// table scan instead of k. The reading is None in exactly the
/// shapes where a scan's answer is unknown: a table that
/// cannot be listed, a listing that fails part way, a command
/// line read refused for any reason but a process vanishing
/// between the listing and the read, and a table in which not
/// one command line could be read at all.
struct ProcTable {
    lines: Vec<(u32, Vec<u8>)>,
}

impl ProcTable {
    /// The pids in this reading whose command line names the
    /// given path. The naming is byte for byte: the path's
    /// bytes, in the spelling the caller built it with, must
    /// stand somewhere in the command line's bytes, so a
    /// process that names the same directory under another
    /// spelling of it is not seen by this reading.
    fn referencing(&self, path: &Path) -> Vec<u32> {
        let needle = path.as_os_str().as_bytes();
        self.lines
            .iter()
            .filter(|(_, cmdline)| contains_subslice(cmdline, needle))
            .map(|(pid, _)| *pid)
            .collect()
    }
}

fn read_proc_table(proc_dir: &Path) -> Option<ProcTable> {
    let mut lines: Vec<(u32, Vec<u8>)> = Vec::new();
    let entries = std::fs::read_dir(proc_dir).ok()?;
    let own = std::process::id();
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
            Ok(cmdline) => lines.push((pid, cmdline)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        }
    }
    if lines.is_empty() {
        return None;
    }
    Some(ProcTable { lines })
}

/// The pids whose command lines in the process table at
/// proc_dir name path, or None where that table could not be
/// read whole. The None is load bearing. It says liveness is
/// unknown, where Some of an empty list says the table read
/// clean and nothing live names the path.
pub fn pids_referencing_in(proc_dir: &Path, path: &Path) -> Option<Vec<u32>> {
    read_proc_table(proc_dir).map(|table| table.referencing(path))
}

/// pids_referencing_in against the live process table at
/// /proc.
pub fn pids_referencing(path: &Path) -> Option<Vec<u32>> {
    pids_referencing_in(Path::new("/proc"), path)
}

/// Whether needle stands anywhere in haystack, byte for byte.
/// An empty needle stands in every haystack.
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

/// The canonical form of a run root, the one spelling
/// every seat that builds or judges a path under the root
/// agrees on.
///
/// The walk climbs from the root toward the filesystem
/// root until a prefix of it canonicalizes, and the
/// components below that prefix are then applied to the
/// canonical form in order: a plain name is joined, a
/// current directory reference is skipped, and a parent
/// reference removes the last component standing. A
/// parent reference names the same directory the kernel
/// would reach at every step it is applied, because the
/// component it removes either does not stand yet, which
/// the failed canonicalizations on the climb proved of
/// it, or stands inside a canonical prefix, which
/// carries no links for it to resolve through. A root
/// under parents a runner is about to create therefore
/// takes the canonical spelling whole, and the tail the
/// runner then creates is canonical by construction. A
/// root that reaches no canonicalizable prefix at all
/// keeps the spelling it was given.
pub fn canonical_root(root: &Path) -> PathBuf {
    use std::path::Component;
    let mut suffix: Vec<Component> = Vec::new();
    let mut cursor = root;
    let mut resolved = loop {
        if let Ok(prefix) = std::fs::canonicalize(cursor) {
            break prefix;
        }
        match cursor.components().next_back() {
            Some(component @ (Component::Normal(_) | Component::ParentDir | Component::CurDir)) => {
                suffix.push(component);
                match cursor.parent() {
                    Some(parent) if !parent.as_os_str().is_empty() => cursor = parent,
                    _ if cursor != Path::new(".") => cursor = Path::new("."),
                    _ => return root.to_path_buf(),
                }
            }
            _ => return root.to_path_buf(),
        }
    };
    for component in suffix.iter().rev() {
        match component {
            Component::Normal(name) => resolved.push(name),
            Component::ParentDir => {
                resolved.pop();
            }
            _ => {}
        }
    }
    resolved
}

/// The seconds a sandbox directory must stand untouched
/// before the orphan sweep will judge it. The grace alone
/// does not keep a run being prepared safe from the sweep.
/// Once it has passed, the sweep spares a judged directory
/// only while a live process names it, as the run's janitor
/// does from its spawn, so a run whose janitor did not start
/// can still be swept once the grace has passed.
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
/// older than the grace period, which keeps a young directory
/// from judgment and no more, and only when no live process
/// names it in the one reading of the process table the sweep
/// judges against, so a live run named in that reading keeps
/// its sandbox at any age. Past the grace a run still being
/// prepared is kept safe by that naming alone, as its janitor
/// names its directory from its spawn, and a run whose janitor
/// did not start, or had not started when the reading was
/// taken, can still be swept once the grace has passed.
/// The naming that judgement rests on is byte for byte,
/// and both sides of it are built under one spelling of the
/// root. The candidates here are built from the canonical
/// form of the root this invocation was handed, and the
/// runner builds every run's sandbox path from that same
/// canonical form, so a live run's processes name its
/// directory in the spelling the candidate carries,
/// whichever spelling either invocation reached the root
/// by. A directory position is judged by the entry standing
/// there itself and never through a link it may be, so a symlink
/// planted at a run directory position is neither read through
/// nor removed, because the sweep never decides from data
/// outside the run root. Where the process table cannot be listed, liveness
/// is unknown and nothing is swept. The liveness of every
/// candidate in one sweep is judged against a single reading
/// of the process table, taken when the first candidate is
/// judged. A clean marker whose run
/// directory is already gone is swept on the same terms. A
/// sweep failure never breaks the run that triggered it.
pub fn sweep_orphans_in(root: &Path, proc_dir: &Path) {
    let root = &canonical_root(root);
    let entries: Vec<PathBuf> = match std::fs::read_dir(root) {
        Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).collect(),
        Err(_) => return,
    };
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    // The table is read once, when the first candidate reaches
    // the liveness check, and every candidate in the sweep is
    // judged against that one reading. A sweep whose root
    // holds no stale candidate never reads the table at all.
    let mut table: Option<Option<ProcTable>> = None;
    for entry in entries {
        sweep_one(root, proc_dir, &entry, now_secs, &mut table);
    }
}

/// The modification time of the entry standing at a path, in
/// seconds since the epoch, as the sweep judges an entry's
/// age. None where the entry's metadata or its modification
/// time cannot be read, the answer both of sweep_one's
/// judgements take as a refusal to judge the entry at all. A
/// time standing before the epoch reads as 0.0.
fn entry_mtime_secs(entry: &Path) -> Option<f64> {
    match entry.metadata().and_then(|m| m.modified()) {
        Ok(t) => Some(
            t.duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0),
        ),
        Err(_) => None,
    }
}

fn sweep_one(
    root: &Path,
    proc_dir: &Path,
    entry: &Path,
    now_secs: f64,
    table: &mut Option<Option<ProcTable>>,
) {
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
    if is_real_dir && name.starts_with(RUN_DIR_PREFIX) {
        let mtime = match entry_mtime_secs(entry) {
            Some(t) => t,
            None => return,
        };
        if now_secs - mtime < ORPHAN_GRACE_SECONDS {
            return;
        }
        let run_id = &name[RUN_DIR_PREFIX.len()..];
        if read_provenance(entry).as_deref() != Some(run_id) {
            return;
        }
        let snapshot = table.get_or_insert_with(|| read_proc_table(proc_dir));
        match snapshot.as_ref().map(|t| t.referencing(entry)) {
            None => return,
            Some(pids) if !pids.is_empty() => return,
            _ => {}
        }
        let _ = std::fs::remove_dir_all(entry);
        let marker = root.join(clean_marker_name(run_id));
        let _ = std::fs::remove_file(marker);
    } else if entry.is_file() && name.starts_with(CLEAN_MARKER_PREFIX) {
        let run_dir = root.join(run_dir_name(&name[CLEAN_MARKER_PREFIX.len()..]));
        let run_dir_stands = std::fs::symlink_metadata(&run_dir)
            .map(|meta| meta.file_type().is_dir())
            .unwrap_or(false);
        if run_dir_stands {
            return;
        }
        let mtime = match entry_mtime_secs(entry) {
            Some(t) => t,
            None => return,
        };
        if now_secs - mtime < ORPHAN_GRACE_SECONDS {
            return;
        }
        let _ = std::fs::remove_file(entry);
    }
}

/// sweep_orphans_in against the live process table at /proc.
/// Every run calls this on its root before it prepares its
/// own sandbox.
pub fn sweep_orphans(root: &Path) {
    sweep_orphans_in(root, Path::new("/proc"));
}
