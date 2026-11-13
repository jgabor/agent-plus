//! The `ap` command.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStringExt;
use std::process::ExitCode;
use std::time::Duration;

use ap::boot;

mod commands {
    pub mod build;
    pub mod fmt;
    pub mod init;
    pub mod inspect;
    pub mod ls;
    pub mod ps;
    pub mod rm;
    pub mod run;
}

const USAGE: &str = "usage: ap --version
       ap init [dir]
       ap fmt [--check] [dir]
       ap build [--strict] [--replace] [--approve=file] [-t name] [dir]
       ap ls
       ap inspect [--grants] agent
       ap rm agent
       ap run [-i] [-p prompt] agent
       ap ps
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
    if program.as_deref() == Some(OsStr::new(guest::agent::AS_AGENT)) {
        // The guest's init started this to become `agent` and exec the harness.
        let error = guest::agent::exec_as_agent(&args);
        eprintln!("ap: can't start the harness: {error}");
        // The harness never started, so this is `ap`'s failure, not its.
        return ExitCode::from(125);
    }
    if program.as_deref() == Some(OsStr::new(guest::agent::EXEC_AS_AGENT)) {
        // The guest's init started this to become `agent` and exec a command
        // beside the harness, which fails as a shell's would.
        let error = guest::agent::exec_as_agent(&args);
        let name = args.first().map(|name| name.to_string_lossy());
        eprintln!("ap exec: {}: {error}", name.unwrap_or_default());
        return ExitCode::from(guest::agent::status_of_failure(&error));
    }
    if program.as_deref() == Some(OsStr::new(ap::runs::RUN_PROCESS)) {
        return ExitCode::from(ap::runs::run_process(&args));
    }
    if program.as_deref() == Some(OsStr::new(ap::runs::CLEANUP)) {
        return ExitCode::from(ap::runs::cleanup(&args));
    }
    if args.len() == 1 && args[0] == "--version" {
        println!("ap {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    if args.first().is_some_and(|command| command == "init")
        && let Some(dir) = commands::init::parse(&args[1..])
    {
        return ExitCode::from(commands::init::run(dir.as_ref()));
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
    if args.first().is_some_and(|command| command == "ls")
        && commands::ls::parse(&args[1..]).is_some()
    {
        return ExitCode::from(commands::ls::run());
    }
    if args.first().is_some_and(|command| command == "inspect")
        && let Some(inspect) = commands::inspect::parse(&args[1..])
    {
        return ExitCode::from(commands::inspect::run(&inspect));
    }
    if args.first().is_some_and(|command| command == "rm")
        && let Some(name) = commands::rm::parse(&args[1..])
    {
        return ExitCode::from(commands::rm::run(&name));
    }
    if args.first().is_some_and(|command| command == "run")
        && let Some(run) = commands::run::parse(&args[1..])
    {
        return ExitCode::from(commands::run::run(&run));
    }
    if args.first().is_some_and(|command| command == "ps")
        && let Some(all) = commands::ps::parse(&args[1..])
    {
        return ExitCode::from(commands::ps::run(all));
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
