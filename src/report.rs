//! The run report model.
//!
//! The spec a run is handed and the record a run answers
//! with, with the phase and outcome types the report is
//! keyed by.

use std::ffi::OsString;
use std::path::PathBuf;

use crate::json::{dump_pretty, opt_bool, opt_float, opt_int, opt_str, JsonValue};

/// One task to run in one fresh sandbox.
#[derive(Debug, Clone)]
pub struct RunSpec {
    /// The command the task runs, its program first, in the form
    /// the exec receives it.
    pub command: Vec<OsString>,
    /// The run root the run's sandbox directory is created
    /// under, and the directory the orphan sweep reads before
    /// the run prepares its own. The root is read under its
    /// canonical form, the form canonical_root computes: the
    /// sandbox directory is created under that spelling, and
    /// the sweep reads under it, whichever spelling the
    /// caller handed in.
    pub root: PathBuf,
    /// The caller environment keys the scrub carries into the
    /// sandbox. RunSpec::new defaults the list to PATH alone.
    /// Assigning a new list replaces that default whole, while
    /// pushing a key onto the list keeps PATH beside it. The
    /// command line replaces the default whole whenever any
    /// --allow-env key is named, so a caller there who wants a
    /// key beside PATH names both.
    pub allow_env: Vec<OsString>,
    /// Whether the task shares the host network. False, the
    /// default, gives the task a fresh network namespace
    /// holding only the loopback; true shares the host network.
    pub network: bool,
    /// Descriptors the caller hands the task across the sandbox
    /// boundary, kept at their own numbers in the task. The two
    /// exceptions are the task's own output streams, since the
    /// runner gives the task its capture pipes at numbers 1 and
    /// 2, so a descriptor handed over at one of those numbers is
    /// replaced by the pipe and never reaches the task. This is
    /// the one caller granted exception to the isolation the
    /// contract states, named in MF-1. A descriptor handed over
    /// this way is the caller's own grant to the task and stands
    /// outside the scrub and the prepared root by that choice.
    pub pass_fds: Vec<std::os::unix::io::RawFd>,
    /// A snapshot of the caller environment the scrub reads, taken
    /// by the caller. Where this is empty the live process
    /// environment is read at run time instead, which is what the
    /// command line and the ordinary library calls do.
    pub caller_env: Vec<(OsString, OsString)>,
    /// The wall clock timeout of the task phase in whole
    /// seconds, where the caller sets one, under MF-13. None
    /// runs under no timeout.
    pub timeout_seconds: Option<u64>,
    /// The processor time ceiling of the task in whole
    /// seconds, where the caller sets one, under MF-13. None
    /// runs under no processor ceiling.
    pub max_cpu_seconds: Option<u64>,
    /// The address space ceiling of the task in bytes, where
    /// the caller sets one, under MF-13. None runs under no
    /// address space ceiling.
    pub max_memory_bytes: Option<u64>,
}

impl RunSpec {
    /// A spec carrying the defaults, allow_env at PATH alone,
    /// the network off, no handed descriptors, no caller
    /// environment snapshot, and no bound of MF-13 set.
    pub fn new(command: Vec<OsString>, root: PathBuf) -> RunSpec {
        RunSpec {
            command,
            root,
            allow_env: vec![OsString::from("PATH")],
            network: false,
            pass_fds: Vec::new(),
            caller_env: Vec::new(),
            timeout_seconds: None,
            max_cpu_seconds: None,
            max_memory_bytes: None,
        }
    }
}

/// The lifecycle phase timings of one run, in the order the
/// report prints them. A phase a run ends before completing is
/// None, never a measured-looking zero.
#[derive(Debug, Clone, Default)]
pub struct Timings {
    pub prepare: Option<f64>,
    pub start: Option<f64>,
    pub task: Option<f64>,
    pub teardown: Option<f64>,
    pub total: Option<f64>,
}

/// One lifecycle phase a run reports a timing for. The name is
/// the key the report prints for the phase, and the phases are
/// keyed by this type rather than by their name strings, so a
/// mistyped phase is a compile error and never a missing key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Prepare,
    Start,
    Task,
    Teardown,
    Total,
}

impl Phase {
    pub const ALL: [Phase; 5] = [
        Phase::Prepare,
        Phase::Start,
        Phase::Task,
        Phase::Teardown,
        Phase::Total,
    ];

    pub fn name(&self) -> &'static str {
        match self {
            Phase::Prepare => "prepare",
            Phase::Start => "start",
            Phase::Task => "task",
            Phase::Teardown => "teardown",
            Phase::Total => "total",
        }
    }
}

impl Timings {
    pub fn get(&self, phase: Phase) -> Option<f64> {
        match phase {
            Phase::Prepare => self.prepare,
            Phase::Start => self.start,
            Phase::Task => self.task,
            Phase::Teardown => self.teardown,
            Phase::Total => self.total,
        }
    }

    pub fn set(&mut self, phase: Phase, value: Option<f64>) {
        match phase {
            Phase::Prepare => self.prepare = value,
            Phase::Start => self.start = value,
            Phase::Task => self.task = value,
            Phase::Teardown => self.teardown = value,
            Phase::Total => self.total = value,
        }
    }
}

/// The outcome a run ends in, one of the two values the report
/// prints for its outcome field. A report carries this type
/// rather than the printed string, so an outcome the contract
/// does not name cannot be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Task,
    HarnessError,
}

impl Outcome {
    /// The text the report prints for this outcome.
    pub fn as_str(&self) -> &'static str {
        match self {
            Outcome::Task => "task",
            Outcome::HarnessError => "harness_error",
        }
    }
}

/// The record of one run, printed by the command line as json.
#[derive(Debug, Clone)]
pub struct RunReport {
    /// The backend that ran the task, by its registered name.
    /// On the command line's unknown backend refusal, the name
    /// the caller asked for, which no backend registered.
    pub backend: String,
    /// The run's id, drawn for the run. Empty on an early
    /// refusal.
    pub run_id: String,
    /// The sandbox directory the run prepared under the root,
    /// named under the root's canonical spelling, so the path
    /// stands resolved wherever the caller's spelling of the
    /// root was not canonical. The path is converted to text
    /// lossily, so a root whose canonical form holds bytes
    /// that are not valid UTF-8 prints a string that may not
    /// name the directory. Empty on an early refusal.
    pub sandbox_dir: String,
    /// The command the run carried, as its caller named it.
    pub command: Vec<OsString>,
    /// The run's outcome, a task outcome or a harness error.
    pub outcome: Outcome,
    /// The task's exit code, where the task ended by exiting.
    /// On a task outcome exactly one of this field and
    /// task_signal stands.
    pub task_exit_code: Option<i32>,
    /// The signal that killed the task, where the task ended
    /// by a signal. On a task outcome exactly one of this
    /// field and task_exit_code stands.
    pub task_signal: Option<i32>,
    /// The harness error's text. None on a task outcome, and
    /// None too on a harness error the run could not name, as
    /// where the wait on the sandbox child itself failed.
    pub error: Option<String>,
    /// Whether a fresh proc was mounted in the prepared root.
    /// Some on every report a backend returns, and false on
    /// an early refusal.
    pub proc_mounted: Option<bool>,
    /// Whether the sandbox directory stands removed at the
    /// report's end. Some on every report a backend returns,
    /// and true on an early refusal, where none was created.
    pub sandbox_removed: Option<bool>,
    /// The run's phase timings. The total stands Some on
    /// every report a backend returns, refusals included, and
    /// None on the command line's unknown backend refusal,
    /// which no backend answered.
    pub timings_ms: Timings,
    /// The tail of the task's standard output, kept to the
    /// harness's tail bound.
    pub stdout_tail: String,
    /// The tail of the task's standard error, kept to the
    /// harness's tail bound.
    pub stderr_tail: String,
}

impl RunReport {
    pub fn new(
        backend: &str,
        run_id: &str,
        sandbox_dir: &str,
        command: Vec<OsString>,
        outcome: Outcome,
    ) -> RunReport {
        RunReport {
            backend: backend.to_string(),
            run_id: run_id.to_string(),
            sandbox_dir: sandbox_dir.to_string(),
            command,
            outcome,
            task_exit_code: None,
            task_signal: None,
            error: None,
            proc_mounted: None,
            sandbox_removed: None,
            timings_ms: Timings::default(),
            stdout_tail: String::new(),
            stderr_tail: String::new(),
        }
    }

    /// The report of a run refused before anything was
    /// prepared. No run id and no sandbox directory stand on
    /// it, the error is set, no proc is mounted, and the
    /// sandbox stands recorded as removed because none was
    /// created. The backend's bound refusal and random draw
    /// refusal and the command line's unknown backend refusal
    /// all answer with this shape, and a caller whose refusal
    /// still measures the run's total time sets that timing
    /// on the report it gets back.
    pub fn early_refusal(backend: &str, command: Vec<OsString>, error: String) -> RunReport {
        let mut report = RunReport::new(backend, "", "", command, Outcome::HarnessError);
        report.error = Some(error);
        report.proc_mounted = Some(false);
        report.sandbox_removed = Some(true);
        report
    }

    pub fn to_json(&self) -> String {
        let mut timings: Vec<(String, JsonValue)> = Vec::new();
        for phase in Phase::ALL {
            timings.push((phase.name().into(), opt_float(self.timings_ms.get(phase))));
        }
        let fields: Vec<(String, JsonValue)> = vec![
            ("backend".into(), JsonValue::Str(self.backend.clone())),
            ("run_id".into(), JsonValue::Str(self.run_id.clone())),
            (
                "sandbox_dir".into(),
                JsonValue::Str(self.sandbox_dir.clone()),
            ),
            (
                "command".into(),
                JsonValue::List(
                    self.command
                        .iter()
                        .map(|arg| JsonValue::Str(arg.to_string_lossy().into_owned()))
                        .collect(),
                ),
            ),
            (
                "outcome".into(),
                JsonValue::Str(self.outcome.as_str().to_string()),
            ),
            ("task_exit_code".into(), opt_int(self.task_exit_code)),
            ("task_signal".into(), opt_int(self.task_signal)),
            ("error".into(), opt_str(self.error.as_deref())),
            ("proc_mounted".into(), opt_bool(self.proc_mounted)),
            ("sandbox_removed".into(), opt_bool(self.sandbox_removed)),
            ("timings_ms".into(), JsonValue::Object(timings)),
            (
                "stdout_tail".into(),
                JsonValue::Str(self.stdout_tail.clone()),
            ),
            (
                "stderr_tail".into(),
                JsonValue::Str(self.stderr_tail.clone()),
            ),
        ];
        dump_pretty(&JsonValue::Object(fields))
    }
}
