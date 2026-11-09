//! The `ap` command.

use std::ffi::OsString;
use std::process::ExitCode;
use std::time::Duration;

use ap::boot;

const USAGE: &str = "usage: ap --version\n       ap boot [--timeout N]";
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
    if args.first().is_some_and(|command| command == "boot")
        && let Some(timeout) = parse_timeout(&args[1..])
    {
        return match boot::boot_bare(timeout) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("ap: {error}");
                ExitCode::from(error.exit_code())
            }
        };
    }
    eprintln!("{USAGE}");
    ExitCode::from(2)
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
