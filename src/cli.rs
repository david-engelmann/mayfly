//! The mayfly command line.
//!
//! mayfly run takes a root directory and a command, runs the
//! command in a fresh sandbox under that root, prints the run
//! report as json, and exits with the task exit code. A harness
//! error exits at 125 and a task killed by a signal exits at
//! 128 plus the signal number, so the three outcomes are never
//! confused with each other.

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;

use crate::{
    dump_pretty, get_backend, JsonValue, Outcome, RunSpec, HARNESS_EXIT_CODE,
    MAX_CPU_CEILING_SECONDS, MAX_CPU_FLOOR_SECONDS, MAX_MEMORY_FLOOR_BYTES, TIMEOUT_FLOOR_SECONDS,
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
/// with no ceiling passes none.
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
    let parsed: u64 = match value.to_string_lossy().parse() {
        Ok(parsed) => parsed,
        Err(_) => {
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
                match option_value(rest, index) {
                    Some(value) => root = Some(PathBuf::from(value)),
                    None => {
                        let _ = writeln!(err, "mayfly run needs a value after --root");
                        return 2;
                    }
                }
                index += 2;
            }
            "--backend" => {
                match option_value(rest, index) {
                    Some(value) => backend = value.to_string_lossy().into_owned(),
                    None => {
                        let _ = writeln!(err, "mayfly run needs a value after --backend");
                        return 2;
                    }
                }
                index += 2;
            }
            "--allow-env" => {
                match option_value(rest, index) {
                    Some(value) => allow_env.push(value),
                    None => {
                        let _ = writeln!(err, "mayfly run needs a value after --allow-env");
                        return 2;
                    }
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
        Err(exc) => {
            let payload = JsonValue::Object(vec![
                ("backend".to_string(), JsonValue::Str(backend.clone())),
                (
                    "command".to_string(),
                    JsonValue::List(
                        command
                            .iter()
                            .map(|arg| JsonValue::Str(arg.to_string_lossy().into_owned()))
                            .collect(),
                    ),
                ),
                (
                    "outcome".to_string(),
                    JsonValue::Str("harness_error".to_string()),
                ),
                ("error".to_string(), JsonValue::Str(exc.0.clone())),
            ]);
            let _ = writeln!(out, "{}", dump_pretty(&payload));
            return HARNESS_EXIT_CODE;
        }
    };
    let _ = writeln!(out, "{}", report.to_json());
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
