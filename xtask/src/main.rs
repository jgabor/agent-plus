//! The harness program. HARNESS.md defines its commands and exit statuses.

mod base;
mod bench;
mod floors;
mod forge;
mod gate;
mod journey;
mod json;
mod ledger;
mod lock;
mod score;
mod spec;
mod testrun;
mod toml;
mod work;
mod workspace;

use std::process::ExitCode;

const USAGE: &str = "\
usage: cargo xtask <command>

commands:
  gate             run the checks a change must pass
  score [--json]   print the scores
  run <command>    run a command under a shared hold of the activity lock
  journey          run the journey against a release build
  bench            compare the budgets with the base build
  next             print the issues that can be claimed, as JSON
  claim <issue> [--thread <link>]
                   claim an issue for the current branch
  record --issue <n> --pr <n>
                   write the ledger record for the current head

exit status: 0 pass, 1 failure, 2 bad usage or environment error
";

/// The result of a command that was able to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail,
}

/// Why a command couldn't run, which is exit status 2.
#[derive(Debug)]
pub struct Problem(pub String);

fn run(args: &[&str]) -> Result<Verdict, Problem> {
    match args {
        ["gate"] => gate::run(),
        ["score"] => score::run(false),
        ["score", "--json"] => score::run(true),
        ["run", command @ ..] => lock::run(command),
        ["journey"] => journey::run(),
        ["bench"] => bench::run(),
        ["next"] => work::run_next(),
        ["claim", issue] => work::run_claim(issue, None),
        ["claim", issue, "--thread", thread] => work::run_claim(issue, Some(thread)),
        ["claim", ..] => Err(Problem(format!(
            "`claim` takes `<issue> [--thread <link>]`\n\n{USAGE}"
        ))),
        ["record", "--issue", issue, "--pr", pr] => ledger::record(issue, pr),
        ["record", ..] => Err(Problem(format!(
            "`record` takes `--issue <n> --pr <n>`\n\n{USAGE}"
        ))),
        ["-h" | "--help" | "help"] => {
            print!("{USAGE}");
            Ok(Verdict::Pass)
        }
        [] => Err(Problem(format!("no command given\n\n{USAGE}"))),
        [
            command @ ("gate" | "score" | "journey" | "bench" | "next"),
            extra,
            ..,
        ] => Err(Problem(format!(
            "`{command}` takes no arguments, but got `{extra}`\n\n{USAGE}"
        ))),
        [other, ..] => Err(Problem(format!("unknown command `{other}`\n\n{USAGE}"))),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match run(&args) {
        Ok(Verdict::Pass) => ExitCode::SUCCESS,
        Ok(Verdict::Fail) => ExitCode::from(1),
        Err(Problem(message)) => {
            eprintln!("error: {message}");
            ExitCode::from(2)
        }
    }
}
