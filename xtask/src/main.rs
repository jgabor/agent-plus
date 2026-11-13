//! The harness program. HARNESS.md defines its commands and exit statuses.

mod base;
mod bench;
mod bisect;
mod floors;
mod forge;
mod fuzz;
mod gaps;
mod gate;
mod journey;
mod json;
mod ledger;
mod limit;
mod lock;
mod merge;
mod mirror;
mod mutation;
mod nightly;
mod readme;
mod refresh;
mod report;
mod roadmap;
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
  gaps [--json]    list the failing steps, missed budgets, and unproven promises
  run <command>    run a command under a shared hold of the activity lock
  journey          run the journey against a release build
  bench            compare the budgets with the base build
  report [--json]  print the nightly report
  nightly          run the slow checks on main, and post the report
  roadmap [--details]
                   print the roadmap, or the owner's view of it
  next             print the issues that can be claimed, as JSON
  claim <issue> [--thread <link>]
                   claim an issue for the current branch
  status <issue> --thread <link>
                   replace the status comment with stdin, and renew the lease
  record --issue <n> --pr <n>
                   write the ledger record for the current head
  merge <pr> [--thread <link>]
                   rebase, gate, record, and squash-merge a PR
  prepare <pr> [--keep-budgets] [--thread <link>]
                   rebase, gate, and record a PR, for the owner to merge
  refresh          prove each land-needed PR again that is behind main
  land <pr>        run prepare, then squash-merge the PR, for the owner
  prove <pr> <merge|prepare|land> [--keep <file>]
                   prove a PR with this xtask in its rebased tree, for a merge
  mirror <number> <label>
                   copy a parking label from an issue or PR to its other side

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
        ["gaps"] => gaps::run(false),
        ["gaps", "--json"] => gaps::run(true),
        ["gaps", ..] => Err(Problem(format!("`gaps` takes only `--json`\n\n{USAGE}"))),
        ["run", command @ ..] => lock::run(command),
        ["journey"] => journey::run(),
        ["bench"] => bench::run(),
        ["report"] => report::run(false),
        ["report", "--json"] => report::run(true),
        ["report", ..] => Err(Problem(format!("`report` takes only `--json`\n\n{USAGE}"))),
        ["nightly"] => nightly::run(),
        ["roadmap"] => roadmap::run(false),
        ["roadmap", "--details"] => roadmap::run(true),
        ["roadmap", ..] => Err(Problem(format!(
            "`roadmap` takes only `--details`\n\n{USAGE}"
        ))),
        ["next"] => work::run_next(),
        ["claim", issue] => work::run_claim(issue, None),
        ["claim", issue, "--thread", thread] => work::run_claim(issue, Some(thread)),
        ["claim", ..] => Err(Problem(format!(
            "`claim` takes `<issue> [--thread <link>]`\n\n{USAGE}"
        ))),
        ["status", issue, "--thread", thread] => work::run_status(issue, thread),
        ["status", ..] => Err(Problem(format!(
            "`status` takes `<issue> --thread <link>`\n\n{USAGE}"
        ))),
        ["record", "--issue", issue, "--pr", pr] => ledger::record(issue, pr),
        ["record", ..] => Err(Problem(format!(
            "`record` takes `--issue <n> --pr <n>`\n\n{USAGE}"
        ))),
        ["merge", pr] => merge::run(pr, None),
        ["merge", pr, "--thread", thread] => merge::run(pr, Some(thread)),
        ["merge", ..] => Err(Problem(format!(
            "`merge` takes `<pr> [--thread <link>]`\n\n{USAGE}"
        ))),
        ["prepare", pr] => merge::prepare(pr, false, None),
        ["prepare", pr, "--keep-budgets"] => merge::prepare(pr, true, None),
        ["prepare", pr, "--thread", thread] => merge::prepare(pr, false, Some(thread)),
        ["prepare", pr, "--keep-budgets", "--thread", thread]
        | ["prepare", pr, "--thread", thread, "--keep-budgets"] => {
            merge::prepare(pr, true, Some(thread))
        }
        ["prepare", ..] => Err(Problem(format!(
            "`prepare` takes `<pr> [--keep-budgets] [--thread <link>]`\n\n{USAGE}"
        ))),
        ["refresh"] => refresh::run(),
        ["prove", pr, mode] => merge::prove_here(pr, mode, None),
        ["prove", pr, mode, "--keep", kept] => merge::prove_here(pr, mode, Some(kept)),
        ["prove", ..] => Err(Problem(format!(
            "`prove` takes `<pr> <merge|prepare|land> [--keep <file>]`\n\n{USAGE}"
        ))),
        ["land", pr] => merge::land(pr),
        ["land", ..] => Err(Problem(format!("`land` takes `<pr>`\n\n{USAGE}"))),
        ["mirror", number, label] => mirror::run(number, label),
        ["mirror", ..] => Err(Problem(format!(
            "`mirror` takes `<number> <label>`\n\n{USAGE}"
        ))),
        ["-h" | "--help" | "help"] => {
            print!("{USAGE}");
            Ok(Verdict::Pass)
        }
        [] => Err(Problem(format!("no command given\n\n{USAGE}"))),
        [
            command @ ("gate" | "score" | "journey" | "bench" | "next" | "nightly" | "refresh"),
            extra,
            ..,
        ] => Err(Problem(format!(
            "`{command}` takes no arguments, but got `{extra}`\n\n{USAGE}"
        ))),
        [other, ..] => Err(Problem(format!("unknown command `{other}`\n\n{USAGE}"))),
    }
}

fn main() -> ExitCode {
    if let Some(code) = floors::stand_in_for_rustc() {
        return code;
    }
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
