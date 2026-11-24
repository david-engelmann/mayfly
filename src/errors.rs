//! Python shaped error forms.
//!
//! The formatting that makes error text the report carries
//! read the same in this implementation as in the Python
//! harness, and the harness error shape itself.

use std::fmt;
use std::io;
use std::path::Path;

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

/// The message the C library carries for an errno, or an
/// unknown error line naming the number where it carries none.
/// This is the text the Python shaped error strings are built
/// from.
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
