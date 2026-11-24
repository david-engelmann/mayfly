//! Environment helpers.
//!
//! A live view of environment pairs as ordered bytes, and
//! the lookup over such a list.

use std::ffi::{OsStr, OsString};

/// A live view of environment pairs as ordered bytes, so tests
/// can build synthetic caller environments without mutating the
/// process environment.
pub fn current_env() -> Vec<(OsString, OsString)> {
    std::env::vars_os().collect()
}

/// The value standing under key in an environment list, the
/// first standing where a key stands twice, or None where it
/// does not stand.
pub fn env_lookup<'a>(env: &'a [(OsString, OsString)], key: &OsStr) -> Option<&'a OsStr> {
    env.iter()
        .find(|(k, _)| k.as_os_str() == key)
        .map(|(_, v)| v.as_os_str())
}
