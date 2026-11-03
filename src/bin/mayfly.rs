//! The mayfly binary. The public face is the run command line.
//! The init and the janitor and the assembly drive are the same
//! binary re-invoked under hidden subcommands, so the sandbox
//! processes carry no interpreter or package path with them.

use std::ffi::OsString;
use std::io::Write;

fn main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let code = match args.first().map(|arg| arg.to_string_lossy().into_owned()) {
        Some(ref name) if name == "__janitor" => mayfly::janitor::janitor_main(&args[1..]),
        Some(ref name) if name == "__init" => mayfly::init::init_main(&args[1..]),
        Some(ref name) if name == "__assemble-root" => mayfly::init::assemble_root_main(&args[1..]),
        _ => {
            let mut out = std::io::stdout().lock();
            let mut err = std::io::stderr().lock();
            let code = mayfly::cli::cli_main(&args, &mut out, &mut err);
            let _ = out.flush();
            let _ = err.flush();
            code
        }
    };
    std::process::exit(code);
}
