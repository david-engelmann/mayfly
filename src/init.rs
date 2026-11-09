//! The mayfly in-sandbox init.
//!
//! This module runs as pid 1 inside the namespaces unshare
//! created, reached by re-invoking the mayfly binary under the
//! hidden __init subcommand. It assembles the prepared root,
//! pivots into it with the old root detached behind it,
//! supervises one task, and reports lifecycle events as json
//! lines on the status descriptor it was handed.
//!
//! The init forks the task itself and waits on it, so the exact
//! exit code and the exact signal come from its own wait.
//! Nothing here trusts unshare to report how the task died,
//! because a probe showed unshare reports 0 when pid 1 dies by
//! a signal.
//!
//! The init keeps its own capabilities for the assembly. The
//! forked task child drops every capability before it execs, so
//! the task holds no CAP_SYS_CHROOT and no CAP_SYS_ADMIN and
//! cannot chroot or mount or remount its way out of the
//! prepared root. The init also arms a parent death signal and
//! watches the status pipe, so when the supervisor on the host
//! goes away the sandbox dies with it.

use std::ffi::{CString, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;

use crate::{now, py_os_error_path, strerror};

pub const MS_BIND: u64 = 4096;
pub const MS_REC: u64 = 16384;
pub const MS_PRIVATE: u64 = 1 << 18;
pub const MS_REMOUNT: u64 = 32;
pub const MS_RDONLY: u64 = 1;
pub const MNT_DETACH: i32 = 2;

pub const PR_SET_PDEATHSIG: i32 = 1;
pub const PR_CAPBSET_DROP: i32 = 24;
pub const CAPSET_VERSION_3: u32 = 0x20080522;
pub const AT_FDCWD: i32 = -100;
pub const AT_RECURSIVE: u32 = 0x8000;
pub const MOUNT_ATTR_RDONLY: u64 = 0x0001;
pub const SYS_PIVOT_ROOT: &[(&str, i64)] = &[("x86_64", 155), ("aarch64", 41)];
pub const SYS_MOUNT_SETATTR: &[(&str, i64)] = &[("x86_64", 442), ("aarch64", 442)];

pub const RESERVED_EXIT: i32 = 125;
pub const HOSTNAME: &str = "mayfly";
pub const DEVICE_FILES: [&str; 5] = ["null", "zero", "full", "random", "urandom"];
pub const ROOT_DIRS: [&str; 7] = ["usr", "etc", "dev", "proc", "sys", "work", "tmp"];
pub const LINKED_DIRS: [&str; 4] = ["bin", "sbin", "lib", "lib64"];

fn arch_name() -> &'static str {
    std::env::consts::ARCH
}

fn table_lookup(table: &[(&str, i64)], arch: &str) -> Option<i64> {
    table
        .iter()
        .find(|(name, _)| *name == arch)
        .map(|(_, n)| *n)
}

fn errno_now() -> i32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn cstring(text: &str) -> CString {
    CString::new(text).unwrap_or_else(|_| CString::new("").expect("empty cstring"))
}

/// One mount call, in the shape the assembly drives it. The
/// error text is the str of the OSError the Python init raised
/// for the same call, errno and message and target.
pub type MountFn<'a> = dyn Fn(Option<&str>, &str, Option<&str>, u64) -> Result<(), String> + 'a;

pub fn real_mount(
    source: Option<&str>,
    target: &str,
    fstype: Option<&str>,
    flags: u64,
) -> Result<(), String> {
    let src = source.map(cstring);
    let fs = fstype.map(cstring);
    let tgt = cstring(target);
    // Every pointer is either null or borrowed from a CString
    // this call owns and keeps alive until the call returns,
    // and the flags are plain values.
    let rc = unsafe {
        libc::mount(
            src.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
            tgt.as_ptr(),
            fs.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
            flags as libc::c_ulong,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        let errno = errno_now();
        return Err(format!(
            "[Errno {}] {}: '{}'",
            errno,
            strerror(errno),
            target
        ));
    }
    Ok(())
}

/// Decode the escapes the kernel writes into a mount point
/// field of the mounts table. A space in a mount point stands
/// in the table as a backslash followed by three octal digits,
/// and so do a tab and a newline and a backslash itself, so a
/// literal comparison against the real path never matches a
/// tree bound under an escaped path. Anything that is not a
/// backslash followed by three octal digits stands as itself.
pub fn decode_mount_point(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\'
            && index + 3 < bytes.len()
            && matches!(bytes[index + 1], b'0'..=b'7')
            && matches!(bytes[index + 2], b'0'..=b'7')
            && matches!(bytes[index + 3], b'0'..=b'7')
        {
            let value = (bytes[index + 1] - b'0') * 64
                + (bytes[index + 2] - b'0') * 8
                + (bytes[index + 3] - b'0');
            out.push(value);
            index += 4;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Set the read only flag on a bound tree and every submount.
///
/// The recursive mount_setattr call carries the claim where the
/// kernel offers it. The walk behind it remounts each mount point
/// under the target one by one, deepest first, for kernels where
/// the call is refused.
fn remount_read_only_tree(target: &str, mounts: &MountFn) -> Result<(), String> {
    if let Some(number) = table_lookup(SYS_MOUNT_SETATTR, arch_name()) {
        // The mount attr structure is five u64 words, with the
        // set mask in the first word and the size passed as 40.
        let attr: [u64; 5] = [MOUNT_ATTR_RDONLY, 0, 0, 0, 0];
        let tgt = cstring(target);
        // The structure is a plain array of words whose layout
        // the call defines, the target string outlives the call,
        // and the kernel reads both only for the call's duration.
        let rc = unsafe {
            libc::syscall(
                number,
                AT_FDCWD,
                tgt.as_ptr(),
                AT_RECURSIVE,
                attr.as_ptr(),
                40usize,
            )
        };
        if rc == 0 {
            return Ok(());
        }
    }
    let mounts_text = std::fs::read_to_string("/proc/self/mounts")
        .map_err(|err| py_os_error_path(&err, Path::new("/proc/self/mounts")))?;
    let mut points: Vec<String> = Vec::new();
    for line in mounts_text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() >= 2 {
            points.push(decode_mount_point(fields[1]));
        }
    }
    let under: Vec<String> = points
        .into_iter()
        .filter(|p| p == target || p.starts_with(&format!("{}/", target)))
        .collect();
    let mut sorted = under;
    sorted.sort_by_key(|point| std::cmp::Reverse(point.len()));
    for point in sorted {
        mounts(Some(&point), &point, None, MS_BIND | MS_REMOUNT | MS_RDONLY)?;
    }
    Ok(())
}

/// Build the prepared root and return whether proc was mounted.
pub fn assemble_root_with(rootfs: &Path, mounts: &MountFn) -> Result<bool, String> {
    write_provenance_init(rootfs);
    mounts(None, "/", None, MS_REC | MS_PRIVATE)?;
    for name in ROOT_DIRS {
        let dir = rootfs.join(name);
        std::fs::create_dir_all(&dir).map_err(|err| py_os_error_path(&err, &dir))?;
    }
    let mut bind_sources: Vec<String> = vec!["/usr".to_string(), "/etc".to_string()];
    for name in LINKED_DIRS {
        let host = format!("/{}", name);
        let target = rootfs.join(name);
        if let Ok(meta) = std::fs::symlink_metadata(&host) {
            if meta.file_type().is_symlink() {
                if std::fs::symlink_metadata(&target).is_err() {
                    let dest = std::fs::read_link(&host)
                        .map_err(|err| py_os_error_path(&err, Path::new(&host)))?;
                    std::os::unix::fs::symlink(&dest, &target)
                        .map_err(|err| py_os_error_path(&err, &target))?;
                }
            } else if Path::new(&host).is_dir() {
                std::fs::create_dir_all(&target).map_err(|err| py_os_error_path(&err, &target))?;
                bind_sources.push(host);
            }
        }
    }
    let mut targets: Vec<String> = Vec::new();
    for host in &bind_sources {
        let target = rootfs.join(host.trim_start_matches('/'));
        let target_text = target.to_string_lossy().into_owned();
        mounts(Some(host), &target_text, None, MS_BIND | MS_REC)?;
        targets.push(target_text);
    }
    for target in &targets {
        remount_read_only_tree(target, mounts)?;
    }
    for name in DEVICE_FILES {
        let target = rootfs.join("dev").join(name);
        if !target.exists() {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&target)
                .map_err(|err| py_os_error_path(&err, &target))?;
        }
        mounts(
            Some(&format!("/dev/{}", name)),
            &target.to_string_lossy(),
            None,
            MS_BIND,
        )?;
    }
    let work = rootfs.join("work");
    mounts(Some("tmpfs"), &work.to_string_lossy(), Some("tmpfs"), 0)?;
    let proc_dir = rootfs.join("proc");
    let proc_mounted = mounts(Some("proc"), &proc_dir.to_string_lossy(), Some("proc"), 0).is_ok();
    write_provenance_init(rootfs);
    Ok(proc_mounted)
}

pub fn assemble_root(rootfs: &Path) -> Result<bool, String> {
    assemble_root_with(rootfs, &real_mount)
}

/// Write the run provenance beside the prepared root.
///
/// The runner writes the provenance file when it creates the
/// sandbox directory, and that copy is what the orphan sweep
/// trusts. A janitor whose supervisor was killed can remove
/// the whole directory while this assembly is still under
/// way, and the assembly then recreates the rootfs shape on
/// the same path, so a skeleton left that way carried no
/// provenance and no sweep would ever collect it. The init
/// therefore writes the provenance when its creation work
/// begins and again when that work is done, naming the run
/// the directory name already carries, so any residue this
/// assembly leaves stands under the provenance it was born
/// with, and a removal that lands before the assembly starts
/// meets the write at the assembly's first step rather than
/// only at its last. The write lands through
/// a temporary file in the same directory that is renamed
/// over the provenance path, so a death inside the write
/// leaves the runner copy standing whole and never a
/// truncated file that names nothing. A failure to write it
/// is swallowed, because the runner copy may still stand and
/// a missing provenance must never fail a run whose sandbox
/// is otherwise sound.
fn write_provenance_init(rootfs: &Path) {
    let absolute = std::path::absolute(rootfs).unwrap_or_else(|_| rootfs.to_path_buf());
    let sandbox: &Path = match absolute.parent() {
        Some(parent) => parent,
        None => return,
    };
    let name = match sandbox.file_name().and_then(|n| n.to_str()) {
        Some(name) => name,
        None => return,
    };
    if !name.starts_with("run-") {
        return;
    }
    let run_id = &name["run-".len()..];
    let _ = crate::write_provenance(sandbox, run_id);
}

/// Pivot into the prepared root and detach the old root.
pub fn enter_root(rootfs: &Path) -> Result<(), String> {
    let number = match table_lookup(SYS_PIVOT_ROOT, arch_name()) {
        Some(number) => number,
        None => {
            return Err("[Errno 38] pivot_root is not available on this architecture".to_string())
        }
    };
    let rootfs_text = rootfs.to_string_lossy().into_owned();
    real_mount(Some(&rootfs_text), &rootfs_text, None, MS_BIND | MS_REC)?;
    let oldroot = rootfs.join(".oldroot");
    std::fs::create_dir_all(&oldroot).map_err(|err| py_os_error_path(&err, &oldroot))?;
    let new_root = cstring(&rootfs_text);
    let put_old = cstring(&oldroot.to_string_lossy());
    // Both paths are CStrings owned here and alive for the
    // call, and the syscall number was resolved for this
    // architecture before the call.
    let rc = unsafe { libc::syscall(number, new_root.as_ptr(), put_old.as_ptr()) };
    if rc != 0 {
        let errno = errno_now();
        return Err(format!(
            "[Errno {}] {}: '{}'",
            errno,
            strerror(errno),
            rootfs_text
        ));
    }
    std::env::set_current_dir("/").map_err(|err| py_os_error_path(&err, Path::new("/")))?;
    let old = cstring("/.oldroot");
    // The path is a CString owned here and the detach flag is
    // a plain value, and the old root was just placed there by
    // the pivot above.
    let rc = unsafe { libc::umount2(old.as_ptr(), MNT_DETACH) };
    if rc != 0 {
        let errno = errno_now();
        return Err(format!(
            "[Errno {}] {}: '/.oldroot'",
            errno,
            strerror(errno)
        ));
    }
    std::fs::remove_dir("/.oldroot")
        .map_err(|err| py_os_error_path(&err, Path::new("/.oldroot")))?;
    Ok(())
}

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

/// Empty the capability sets of the calling process.
///
/// Every capability leaves the bounding set first, so no exec can
/// bring one back, and the effective and permitted and inheritable
/// sets are then set empty in one capset call. The error carries
/// the errno and its message, in the shape the exec error pipe
/// reports.
fn drop_capabilities() -> Result<(), (i32, String)> {
    // The header and data are repr C structs built to the
    // kernel layout for version 3 and owned by this call, and
    // the prctl drops pass plain capability numbers only.
    unsafe {
        for cap in 0..64 {
            libc::prctl(PR_CAPBSET_DROP, cap as libc::c_ulong, 0, 0, 0);
        }
        let header = CapHeader {
            version: CAPSET_VERSION_3,
            pid: 0,
        };
        let data = [CapData {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        }; 2];
        let rc = libc::syscall(libc::SYS_capset, &header as *const CapHeader, data.as_ptr());
        if rc != 0 {
            let errno = errno_now();
            return Err((errno, strerror(errno)));
        }
    }
    Ok(())
}

fn status(status_fd: i32, event: serde_json::Value) {
    let mut payload = event;
    if let Some(object) = payload.as_object_mut() {
        object.insert("ts".to_string(), serde_json::json!(now()));
    }
    let mut line = serde_json::to_string(&payload).unwrap_or_default();
    line.push('\n');
    // The buffer is the line just built and outlives the write,
    // and the descriptor is the status descriptor the init was
    // handed for exactly these writes.
    unsafe {
        libc::write(status_fd, line.as_ptr() as *const libc::c_void, line.len());
    }
}

fn status_event(status_fd: i32, fields: serde_json::Value) {
    status(status_fd, fields);
}

fn fail(status_fd: i32, detail: &str) -> i32 {
    status_event(
        status_fd,
        serde_json::json!({"event": "start_error", "detail": detail}),
    );
    RESERVED_EXIT
}

/// Die with the task when the host supervisor goes away.
///
/// The runner holds the read end of the status pipe for the
/// whole run. When every read end is gone the poll on this
/// write end reports an error, and the sandbox has no
/// supervisor left to report to, so the task is killed and
/// the namespace dies with the init.
fn watch_supervisor(status_fd: i32, child_slot: Arc<AtomicI32>) {
    loop {
        let child = child_slot.load(Ordering::SeqCst);
        if child == 0 {
            return;
        }
        let mut pfd = libc::pollfd {
            fd: status_fd,
            events: 0,
            revents: 0,
        };
        // The pollfd is owned here and the call writes only its
        // revents field, and the pid killed is the task child
        // the init itself forked and stored in the slot.
        let rc = unsafe { libc::poll(&mut pfd, 1, 200) };
        if rc < 0 {
            return;
        }
        if rc > 0 && (pfd.revents & (libc::POLLERR | libc::POLLHUP)) != 0 {
            unsafe {
                libc::kill(child, libc::SIGKILL);
            }
            std::process::exit(128 + libc::SIGKILL);
        }
    }
}

/// Set the ceilings of MF-13 on the calling process, the
/// forked task child between the fork and the exec, beside
/// the capability drop the child already makes. The processor
/// ceiling stands as a soft limit at the seconds the caller
/// set and a hard limit one second past it, so a task that
/// spends to the soft limit dies by the processor limit
/// signal and a task that catches or ignores that signal dies
/// by the kill signal at the hard limit. The address space
/// ceiling stands as one limit at the bytes the caller set.
/// A ceiling the run does not carry makes no call for it, so
/// an unbounded run's child setup is the landed setup
/// unchanged. The error carries the errno and its message, in
/// the shape the exec error pipe reports, so a limit the
/// machine refuses lands as a start failure in the standing
/// shape and never as a task outcome.
fn set_task_limits(
    max_cpu_seconds: Option<u64>,
    max_memory_bytes: Option<u64>,
) -> Result<(), (i32, String)> {
    // Each limit is a plain pair of counts built for the call,
    // and the call writes nothing through a pointer.
    unsafe {
        if let Some(seconds) = max_cpu_seconds {
            let limit = libc::rlimit {
                rlim_cur: seconds,
                rlim_max: seconds + 1,
            };
            if libc::setrlimit(libc::RLIMIT_CPU, &limit) != 0 {
                let errno = errno_now();
                return Err((errno, strerror(errno)));
            }
        }
        if let Some(bytes) = max_memory_bytes {
            let limit = libc::rlimit {
                rlim_cur: bytes,
                rlim_max: bytes,
            };
            if libc::setrlimit(libc::RLIMIT_AS, &limit) != 0 {
                let errno = errno_now();
                return Err((errno, strerror(errno)));
            }
        }
    }
    Ok(())
}

fn supervise(
    status_fd: i32,
    command: &[OsString],
    max_cpu_seconds: Option<u64>,
    max_memory_bytes: Option<u64>,
) -> i32 {
    let mut pipe_fds: [i32; 2] = [0, 0];
    // The array is owned by this call and holds exactly the two
    // descriptors pipe2 promises to write.
    if unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return 1;
    }
    let (read_fd, write_fd) = (pipe_fds[0], pipe_fds[1]);
    // Prepare the exec arguments before the fork, so the child
    // only touches async signal safe calls on its way to exec.
    // The limit calls the child makes are in that same class,
    // plain system calls that pass counts and write nothing
    // through a pointer.
    let argv: Vec<CString> = command
        .iter()
        .map(|arg| {
            CString::new(arg.as_bytes()).unwrap_or_else(|_| CString::new("").expect("empty"))
        })
        .collect();
    let mut argv_ptrs: Vec<*const libc::c_char> = argv.iter().map(|arg| arg.as_ptr()).collect();
    argv_ptrs.push(std::ptr::null());
    let file_ptr = argv_ptrs[0];
    // The fork is the one place the init multiplies itself, and
    // everything the child touches on its way to exec was built
    // above, before the fork, the argument strings and their
    // pointer vector and the pipe descriptors. The child calls
    // only async signal safe functions, close and the limit
    // calls and the capability drop and execvp and a write of
    // plain bytes and _exit, and
    // allocates nothing, so no lock or buffer the parent holds
    // can be met in a copied and locked state. The parent keeps
    // ownership of the argument storage and drops it only after
    // its own copies of the pipe ends are closed.
    let child = unsafe { libc::fork() };
    if child == 0 {
        // In the child. Every call here is on descriptors this
        // function created, and the exec replaces the process
        // image whole on success, so no Rust state past this
        // point is ever observed by the child.
        unsafe {
            libc::close(read_fd);
            libc::close(status_fd);
            // The limits are set first and the capabilities
            // dropped second, and a failure of either answers
            // through the exec error pipe below.
            let failure = match set_task_limits(max_cpu_seconds, max_memory_bytes) {
                Ok(()) => match drop_capabilities() {
                    Ok(()) => {
                        libc::execvp(file_ptr, argv_ptrs.as_ptr());
                        Some((errno_now(), strerror(errno_now())))
                    }
                    Err(err) => Some(err),
                },
                Err(err) => Some(err),
            };
            if let Some((errno, message)) = failure {
                let payload = format!("{} {}", errno, message);
                libc::write(
                    write_fd,
                    payload.as_ptr() as *const libc::c_void,
                    payload.len(),
                );
            }
            libc::_exit(127);
        }
    }
    if child < 0 {
        return 1;
    }
    drop(argv);
    drop(argv_ptrs);
    let child_slot = Arc::new(AtomicI32::new(child));
    {
        let slot = Arc::clone(&child_slot);
        std::thread::spawn(move || watch_supervisor(status_fd, slot));
    }
    // The write end is this function's own descriptor and the
    // child holds its copy already, so closing here is what
    // makes the read below report the child's exec by its
    // silence.
    unsafe {
        libc::close(write_fd);
    }
    let mut report_buf = [0u8; 4096];
    // The buffer is owned here and sized to the read, and the
    // descriptor is the read end of the exec pipe the child
    // writes its one failure report to.
    let read_count = unsafe {
        libc::read(
            read_fd,
            report_buf.as_mut_ptr() as *mut libc::c_void,
            report_buf.len(),
        )
    };
    // The read end is closed exactly once, here, after the one
    // read the protocol makes on it.
    unsafe {
        libc::close(read_fd);
    }
    if read_count > 0 {
        let mut status_word: i32 = 0;
        // The status word is owned storage, and the child is
        // the one this function forked and has not yet reaped.
        unsafe {
            libc::waitpid(child, &mut status_word, 0);
        }
        let report = String::from_utf8_lossy(&report_buf[..read_count as usize]).into_owned();
        return fail(status_fd, &format!("could not start the task, {}", report));
    }
    status_event(status_fd, serde_json::json!({"event": "started"}));
    let mut exit_code: Option<i32> = None;
    let mut signal_no: Option<i32> = None;
    loop {
        let mut status_word: i32 = 0;
        // The status word is owned storage, and the wait is on
        // any child of this init, which has exactly the task
        // tree under it.
        let pid = unsafe { libc::waitpid(-1, &mut status_word, 0) };
        if pid < 0 {
            let errno = errno_now();
            if errno == libc::EINTR {
                continue;
            }
            break;
        }
        if pid == child {
            if libc::WIFEXITED(status_word) {
                exit_code = Some(libc::WEXITSTATUS(status_word));
            } else if libc::WIFSIGNALED(status_word) {
                signal_no = Some(libc::WTERMSIG(status_word));
            }
            break;
        }
    }
    loop {
        let mut status_word: i32 = 0;
        // The same wait in its non blocking form, draining any
        // grandchildren the task left behind so none of them is
        // left unreaped under this init.
        let pid = unsafe { libc::waitpid(-1, &mut status_word, libc::WNOHANG) };
        if pid <= 0 {
            break;
        }
    }
    child_slot.store(0, Ordering::SeqCst);
    if let Some(signal) = signal_no {
        status_event(
            status_fd,
            serde_json::json!({"event": "task_done", "signal": signal}),
        );
        return 128 + signal;
    }
    match exit_code {
        None => fail(
            status_fd,
            "the task vanished before its wait status was read",
        ),
        Some(code) => {
            status_event(
                status_fd,
                serde_json::json!({"event": "task_done", "exit_code": code}),
            );
            code
        }
    }
}

fn set_hostname() -> Result<(), String> {
    let name = cstring(HOSTNAME);
    // The name is a CString owned here and its length is the
    // constant's own length, so the pointer and length agree.
    let rc = unsafe { libc::sethostname(name.as_ptr(), HOSTNAME.len()) };
    if rc != 0 {
        let errno = errno_now();
        return Err(format!("[Errno {}] {}", errno, strerror(errno)));
    }
    Ok(())
}

fn opt_value<'a>(opts: &'a [OsString], name: &str) -> Option<&'a OsString> {
    opts.iter()
        .position(|arg| arg.as_os_str() == name)
        .and_then(|index| opts.get(index + 1))
}

/// The init entry point, reached through the hidden __init
/// subcommand of the mayfly binary with the argv the unshare
/// call built.
pub fn init_main(args: &[OsString]) -> i32 {
    // Arm the parent death signal before anything else, so an
    // init whose supervisor dies at any later point in the
    // assembly or the supervision dies with it. A failure to
    // arm is an init failure reported like any other, never a
    // run that continues unarmed, and the report goes to the
    // status descriptor the arguments carry where they carry
    // one. No check that the parent still lives follows the
    // arming. Inside the pid namespace the init is pid 1 and
    // its parent stands outside the namespace, so a parent
    // reading cannot tell a live supervisor from a dead one,
    // and the arming itself closes the race the check would
    // chase, because a parent already gone when the signal is
    // armed delivers the signal at once.
    // The call passes plain scalar arguments only and writes
    // nothing through a pointer.
    let armed = unsafe { libc::prctl(PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong, 0, 0, 0) };
    if armed != 0 {
        let errno = errno_now();
        let detail = format!(
            "the parent death signal could not be armed, [Errno {}] {}",
            errno,
            strerror(errno)
        );
        let opts_end = args
            .iter()
            .position(|arg| arg.as_os_str() == "--")
            .unwrap_or(args.len());
        return match opt_value(&args[..opts_end], "--status-fd")
            .and_then(|value| value.to_str())
            .and_then(|text| text.parse().ok())
        {
            Some(fd) => fail(fd, &detail),
            None => 1,
        };
    }
    let sep = match args.iter().position(|arg| arg.as_os_str() == "--") {
        Some(index) => index,
        None => return 1,
    };
    let opts = &args[..sep];
    let command = &args[sep + 1..];
    let rootfs = match opt_value(opts, "--rootfs") {
        Some(value) => PathBuf::from(value),
        None => return 1,
    };
    let status_fd: i32 = match opt_value(opts, "--status-fd")
        .and_then(|value| value.to_str())
        .and_then(|text| text.parse().ok())
    {
        Some(fd) => fd,
        None => return 1,
    };
    // The hidden ceiling options the supervisor placed after
    // the status descriptor option, present only where the
    // caller set the bound. A value that does not read as a
    // count names no ceiling, as an absent option does not,
    // because the supervisor writes only counts it validated.
    let max_cpu_seconds: Option<u64> = opt_value(opts, "--max-cpu")
        .and_then(|value| value.to_str())
        .and_then(|text| text.parse().ok());
    let max_memory_bytes: Option<u64> = opt_value(opts, "--max-memory")
        .and_then(|value| value.to_str())
        .and_then(|text| text.parse().ok());
    if command.is_empty() {
        return fail(status_fd, "no command was given");
    }
    // The internal token rode in with the environment the
    // supervisor built. It has done its work of naming this
    // process as one of the run's own children, and it leaves
    // the environment here, before the assembly and the task,
    // so no process inside the sandbox can read it.
    std::env::remove_var(crate::INTERNAL_TOKEN_ENV);
    let assembled = assemble_root(&rootfs)
        .and_then(|proc_mounted| set_hostname().map(|_| proc_mounted))
        .and_then(|proc_mounted| enter_root(&rootfs).map(|_| proc_mounted))
        .and_then(|proc_mounted| {
            std::env::set_current_dir("/work")
                .map_err(|err| py_os_error_path(&err, Path::new("/work")))
                .map(|_| proc_mounted)
        });
    let proc_mounted = match assembled {
        Ok(proc_mounted) => proc_mounted,
        Err(detail) => {
            return fail(status_fd, &format!("the prepared root failed, {}", detail));
        }
    };
    status_event(
        status_fd,
        serde_json::json!({"event": "ready", "proc_mounted": proc_mounted}),
    );
    supervise(status_fd, command, max_cpu_seconds, max_memory_bytes)
}

/// The assembly drive the test suite uses to run the assembly
/// inside a real namespace with no runner, reached through
/// the hidden __assemble-root subcommand.
///
/// The drive performs its mounts in the mount namespace of
/// the process that invokes it, and unlike the init it does
/// not unshare first, so invoked directly on a host it would
/// mount into the host mount table. It therefore refuses to
/// run unless the caller presents the internal run token, the
/// per run value the supervisor places in the environment of
/// its own children under the internal token key, in the
/// token shape the generator produces. A caller who can set
/// that environment already holds the mount privilege the
/// assembly needs, so the gate is a guard against accident
/// and not a boundary, and it is checked before any mount.
pub fn assemble_root_main(args: &[OsString]) -> i32 {
    match std::env::var(crate::INTERNAL_TOKEN_ENV) {
        Ok(value) if crate::token_shape_ok(&value) => {}
        _ => {
            eprintln!("mayfly __assemble-root refuses to run without the internal run token");
            return 1;
        }
    }
    let rootfs = match opt_value(args, "--rootfs") {
        Some(value) => PathBuf::from(value),
        None => return 1,
    };
    match assemble_root(&rootfs) {
        Ok(_) => 0,
        Err(detail) => {
            eprintln!("the prepared root failed, {}", detail);
            1
        }
    }
}
