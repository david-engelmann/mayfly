//! The mayfly run janitor.
//!
//! One janitor is spawned per run, reached by re-invoking the
//! mayfly binary under the hidden __janitor subcommand. It holds
//! the read end of a pipe the runner holds the write end of, and
//! it watches for the runner to go away. The runner hands the
//! janitor a per run token over its standard input at spawn, a
//! token that appears in no directory listing and in no command
//! line. While the runner lives, the pipe stays open and the
//! janitor sleeps on it. When the runner finishes a run it
//! removes the sandbox itself, leaves a clean marker beside the
//! run root carrying that token, and closes the pipe, and the
//! janitor sees the marker carries its token and exits. A marker
//! without the token was planted by someone else and stands
//! nothing down. When the runner dies without removing the
//! sandbox, the pipe closes with no genuine marker left, and the
//! janitor removes the sandbox directory itself, so a killed
//! supervisor leaves no sandbox directory behind. The removal
//! can land while the run's init is still dying, and the init
//! then recreates the sandbox shape behind the removal, so the
//! janitor watches the removed path for a few seconds afterwards
//! and removes the directory again if it reappears.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const RETRY_SECONDS: f64 = 30.0;
pub const WATCH_SECONDS: f64 = 5.0;

/// Read the per run token the runner wrote to standard input.
///
/// The runner writes the token once at spawn and closes the
/// stream, so this returns as soon as the line or the end of the
/// stream arrives. A janitor whose runner died before writing
/// reads an empty token, and an empty token authenticates nothing.
fn read_token() -> String {
    let mut chunks: Vec<u8> = Vec::new();
    let mut stdin = std::io::stdin().lock();
    let mut buf = [0u8; 4096];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                chunks.extend_from_slice(&buf[..n]);
                if buf[..n].contains(&b'\n') {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&chunks).trim().to_string()
}

/// Whether the marker carries exactly this run's token.
///
/// The marker is opened only where the marker path is a
/// regular file, so a pipe or any other planted object at the
/// path reads as a marker without the token instead of
/// blocking the janitor inside its open while the sandbox
/// stands. The check follows a symlink to the file it names,
/// and a symlink is read through and never removed. The read
/// itself cannot block and is bounded by a limit derived from
/// the token length, and a marker that runs past the limit is
/// not the clean marker whatever its first bytes say. Bytes
/// that are not valid text decode to replacement characters
/// and fail the comparison. A planted marker that is oversized
/// or undecodable therefore reads as a marker without the
/// token, and the sandbox is removed.
fn marker_carries_token(marker: &Path, token: &str) -> bool {
    use std::os::unix::fs::OpenOptionsExt;
    let limit = token.len() + 64;
    match std::fs::metadata(marker) {
        Ok(meta) if meta.is_file() => {}
        _ => return false,
    }
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(marker)
    {
        Ok(file) => file,
        Err(_) => return false,
    };
    let mut buf = vec![0u8; limit + 1];
    let read_count = match file.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return false,
    };
    if read_count > limit {
        return false;
    }
    String::from_utf8_lossy(&buf[..read_count]).trim() == token
}

/// Remove the sandbox again if a dying init recreates it.
///
/// The removal in janitor_main can land while the run's init is
/// still dying, and the init then recreates the sandbox shape on
/// the same path behind the removal, carrying no provenance the
/// orphan sweep could trust. A run id is never reused, so a
/// directory that reappears at this path inside the watch
/// window can only be that dying init recreating its own
/// tree, and never a directory a later live run created at a
/// reused path. The watch is bounded, and the janitor leaves
/// when the window ends whether or not the path stands clear
/// at that moment.
fn watch_for_recreation(sandbox: &Path) {
    let deadline = Instant::now() + Duration::from_secs_f64(WATCH_SECONDS);
    while Instant::now() < deadline {
        if sandbox.exists() {
            let _ = std::fs::remove_dir_all(sandbox);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if sandbox.exists() {
        let _ = std::fs::remove_dir_all(sandbox);
    }
}

/// The janitor entry point, reached through the hidden
/// __janitor subcommand with the argv the runner built.
pub fn janitor_main(args: &[OsString]) -> i32 {
    let mut sandbox: Option<PathBuf> = None;
    let mut marker: Option<PathBuf> = None;
    let mut watch_fd: Option<i32> = None;
    let mut index = 0;
    while index + 1 < args.len() {
        let key = args[index].to_string_lossy().into_owned();
        let value = &args[index + 1];
        match key.as_str() {
            "--sandbox" => sandbox = Some(PathBuf::from(value)),
            "--marker" => marker = Some(PathBuf::from(value)),
            "--fd" => watch_fd = value.to_str().and_then(|text| text.parse().ok()),
            _ => {}
        }
        index += 2;
    }
    let (sandbox, marker, watch_fd) = match (sandbox, marker, watch_fd) {
        (Some(s), Some(m), Some(f)) => (s, m, f),
        _ => return 1,
    };
    let token = read_token();
    let mut buf = [0u8; 4096];
    loop {
        // The buffer is owned here and sized to the read, and
        // the descriptor is the watch pipe the runner handed
        // this janitor, read until the runner closing it ends
        // the wait.
        let rc = unsafe { libc::read(watch_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if rc <= 0 {
            break;
        }
    }
    // The watch descriptor is closed exactly once, here, when
    // the wait it served is over.
    unsafe {
        libc::close(watch_fd);
    }
    if !token.is_empty() && marker_carries_token(&marker, &token) {
        let _ = std::fs::remove_file(&marker);
        return 0;
    }
    let deadline = Instant::now() + Duration::from_secs_f64(RETRY_SECONDS);
    loop {
        let _ = std::fs::remove_dir_all(&sandbox);
        if !sandbox.exists() {
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = std::fs::remove_file(&marker);
    if !sandbox.exists() {
        watch_for_recreation(&sandbox);
    }
    0
}
