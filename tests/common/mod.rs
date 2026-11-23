//! The harness shapes the two test suites share.
//!
//! tests/mayfly.rs and tests/setup.rs are separate crates,
//! and each names this module with mod common. One shape
//! stands here for each job the two suites both do, so the
//! shapes cannot drift apart between the files: the repo
//! root, one temporary directory creation shape, the fifo
//! plant, the environment output parse, and the two printed
//! shapes the report pins compare against, the report field
//! names and the phase names. Tmp wraps the directory shape
//! with the drop cleanup the behaviour pins rely on. An item
//! here may serve only one of the two crates that compile
//! this module, so dead code is allowed module wide.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

pub const REPORT_FIELDS: [&str; 13] = [
    "backend",
    "run_id",
    "sandbox_dir",
    "command",
    "outcome",
    "task_exit_code",
    "task_signal",
    "error",
    "proc_mounted",
    "sandbox_removed",
    "timings_ms",
    "stdout_tail",
    "stderr_tail",
];

pub const PHASE_NAMES: [&str; 5] = ["prepare", "start", "task", "teardown", "total"];

pub fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

static DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

pub fn fresh_dir(tag: &str) -> PathBuf {
    let n = DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir =
        std::env::temp_dir().join(format!("mayfly-test-{}-{}-{}", std::process::id(), n, tag));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub struct Tmp(PathBuf);

impl Tmp {
    pub fn new(tag: &str) -> Tmp {
        Tmp(fresh_dir(tag))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn mkfifo(path: &Path) {
    let cpath = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    let rc = unsafe { libc::mkfifo(cpath.as_ptr(), 0o666) };
    assert_eq!(rc, 0, "mkfifo failed for {}", path.display());
}

pub fn parse_env_output(text: &str) -> BTreeMap<String, String> {
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    for line in text.trim().lines() {
        if let Some((key, value)) = line.split_once('=') {
            seen.insert(key.to_string(), value.to_string());
        }
    }
    seen
}
