//! Self-contained Rust quickstart for mayfly.
//!
//! Demonstrates backend registry inspection, RunSpec construction, pre-prepare
//! resource-bound and read-only-bind validation on every Unix platform, and a
//! full isolated namespace sandbox task on Linux hosts permitting unprivileged
//! user namespaces.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use mayfly::{backend_names, get_backend, Outcome, RunSpec, TIMEOUT_FLOOR_SECONDS};

struct TempDir(PathBuf);

impl TempDir {
    fn create(prefix: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("{prefix}-{pid}-{nanos}"));
        std::fs::create_dir_all(&dir)?;
        Ok(Self(dir))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn has_run_dirs(root: &Path) -> std::io::Result<bool> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with("run-") {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(target_os = "linux")]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(target_os = "linux")]
fn resolve_mayfly_helper() -> Option<PathBuf> {
    for var in ["MAYFLY_BIN", "CARGO_BIN_EXE_mayfly"] {
        if let Some(val) = std::env::var_os(var) {
            let path = PathBuf::from(val);
            if is_executable_file(&path) {
                return Some(path);
            }
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(target_dir) = exe.parent().and_then(Path::parent) {
            let sibling = target_dir.join("mayfly");
            if is_executable_file(&sibling) {
                return Some(sibling);
            }
        }
    }
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join("mayfly");
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn is_host_namespace_refusal(report: &mayfly::RunReport) -> bool {
    let text = format!(
        "{}\n{}",
        report.error.as_deref().unwrap_or(""),
        report.stderr_tail
    )
    .to_lowercase();
    text.contains("uid_map")
        || (text.contains("unshare")
            && (text.contains("not permitted") || text.contains("permission denied")))
}

pub fn run_quickstart() -> Result<(), Box<dyn std::error::Error>> {
    let names = backend_names();
    assert_eq!(names, vec!["namespace"]);
    let backend = get_backend("namespace")?;
    println!("1. resolved backend={}", backend.name());

    let temp = TempDir::create("mayfly-quickstart")?;
    let run_root = temp.0.join("runs");
    std::fs::create_dir_all(&run_root)?;

    // 2. Pre-prepare resource bound validation refuses timeout below floor (1s)
    //    without creating any sandbox directory on any platform.
    let mut bad_bound = RunSpec::new(
        vec![OsString::from("/bin/echo"), OsString::from("unreachable")],
        run_root.clone(),
    );
    assert_eq!(TIMEOUT_FLOOR_SECONDS, 1);
    bad_bound.timeout_seconds = Some(0);
    let bound_report = backend.run(&bad_bound);
    assert_eq!(bound_report.outcome, Outcome::HarnessError);
    assert!(bound_report.run_id.is_empty());
    assert!(!has_run_dirs(&run_root)?);
    println!("2. pre-prepare timeout=0 refused cleanly before directory creation");

    // 3. Pre-prepare read-only bind validation refuses a missing host source
    //    without creating any sandbox directory on any platform.
    let mut bad_bind = RunSpec::new(
        vec![
            OsString::from("/bin/cat"),
            OsString::from("/inputs/missing.txt"),
        ],
        run_root.clone(),
    );
    bad_bind.ro_binds = vec![(
        temp.0.join("missing.txt"),
        PathBuf::from("/inputs/missing.txt"),
    )];
    let bind_report = backend.run(&bad_bind);
    assert_eq!(bind_report.outcome, Outcome::HarnessError);
    assert!(bind_report.run_id.is_empty());
    assert!(!has_run_dirs(&run_root)?);
    println!("3. pre-prepare missing --ro-bind source refused cleanly");

    // 4. On Linux, run one disposable sandbox task with a read-only input bind.
    #[cfg(target_os = "linux")]
    {
        let helper = resolve_mayfly_helper().ok_or(
            "executable mayfly helper binary not found; run `cargo build --bin mayfly` first",
        )?;
        mayfly::set_helper_exe(&helper);
        let input_file = temp.0.join("greeting.txt");
        std::fs::write(&input_file, "hello from mayfly quickstart\n")?;

        let mut spec = RunSpec::new(
            vec![
                OsString::from("/bin/cat"),
                OsString::from("/inputs/greeting.txt"),
            ],
            run_root.clone(),
        );
        spec.timeout_seconds = Some(10);
        spec.ro_binds = vec![(input_file, PathBuf::from("/inputs/greeting.txt"))];

        let report = backend.run(&spec);
        if report.outcome == Outcome::Task {
            assert_eq!(report.task_exit_code, Some(0));
            assert_eq!(report.sandbox_removed, Some(true));
            assert_eq!(report.stdout_tail, "hello from mayfly quickstart\n");
            println!("4. Linux namespace sandbox ran and incinerated cleanly");
        } else if is_host_namespace_refusal(&report) {
            println!(
                "4. host restricted unprivileged user namespaces ({})",
                report.error.as_deref().unwrap_or("harness_error")
            );
        } else {
            return Err(format!(
                "unexpected sandbox failure: error={:?} stderr={:?}",
                report.error, report.stderr_tail
            )
            .into());
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        println!("4. live namespace spawn requires Linux (spec and pre-prepare checks passed)");
    }

    Ok(())
}

#[allow(dead_code)]
fn main() -> ExitCode {
    match run_quickstart() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::from(1)
        }
    }
}
