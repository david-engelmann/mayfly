//! The mayfly command line.
//!
//! mayfly run takes a root directory and a command, runs the
//! command in a fresh sandbox under that root, prints the run
//! report as json, and exits with the task exit code. A harness
//! error exits at 125 and a task killed by a signal exits at
//! 128 plus the signal number. The exit code alone does not
//! always tell the three outcomes apart, because a task can
//! itself exit at 125 or at a signal's code. The report is
//! what separates them there. Its outcome field tells a
//! harness error from a task outcome, and its task_signal
//! and task_exit_code fields tell a signal death from an
//! exit at the same code.

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;

use crate::{
    get_backend, Outcome, RunReport, RunSpec, HARNESS_EXIT_CODE, MAX_CPU_CEILING_SECONDS,
    MAX_CPU_FLOOR_SECONDS, MAX_MEMORY_FLOOR_BYTES, TIMEOUT_FLOOR_SECONDS,
};

const USAGE: &str = "usage: mayfly run --root DIR [--backend NAME] [--allow-env KEY]... [--net] [--timeout SECONDS] [--max-cpu SECONDS] [--max-memory BYTES] -- COMMAND...";

/// Read one bound option value in the refusal classes the
/// command line uses for every value it takes. A missing
/// value, a value that does not read as a whole count, and a
/// value under the bound floor each refuse at exit 2 in their
/// own words, and no report is printed for any of them, as no
/// report is printed for any malformed call. The floor text
/// is the words the floor refusal carries for this bound. A
/// bound that carries a ceiling refuses a value over it in
/// the same class, in the words of its bound, and a bound
/// with no ceiling passes none. A value written in digits
/// alone that stands past the widest count the parser
/// carries is still a whole count, so the whole count
/// refusal would be false for it; it refuses in range words
/// instead, the ceiling words of its bound where the bound
/// carries a ceiling and a widest count refusal where it
/// carries none.
fn bound_value(
    value: Option<OsString>,
    name: &str,
    floor: u64,
    floor_text: &str,
    ceiling: Option<u64>,
    err: &mut dyn Write,
) -> Result<u64, i32> {
    let value = match value {
        Some(value) => value,
        None => {
            let _ = writeln!(err, "mayfly run needs a value after {}", name);
            return Err(2);
        }
    };
    let text = value.to_string_lossy();
    let parsed: u64 = match text.parse() {
        Ok(parsed) => parsed,
        Err(_) => {
            if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) {
                // The value reads as a whole count and stands
                // past the widest count the parser carries, so
                // the whole count words would be false for it.
                // It refuses in range words instead.
                if let Some(ceiling) = ceiling {
                    let _ = writeln!(
                        err,
                        "mayfly run needs {} of at most {} seconds",
                        name, ceiling
                    );
                } else {
                    let _ = writeln!(
                        err,
                        "mayfly run needs a value after {} no larger than {}",
                        name,
                        u64::MAX
                    );
                }
                return Err(2);
            }
            let _ = writeln!(err, "mayfly run needs a whole number after {}", name);
            return Err(2);
        }
    };
    if parsed < floor {
        let _ = writeln!(err, "mayfly run needs {} of at least {}", name, floor_text);
        return Err(2);
    }
    if let Some(ceiling) = ceiling {
        if parsed > ceiling {
            let _ = writeln!(
                err,
                "mayfly run needs {} of at most {} seconds",
                name, ceiling
            );
            return Err(2);
        }
    }
    Ok(parsed)
}

/// Read one valued string option in the refusal class the
/// command line uses for a missing value. The bound options
/// read through bound_value in this same shape, a helper
/// returning the value or the exit code after writing its
/// refusal; the string options read through here, and the
/// refusal is the one sentence every valued option answers
/// with where its value is missing or option shaped.
fn string_value(value: Option<OsString>, name: &str, err: &mut dyn Write) -> Result<OsString, i32> {
    match value {
        Some(value) => Ok(value),
        None => {
            let _ = writeln!(err, "mayfly run needs a value after {}", name);
            Err(2)
        }
    }
}

/// The command line entry point. The argument list excludes the
/// program name. Output goes to the two writers, and the return
/// value is the process exit code.
pub fn cli_main(args: &[OsString], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    if args.is_empty() || args[0].as_os_str() != "run" {
        let _ = writeln!(err, "{}", USAGE);
        return 2;
    }
    let rest = &args[1..];
    let mut root: Option<PathBuf> = None;
    let mut backend = "namespace".to_string();
    let mut allow_env: Vec<OsString> = Vec::new();
    let mut network = false;
    let mut timeout_seconds: Option<u64> = None;
    let mut max_cpu_seconds: Option<u64> = None;
    let mut max_memory_bytes: Option<u64> = None;
    let mut command: Vec<OsString> = Vec::new();
    let mut index = 0;
    // The value an option takes. A token that starts with a
    // hyphen is an option shape, never a value, so an option
    // followed by one is missing its value, the same refusal
    // the argparse command line of Round 1 made.
    let option_value = |rest: &[OsString], index: usize| -> Option<OsString> {
        rest.get(index + 1).and_then(|value| {
            if value.to_string_lossy().starts_with('-') {
                None
            } else {
                Some(value.clone())
            }
        })
    };
    while index < rest.len() {
        let arg = &rest[index];
        let text = arg.to_string_lossy().into_owned();
        if text == "--" {
            command = rest[index + 1..].to_vec();
            break;
        }
        match text.as_str() {
            "--root" => {
                match string_value(option_value(rest, index), "--root", err) {
                    Ok(value) => root = Some(PathBuf::from(value)),
                    Err(code) => return code,
                }
                index += 2;
            }
            "--backend" => {
                match string_value(option_value(rest, index), "--backend", err) {
                    Ok(value) => backend = value.to_string_lossy().into_owned(),
                    Err(code) => return code,
                }
                index += 2;
            }
            "--allow-env" => {
                match string_value(option_value(rest, index), "--allow-env", err) {
                    Ok(value) => allow_env.push(value),
                    Err(code) => return code,
                }
                index += 2;
            }
            "--net" => {
                network = true;
                index += 1;
            }
            "--timeout" => {
                match bound_value(
                    option_value(rest, index),
                    "--timeout",
                    TIMEOUT_FLOOR_SECONDS,
                    "1 second",
                    None,
                    err,
                ) {
                    Ok(value) => timeout_seconds = Some(value),
                    Err(code) => return code,
                }
                index += 2;
            }
            "--max-cpu" => {
                match bound_value(
                    option_value(rest, index),
                    "--max-cpu",
                    MAX_CPU_FLOOR_SECONDS,
                    "1 second",
                    Some(MAX_CPU_CEILING_SECONDS),
                    err,
                ) {
                    Ok(value) => max_cpu_seconds = Some(value),
                    Err(code) => return code,
                }
                index += 2;
            }
            "--max-memory" => {
                match bound_value(
                    option_value(rest, index),
                    "--max-memory",
                    MAX_MEMORY_FLOOR_BYTES,
                    "16777216 bytes",
                    None,
                    err,
                ) {
                    Ok(value) => max_memory_bytes = Some(value),
                    Err(code) => return code,
                }
                index += 2;
            }
            _ => {
                if text.starts_with('-') && text.len() > 1 {
                    let _ = writeln!(err, "mayfly run does not know the option {}", text);
                    let _ = writeln!(err, "{}", USAGE);
                    return 2;
                }
                command = rest[index..].to_vec();
                break;
            }
        }
    }
    if !command.is_empty() && command[0].as_os_str() == "--" {
        command.remove(0);
    }
    if command.is_empty() {
        let _ = writeln!(err, "mayfly run needs a command after --");
        return 2;
    }
    let root = match root {
        Some(root) => root,
        None => {
            let _ = writeln!(err, "mayfly run needs a --root directory");
            let _ = writeln!(err, "{}", USAGE);
            return 2;
        }
    };
    if allow_env.is_empty() {
        allow_env.push(OsString::from("PATH"));
    }
    let mut spec = RunSpec::new(command.clone(), root);
    spec.allow_env = allow_env;
    spec.network = network;
    spec.timeout_seconds = timeout_seconds;
    spec.max_cpu_seconds = max_cpu_seconds;
    spec.max_memory_bytes = max_memory_bytes;
    let report = match get_backend(&backend) {
        Ok(backend_impl) => backend_impl.run(&spec),
        Err(exc) => RunReport::early_refusal(&backend, command.clone(), exc.0),
    };
    // The report is the run's one product and the exit code
    // below mirrors it, so a report that could not be written
    // was never delivered and the mirror has nothing to stand
    // on. The run answers as a harness error, the code that
    // already names every other harness failure.
    if writeln!(out, "{}", report.to_json()).is_err() {
        return HARNESS_EXIT_CODE;
    }
    if report.outcome == Outcome::HarnessError {
        return HARNESS_EXIT_CODE;
    }
    if let Some(signal) = report.task_signal {
        return 128 + signal;
    }
    if let Some(code) = report.task_exit_code {
        return code;
    }
    HARNESS_EXIT_CODE
}
