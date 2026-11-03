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

use crate::{dump_pretty, get_backend, JsonValue, RunSpec, HARNESS_EXIT_CODE};

const USAGE: &str =
    "usage: mayfly run --root DIR [--backend NAME] [--allow-env KEY]... [--net] -- COMMAND...";

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
    let mut command: Vec<OsString> = Vec::new();
    let mut index = 0;
    while index < rest.len() {
        let arg = &rest[index];
        let text = arg.to_string_lossy().into_owned();
        if text == "--" {
            command = rest[index + 1..].to_vec();
            break;
        }
        match text.as_str() {
            "--root" => {
                match rest.get(index + 1) {
                    Some(value) => root = Some(PathBuf::from(value)),
                    None => {
                        let _ = writeln!(err, "mayfly run needs a value after --root");
                        return 2;
                    }
                }
                index += 2;
            }
            "--backend" => {
                match rest.get(index + 1) {
                    Some(value) => backend = value.to_string_lossy().into_owned(),
                    None => {
                        let _ = writeln!(err, "mayfly run needs a value after --backend");
                        return 2;
                    }
                }
                index += 2;
            }
            "--allow-env" => {
                match rest.get(index + 1) {
                    Some(value) => allow_env.push(value.clone()),
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
    if report.outcome == "harness_error" {
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
