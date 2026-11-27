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

use crate::{now, py_os_error, py_os_error_path, strerror};

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

fn errno_pair(errno: i32) -> (i32, String) {
    (errno, strerror(errno))
}

fn cstring(text: &str) -> CString {
    CString::new(text).unwrap_or_else(|_| CString::new("").expect("empty cstring"))
}

fn cstring_path(path: &Path) -> Result<CString, String> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| py_os_error_path(&io::Error::from_raw_os_error(libc::EINVAL), path))
}

/// One mount call, in the shape the assembly drives it. The
/// error text is the str of the OSError the Python init raised
/// for the same call, errno and message and target.
pub type MountFn<'a> = dyn Fn(Option<&str>, &str, Option<&str>, u64) -> Result<(), String> + 'a;

/// One mount call carrying an optional filesystem option string.
pub type MountOptsFn<'a> =
    dyn Fn(Option<&str>, &str, Option<&str>, u64, Option<&str>) -> Result<(), String> + 'a;

type MountPathFn<'a> =
    dyn Fn(Option<&Path>, &Path, Option<&str>, u64, Option<&str>) -> Result<(), String> + 'a;

fn real_mount_path_opts(
    source: Option<&Path>,
    target: &Path,
    fstype: Option<&str>,
    flags: u64,
    data: Option<&str>,
) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        let src = match source {
            Some(p) => Some(cstring_path(p)?),
            None => None,
        };
        let fs = fstype.map(cstring);
        let tgt = cstring_path(target)?;
        let opt = data.map(cstring);
        // Every pointer is either null or borrowed from a CString
        // this call owns and keeps alive until the call returns,
        // and the flags are plain values.
        let rc = unsafe {
            libc::mount(
                src.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
                tgt.as_ptr(),
                fs.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
                flags as libc::c_ulong,
                opt.as_ref()
                    .map_or(std::ptr::null(), |d| d.as_ptr() as *const libc::c_void),
            )
        };
        if rc != 0 {
            let errno = errno_now();
            return Err(py_os_error_path(
                &io::Error::from_raw_os_error(errno),
                target,
            ));
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        if let Some(p) = source {
            let _ = cstring_path(p)?;
        }
        let _ = cstring_path(target)?;
        let _ = (fstype.map(cstring), flags, data.map(cstring));
        Err(py_os_error_path(
            &io::Error::from_raw_os_error(libc::ENOSYS),
            target,
        ))
    }
}

pub fn real_mount_opts(
    source: Option<&str>,
    target: &str,
    fstype: Option<&str>,
    flags: u64,
    data: Option<&str>,
) -> Result<(), String> {
    real_mount_path_opts(
        source.map(Path::new),
        Path::new(target),
        fstype,
        flags,
        data,
    )
}

pub fn real_mount(
    source: Option<&str>,
    target: &str,
    fstype: Option<&str>,
    flags: u64,
) -> Result<(), String> {
    real_mount_opts(source, target, fstype, flags, None)
}

/// Decode the escapes the kernel writes into a mount point
/// field of the mounts table into raw bytes without lossy UTF-8
/// substitution.
pub fn decode_mount_point_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\'
            && index + 3 < bytes.len()
            && matches!(bytes[index + 1], b'0'..=b'3')
            && matches!(bytes[index + 2], b'0'..=b'7')
            && matches!(bytes[index + 3], b'0'..=b'7')
        {
            let value = (bytes[index + 1] - b'0') as u32 * 64
                + (bytes[index + 2] - b'0') as u32 * 8
                + (bytes[index + 3] - b'0') as u32;
            out.push(value as u8);
            index += 4;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    out
}

/// Decode the escapes the kernel writes into a mount point
/// field of the mounts table. A space in a mount point stands
/// in the table as a backslash followed by three octal digits,
/// and so do a tab and a newline and a backslash itself, so a
/// literal comparison against the real path never matches a
/// tree bound under an escaped path. Anything that is not a
/// backslash followed by three octal digits that name a byte
/// stands as itself, and an escape whose first digit stands
/// above 3 names no byte and stands as itself with the rest.
pub fn decode_mount_point(field: &str) -> String {
    String::from_utf8_lossy(&decode_mount_point_bytes(field.as_bytes())).into_owned()
}

/// Set the read only flag on a bound tree and every submount.
///
/// The recursive mount_setattr call carries the claim where the
/// kernel offers it. The walk behind it remounts each mount point
/// under the target one by one, deepest first, for kernels where
/// the call is refused.
fn remount_read_only_tree_path(target: &Path, mount_path: &MountPathFn) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    if let Some(number) = table_lookup(SYS_MOUNT_SETATTR, arch_name()) {
        // The mount attr structure is five u64 words, with the
        // set mask in the first word and the size passed as 40.
        let attr: [u64; 5] = [MOUNT_ATTR_RDONLY, 0, 0, 0, 0];
        let tgt = cstring_path(target)?;
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
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (
            table_lookup(SYS_MOUNT_SETATTR, arch_name()),
            AT_FDCWD,
            AT_RECURSIVE,
            MOUNT_ATTR_RDONLY,
            decode_mount_point(""),
        );
        mount_path(
            Some(target),
            target,
            None,
            MS_BIND | MS_REMOUNT | MS_RDONLY,
            None,
        )
    }
    #[cfg(target_os = "linux")]
    {
        let mounts_raw = std::fs::read("/proc/self/mounts")
            .map_err(|err| py_os_error_path(&err, Path::new("/proc/self/mounts")))?;
        let mut points: Vec<Vec<u8>> = Vec::new();
        for line in mounts_raw.split(|&b| b == b'\n') {
            let fields: Vec<&[u8]> = line
                .split(|&b| b == b' ' || b == b'\t' || b == b'\r')
                .filter(|s| !s.is_empty())
                .collect();
            if fields.len() >= 2 {
                points.push(decode_mount_point_bytes(fields[1]));
            }
        }
        let target_bytes = target.as_os_str().as_bytes();
        let mut under: Vec<Vec<u8>> = points
            .into_iter()
            .filter(|p| {
                p.as_slice() == target_bytes
                    || (p.len() > target_bytes.len()
                        && p.starts_with(target_bytes)
                        && p[target_bytes.len()] == b'/')
            })
            .collect();
        under.sort_by_key(|point| std::cmp::Reverse(point.len()));
        for point in under {
            let point_path = Path::new(std::ffi::OsStr::from_bytes(&point));
            mount_path(
                Some(point_path),
                point_path,
                None,
                MS_BIND | MS_REMOUNT | MS_RDONLY,
                None,
            )?;
        }
        Ok(())
    }
}

fn ensure_dir_nofollow(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(py_os_error_path(
                    &io::Error::from_raw_os_error(libc::ENOTDIR),
                    path,
                ));
            }
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            std::fs::create_dir(path).map_err(|e| py_os_error_path(&e, path))
        }
        Err(err) => Err(py_os_error_path(&err, path)),
    }
}

fn create_regular_target_nofollow(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o644)
        .open(path)
        .map(|_| ())
        .map_err(|err| py_os_error_path(&err, path))
}

fn assemble_root_inner(
    rootfs: &Path,
    ro_binds: &[(PathBuf, PathBuf)],
    mount_path: &MountPathFn,
) -> Result<bool, String> {
    write_provenance_init(rootfs);
    mount_path(None, Path::new("/"), None, MS_REC | MS_PRIVATE, None)?;
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
    let mut targets: Vec<PathBuf> = Vec::new();
    for host in &bind_sources {
        let target = rootfs.join(host.trim_start_matches('/'));
        mount_path(Some(Path::new(host)), &target, None, MS_BIND | MS_REC, None)?;
        targets.push(target);
    }
    for target in &targets {
        remount_read_only_tree_path(target, mount_path)?;
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
        let dev_src = format!("/dev/{}", name);
        mount_path(Some(Path::new(&dev_src)), &target, None, MS_BIND, None)?;
    }
    let work = rootfs.join("work");
    mount_path(Some(Path::new("tmpfs")), &work, Some("tmpfs"), 0, None)?;
    let tmp = rootfs.join("tmp");
    mount_path(
        Some(Path::new("tmpfs")),
        &tmp,
        Some("tmpfs"),
        0,
        Some("mode=1777"),
    )?;
    if !ro_binds.is_empty() {
        let norm_dests = crate::namespace::validate_ro_bind_destinations(ro_binds)?;
        for ((host_src, _), norm_dst) in ro_binds.iter().zip(norm_dests) {
            let (clean_src, is_dir) = crate::namespace::inspect_ro_bind_source(host_src)?;
            let rel_bytes = norm_dst
                .as_os_str()
                .as_bytes()
                .strip_prefix(b"/")
                .unwrap_or(b"");
            let parts: Vec<&[u8]> = rel_bytes
                .split(|&b| b == b'/')
                .filter(|s| !s.is_empty())
                .collect();
            let mut cursor = rootfs.to_path_buf();
            for parent_part in &parts[..parts.len() - 1] {
                cursor.push(std::ffi::OsStr::from_bytes(parent_part));
                ensure_dir_nofollow(&cursor)?;
            }
            cursor.push(std::ffi::OsStr::from_bytes(parts[parts.len() - 1]));
            if is_dir {
                ensure_dir_nofollow(&cursor)?;
                mount_path(Some(&clean_src), &cursor, None, MS_BIND | MS_REC, None)?;
            } else {
                create_regular_target_nofollow(&cursor)?;
                mount_path(Some(&clean_src), &cursor, None, MS_BIND, None)?;
            }
            remount_read_only_tree_path(&cursor, mount_path)?;
        }
    }
    let proc_dir = rootfs.join("proc");
    let proc_mounted =
        mount_path(Some(Path::new("proc")), &proc_dir, Some("proc"), 0, None).is_ok();
    write_provenance_init(rootfs);
    Ok(proc_mounted)
}

fn path_to_str(p: &Path) -> Result<&str, String> {
    p.to_str()
        .ok_or_else(|| py_os_error_path(&io::Error::from_raw_os_error(libc::EINVAL), p))
}

/// Build the prepared root with optional read-only binds and return whether proc was mounted.
pub fn assemble_root_with_binds(
    rootfs: &Path,
    ro_binds: &[(PathBuf, PathBuf)],
    mounts: &MountFn,
    mount_opts: &MountOptsFn,
) -> Result<bool, String> {
    let adapt_path = |src: Option<&Path>,
                      target: &Path,
                      fstype: Option<&str>,
                      flags: u64,
                      data: Option<&str>|
     -> Result<(), String> {
        let src_str = match src {
            Some(s) => Some(path_to_str(s)?),
            None => None,
        };
        let target_str = path_to_str(target)?;
        if data.is_some() {
            mount_opts(src_str, target_str, fstype, flags, data)
        } else {
            mounts(src_str, target_str, fstype, flags)
        }
    };
    assemble_root_inner(rootfs, ro_binds, &adapt_path)
}

/// Build the prepared root using raw operating system paths without UTF-8 restriction.
pub fn assemble_root_os(rootfs: &Path, ro_binds: &[(PathBuf, PathBuf)]) -> Result<bool, String> {
    assemble_root_inner(rootfs, ro_binds, &real_mount_path_opts)
}

/// Build the prepared root and return whether proc was mounted.
pub fn assemble_root_with(rootfs: &Path, mounts: &MountFn) -> Result<bool, String> {
    let adapt = |src: Option<&str>,
                 target: &str,
                 fstype: Option<&str>,
                 flags: u64,
                 _data: Option<&str>|
     -> Result<(), String> { mounts(src, target, fstype, flags) };
    assemble_root_with_binds(rootfs, &[], mounts, &adapt)
}

pub fn assemble_root(rootfs: &Path) -> Result<bool, String> {
    assemble_root_os(rootfs, &[])
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
    if !name.starts_with(crate::RUN_DIR_PREFIX) {
        return;
    }
    let run_id = &name[crate::RUN_DIR_PREFIX.len()..];
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
    #[cfg(target_os = "linux")]
    {
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
            return Err(py_os_error_path(
                &io::Error::from_raw_os_error(errno),
                Path::new(&rootfs_text),
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
            return Err(py_os_error_path(
                &io::Error::from_raw_os_error(errno),
                Path::new("/.oldroot"),
            ));
        }
        std::fs::remove_dir("/.oldroot")
            .map_err(|err| py_os_error_path(&err, Path::new("/.oldroot")))?;
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (number, MNT_DETACH);
        Err(py_os_error_path(
            &io::Error::from_raw_os_error(libc::ENOSYS),
            rootfs,
        ))
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[repr(C)]
struct CapHeader {
    version: u32,
    pid: i32,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
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
    #[cfg(target_os = "linux")]
    {
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
                return Err(errno_pair(errno_now()));
            }
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (PR_CAPBSET_DROP, CAPSET_VERSION_3);
        Err(errno_pair(libc::ENOSYS))
    }
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

fn fail(status_fd: i32, detail: &str) -> i32 {
    status(
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
                return Err(errno_pair(errno_now()));
            }
        }
        if let Some(bytes) = max_memory_bytes {
            let limit = libc::rlimit {
                rlim_cur: bytes,
                rlim_max: bytes,
            };
            if libc::setrlimit(libc::RLIMIT_AS, &limit) != 0 {
                return Err(errno_pair(errno_now()));
            }
        }
    }
    Ok(())
}

fn create_exec_pipe(pipe_fds: &mut [i32; 2]) -> Result<(), i32> {
    #[cfg(target_os = "linux")]
    {
        if unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(errno_now());
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } != 0 {
            return Err(errno_now());
        }
        unsafe {
            for &fd in pipe_fds.iter() {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags >= 0 {
                    libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
                }
            }
        }
        Ok(())
    }
}

fn supervise(
    status_fd: i32,
    command: &[OsString],
    max_cpu_seconds: Option<u64>,
    max_memory_bytes: Option<u64>,
) -> i32 {
    let mut pipe_fds: [i32; 2] = [0, 0];
    if let Err(errno) = create_exec_pipe(&mut pipe_fds) {
        let (errno, message) = errno_pair(errno);
        return fail(
            status_fd,
            &format!(
                "could not start the task, could not create the exec pipe, {} {}",
                errno, message
            ),
        );
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
    // pointer vector and the pipe descriptors. On its way to a
    // successful exec the child calls only async signal safe
    // functions, close and the limit calls and the capability
    // drop and execvp and a write of plain bytes and _exit, and
    // allocates nothing, so no lock or buffer the parent holds
    // can be met in a copied and locked state. A limit or
    // capability failure is the one path that does more, in the
    // child, formatting its report into a fresh string and
    // reading the error text through strerror before it writes
    // the report to the pipe and exits. That path stands on a
    // narrower ground than the exec path. The init is single
    // threaded at the fork, because the one other thread it
    // runs is spawned by the parent below after the fork
    // returns, so no allocation the child makes there can meet
    // a lock another thread holds. The parent keeps ownership
    // of the argument storage and drops it only after its own
    // copies of the pipe ends are closed.
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
                        Some(errno_pair(errno_now()))
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
        let (errno, message) = errno_pair(errno_now());
        return fail(
            status_fd,
            &format!(
                "could not start the task, could not fork the task process, {} {}",
                errno, message
            ),
        );
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
    status(status_fd, serde_json::json!({"event": "started"}));
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
        status(
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
            status(
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
    #[cfg(target_os = "linux")]
    let len = HOSTNAME.len();
    #[cfg(not(target_os = "linux"))]
    let len = HOSTNAME.len() as libc::c_int;
    let rc = unsafe { libc::sethostname(name.as_ptr(), len) };
    if rc != 0 {
        let errno = errno_now();
        return Err(py_os_error(&io::Error::from_raw_os_error(errno)));
    }
    Ok(())
}

fn extract_status_fd(args: &[OsString]) -> Option<i32> {
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_os_str();
        if flag == "--" {
            break;
        }
        if flag == "--status-fd" {
            return args
                .get(index + 1)
                .and_then(|v| v.to_str())
                .and_then(|t| t.parse().ok());
        }
        if flag == "--rootfs" || flag == "--max-cpu" || flag == "--max-memory" {
            index += 2;
            continue;
        }
        if flag == "--ro-bind-src" {
            if args.get(index + 2).map(OsString::as_os_str)
                == Some(std::ffi::OsStr::new("--ro-bind-dst"))
            {
                index += 4;
            } else {
                index += 2;
            }
            continue;
        }
        index += 1;
    }
    None
}

struct InitArgs {
    rootfs: PathBuf,
    status_fd: i32,
    max_cpu_seconds: Option<u64>,
    max_memory_bytes: Option<u64>,
    ro_binds: Vec<(PathBuf, PathBuf)>,
    command: Vec<OsString>,
}

enum InitParseFailure {
    Silent,
    PreparedRoot(i32, String),
    NoCommand(i32),
}

fn parse_init_args(args: &[OsString]) -> Result<InitArgs, InitParseFailure> {
    let mut rootfs: Option<PathBuf> = None;
    let mut status_fd: Option<i32> = None;
    let mut max_cpu_seconds: Option<u64> = None;
    let mut max_memory_bytes: Option<u64> = None;
    let mut ro_binds: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut command: Option<Vec<OsString>> = None;
    let mut index = 0;
    let report_err = |seen_fd: Option<i32>, msg: String| -> InitParseFailure {
        match seen_fd.or_else(|| extract_status_fd(args)) {
            Some(fd) => InitParseFailure::PreparedRoot(fd, msg),
            None => InitParseFailure::Silent,
        }
    };
    while index < args.len() {
        let flag = args[index].as_os_str();
        if flag == "--" {
            command = Some(args[index + 1..].to_vec());
            break;
        }
        if flag == "--rootfs" {
            let Some(val) = args.get(index + 1) else {
                return Err(InitParseFailure::Silent);
            };
            rootfs = Some(PathBuf::from(val));
            index += 2;
            continue;
        }
        if flag == "--status-fd" {
            let Some(fd) = args
                .get(index + 1)
                .and_then(|v| v.to_str())
                .and_then(|t| t.parse::<i32>().ok())
            else {
                return Err(InitParseFailure::Silent);
            };
            status_fd = Some(fd);
            index += 2;
            continue;
        }
        if flag == "--max-cpu" {
            let Some(val) = args.get(index + 1) else {
                return Err(InitParseFailure::Silent);
            };
            max_cpu_seconds = val.to_str().and_then(|t| t.parse().ok());
            index += 2;
            continue;
        }
        if flag == "--max-memory" {
            let Some(val) = args.get(index + 1) else {
                return Err(InitParseFailure::Silent);
            };
            max_memory_bytes = val.to_str().and_then(|t| t.parse().ok());
            index += 2;
            continue;
        }
        if flag == "--ro-bind-src" {
            match ro_bind_pair_at(args, index) {
                Ok(pair) => ro_binds.push(pair),
                Err(msg) => return Err(report_err(status_fd, msg)),
            }
            index += 4;
            continue;
        }
        return Err(report_err(
            status_fd,
            format!("unknown init option {}", flag.to_string_lossy()),
        ));
    }
    let Some(rootfs) = rootfs else {
        return Err(InitParseFailure::Silent);
    };
    let Some(status_fd) = status_fd else {
        return Err(InitParseFailure::Silent);
    };
    let Some(command) = command else {
        return Err(InitParseFailure::Silent);
    };
    if command.is_empty() {
        return Err(InitParseFailure::NoCommand(status_fd));
    }
    Ok(InitArgs {
        rootfs,
        status_fd,
        max_cpu_seconds,
        max_memory_bytes,
        ro_binds,
        command,
    })
}

struct AssembleArgs {
    rootfs: PathBuf,
    ro_binds: Vec<(PathBuf, PathBuf)>,
}

fn parse_assemble_args(args: &[OsString]) -> Result<AssembleArgs, Option<String>> {
    let mut rootfs: Option<PathBuf> = None;
    let mut ro_binds: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_os_str();
        if flag == "--rootfs" {
            let Some(val) = args.get(index + 1) else {
                return Err(None);
            };
            rootfs = Some(PathBuf::from(val));
            index += 2;
            continue;
        }
        if flag == "--ro-bind-src" {
            match ro_bind_pair_at(args, index) {
                Ok(pair) => ro_binds.push(pair),
                Err(msg) => return Err(Some(msg)),
            }
            index += 4;
            continue;
        }
        return Err(Some(format!(
            "unknown assemble option {}",
            flag.to_string_lossy()
        )));
    }
    let Some(rootfs) = rootfs else {
        return Err(None);
    };
    Ok(AssembleArgs { rootfs, ro_binds })
}

/// Read the read-only bind pair whose --ro-bind-src flag
/// stands at index in the argument list, answering the pair
/// it reads or the one refusal sentence the reading meets.
/// The two hidden entry points' parsers share this one
/// statement of the pair's grammar, as they share
/// token_refusal for the token check, and each maps the
/// sentence through its own failure channel.
fn ro_bind_pair_at(args: &[OsString], index: usize) -> Result<(PathBuf, PathBuf), String> {
    let Some(src) = args.get(index + 1) else {
        return Err("missing --ro-bind-src value".to_string());
    };
    if args.get(index + 2).map(OsString::as_os_str) != Some(std::ffi::OsStr::new("--ro-bind-dst")) {
        return Err("missing --ro-bind-dst after --ro-bind-src".to_string());
    }
    let Some(dst) = args.get(index + 3) else {
        return Err("missing --ro-bind-dst value".to_string());
    };
    Ok((PathBuf::from(src), PathBuf::from(dst)))
}

/// Write one line to standard error without ever panicking.
///
/// The hidden entry points report their refusals and the
/// assembly drive's failure on standard error, and those
/// reports are the last thing the process does before it
/// exits at 1. A print through eprintln! panics when the
/// write itself fails, on a closed descriptor or a full
/// disk, and the process then dies at 101 under a panic
/// message the same broken stream cannot carry, an exit
/// none of the entry points names. This writes through a
/// locked handle in the shape the command line's refusals
/// take and discards the result, so the exit is 1 on a
/// broken stream as on a sound one.
fn report_line(text: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr().lock(), "{}", text);
}

/// Whether the caller of one of the hidden entry points
/// must refuse to run for want of the internal run token,
/// printing the refusal in the entry point's own name where
/// it must. The check is the token environment value read
/// against the token shape, and the two entry points share
/// this one statement of it.
fn token_refusal(subcommand: &str) -> bool {
    match std::env::var(crate::INTERNAL_TOKEN_ENV) {
        Ok(value) if crate::token_shape_ok(&value) => false,
        _ => {
            report_line(&format!(
                "mayfly {} refuses to run without the internal run token",
                subcommand
            ));
            true
        }
    }
}

/// The init entry point, reached through the hidden __init
/// subcommand of the mayfly binary with the argv the unshare
/// call built.
///
/// Like the assembly drive it refuses to run unless the
/// caller presents the internal run token in its environment,
/// checked before anything is armed or attempted, because a
/// direct invocation assembles the root and supervises the
/// command in the caller's own namespaces, and the gate is
/// the same guard against accident the drive carries.
pub fn init_main(args: &[OsString]) -> i32 {
    // The token check stands first, before the death signal
    // is armed and before anything is attempted, through the
    // one check both hidden entry points share. The init
    // performs the
    // assembly and then sets the hostname, pivots the root,
    // and supervises the command, strictly more than the
    // drive performs on a direct invocation, and the runner's
    // own account of the token names the hidden entry points
    // in the plural as the children it tells from a direct
    // caller. A direct caller is therefore refused before it
    // can arm, assemble, or supervise anything, and the
    // refusal leaves nothing behind, no armed signal, no
    // status event, and nothing created.
    if token_refusal("__init") {
        return 1;
    }
    // Arm the parent death signal before the assembly, so an
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
    #[cfg(target_os = "linux")]
    let arm_errno = {
        let armed =
            unsafe { libc::prctl(PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong, 0, 0, 0) };
        if armed != 0 {
            Some(errno_now())
        } else {
            None
        }
    };
    #[cfg(not(target_os = "linux"))]
    let arm_errno = {
        let _ = PR_SET_PDEATHSIG;
        Some(libc::ENOSYS)
    };
    if let Some(errno) = arm_errno {
        let detail = format!(
            "the parent death signal could not be armed, {}",
            py_os_error(&io::Error::from_raw_os_error(errno))
        );
        return match extract_status_fd(args) {
            Some(fd) => fail(fd, &detail),
            None => 1,
        };
    }
    let parsed = match parse_init_args(args) {
        Ok(parsed) => parsed,
        Err(InitParseFailure::Silent) => return 1,
        Err(InitParseFailure::PreparedRoot(fd, detail)) => {
            return fail(fd, &format!("the prepared root failed, {}", detail));
        }
        Err(InitParseFailure::NoCommand(fd)) => {
            return fail(fd, "no command was given");
        }
    };
    let rootfs = parsed.rootfs;
    let status_fd = parsed.status_fd;
    let max_cpu_seconds = parsed.max_cpu_seconds;
    let max_memory_bytes = parsed.max_memory_bytes;
    let ro_binds = parsed.ro_binds;
    let command = &parsed.command;
    // The internal token rode in with the environment the
    // supervisor built. It has done its work of naming this
    // process as one of the run's own children, and it leaves
    // the environment here, before the assembly and the task,
    // so no process inside the sandbox can read it.
    std::env::remove_var(crate::INTERNAL_TOKEN_ENV);
    let assembled = assemble_root_os(&rootfs, &ro_binds)
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
    status(
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
    if token_refusal("__assemble-root") {
        return 1;
    }
    let parsed = match parse_assemble_args(args) {
        Ok(parsed) => parsed,
        Err(None) => return 1,
        Err(Some(detail)) => {
            report_line(&format!("the prepared root failed, {}", detail));
            return 1;
        }
    };
    match assemble_root_os(&parsed.rootfs, &parsed.ro_binds) {
        Ok(_) => 0,
        Err(detail) => {
            report_line(&format!("the prepared root failed, {}", detail));
            1
        }
    }
}
