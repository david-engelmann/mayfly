//! mayfly, a disposable sandbox harness for agent tasks.
//!
//! A backend is anything that takes a RunSpec, runs one task in a
//! disposable sandbox, and returns a RunReport. The runner and the
//! command line talk only to this interface, so a new backend joins
//! by registering a name and implementing run. The lifecycle phases
//! every backend reports timings for are prepare, start, task, and
//! teardown, and every report adds a fifth timing beside them, the
//! total across the whole run.

pub mod backend;
pub mod cli;
pub mod clock;
pub mod env;
pub mod errors;
pub mod init;
pub mod janitor;
pub mod json;
pub mod namespace;
pub mod provenance;
pub mod report;
pub mod signals;
pub mod tail;

pub use namespace::NamespaceBackend;

pub use backend::{backend_names, get_backend, helper_exe, run_task, set_helper_exe};
pub use clock::{ms_since, now, round3};
pub use env::{current_env, env_lookup};
pub use errors::{py_os_error, py_os_error_path, strerror, HarnessError};
pub use json::{dump_pretty, py_float, write_json_string, JsonValue};
pub use provenance::{
    canonical_root, clean_marker_name, contains_subslice, pids_referencing, pids_referencing_in,
    read_provenance, run_dir_name, sweep_orphans, sweep_orphans_in, token_shape_ok,
    write_provenance, CLEAN_MARKER_PREFIX, INTERNAL_TOKEN_ENV, ORPHAN_GRACE_SECONDS,
    PROVENANCE_NAME, PROVENANCE_READ_LIMIT, RUN_DIR_PREFIX,
};
pub use report::{Outcome, Phase, RunReport, RunSpec, Timings};
pub use signals::SignalGuard;
pub use tail::{TailBuffer, TAIL_BYTES, TAIL_CHARS};

/// The exit code a harness error answers at on the command line.
/// A task exit code is mirrored exactly and a task killed by a
/// signal answers at 128 plus the signal number. The three can
/// share a code, since a task can itself exit at 125 or at a
/// signal's code, so the report, not the code, tells them apart.
pub const HARNESS_EXIT_CODE: i32 = 125;

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
