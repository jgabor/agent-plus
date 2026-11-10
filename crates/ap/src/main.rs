//! The `ap` command.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::process::ExitCode;
use std::time::Duration;

use ap::boot;

mod commands {
    pub mod build;
    pub mod fmt;
}

const USAGE: &str = "usage: ap --version
       ap fmt [--check] [dir]
       ap build [--strict] [-t name] [dir]
       ap boot [--timeout N] [-- COMMAND...]";
const DEFAULT_TIMEOUT_SECONDS: u64 = 30;

fn main() -> ExitCode {
    let mut args = std::env::args_os();
    let program = args.next();
    if boot::is_init(std::process::id(), program.as_deref()) {
        // Reached only if the machine couldn't power off.
        let _ = boot::init();
        return ExitCode::FAILURE;
    }

    let args: Vec<_> = args.collect();
    if args.len() == 1 && args[0] == "--version" {
        println!("ap {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    if args.first().is_some_and(|command| command == "fmt")
        && let Some(fmt) = commands::fmt::parse(&args[1..])
    {
        return ExitCode::from(commands::fmt::run(&fmt));
    }
    if args.first().is_some_and(|command| command == "build")
        && let Some(build) = commands::build::parse(&args[1..])
    {
        return ExitCode::from(commands::build::run(&build));
    }
    if args.first().is_some_and(|command| command == "boot")
        && let Some((timeout, command)) = parse_boot(&args[1..])
    {
        return match boot::boot_bare(timeout, &command) {
            Ok(status) => ExitCode::from(status),
            Err(error) => {
                eprintln!("ap: {error}");
                ExitCode::from(error.exit_code())
            }
        };
    }
    eprintln!("{USAGE}");
    ExitCode::from(2)
}

/// Splits `ap boot`'s arguments at the first `--`. What follows it is the
/// guest's, and there must be something.
fn parse_boot(args: &[OsString]) -> Option<(Duration, Vec<Vec<u8>>)> {
    let Some(separator) = args.iter().position(|arg| arg == "--") else {
        return Some((parse_timeout(args)?, Vec::new()));
    };
    let command = &args[separator + 1..];
    if command.is_empty() {
        return None;
    }
    let command = command.iter().cloned().map(OsString::into_vec).collect();
    Some((parse_timeout(&args[..separator])?, command))
}

fn parse_timeout(args: &[OsString]) -> Option<Duration> {
    match args {
        [] => Some(Duration::from_secs(DEFAULT_TIMEOUT_SECONDS)),
        [flag, seconds] if flag == "--timeout" => {
            seconds.to_str()?.parse().ok().map(Duration::from_secs)
        }
        _ => None,
    }
}
