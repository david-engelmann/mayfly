//! The backend registry and the runner seat.
//!
//! Backends register by name here and nowhere else, the
//! one entry that runs a task on a named backend stands
//! here, and the helper executable seat the backend
//! re-invokes stands here with them.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::errors::HarnessError;
use crate::namespace::NamespaceBackend;
use crate::report::{RunReport, RunSpec};

/// The backend registry. Backends register by name here and
/// nowhere else, so the runner and the command line resolve a
/// name to a backend in exactly one place.
pub fn get_backend(name: &str) -> Result<NamespaceBackend, HarnessError> {
    match name {
        "namespace" => Ok(NamespaceBackend::new()),
        _ => Err(HarnessError(format!("unknown backend, {}", name))),
    }
}

/// The names get_backend resolves, in registry order. The two
/// move together, so a backend that joins the registry joins this
/// list in the same change, and a name that leaves the one
/// leaves the other.
pub fn backend_names() -> Vec<&'static str> {
    vec!["namespace"]
}

/// Run one task on the backend a name resolves to, with a
/// default spec, no bound of MF-13 set and no caller
/// environment keys carried over, the limitation MF-A15
/// records. The command line does not use this entry, because
/// it sets the bounds and the allow list its own caller named
/// on its spec, and the test suite drives the backend
/// directly.
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

/// The mayfly binary the backend re-invokes for its init and
/// janitor. It is the path set_helper_exe set where a harness set
/// one, and this process's own executable path where none did,
/// falling back to the name mayfly where that path cannot be
/// read.
pub fn helper_exe() -> PathBuf {
    if let Some(path) = HELPER_EXE.get() {
        return path.clone();
    }
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("mayfly"))
}

/// Point helper_exe at the built mayfly binary, for a test
/// harness whose own executable is not that binary. The
/// setting takes once, and a later call is silently refused, the
/// once contract the suite relies on.
pub fn set_helper_exe(path: &Path) {
    let _ = HELPER_EXE.set(path.to_path_buf());
}
