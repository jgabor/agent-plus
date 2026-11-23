//! `cargo xtask nightly`: the slow checks on `main`, then the report.

use crate::forge::{Forge, Gh};
use crate::journey::Outcome;
use crate::report::ISSUE_TITLE;
use crate::{
    Problem, Verdict, base, bench, bisect, fuzz, gate, journey, ledger, limit, lock, mutation,
    report, roadmap, workspace,
};
use std::path::{Path, PathBuf};
use std::{env, fs, io};

const ISSUE_BODY: &str = "Each night's report is a comment on this issue.";
const STATUS: &str = "xtask/nightly";
const PASS: &str = "pass";
const FAIL: &str = "fail";
const SKIP: &str = "skip";
/// Where the owner puts each provider's key for the paid checks, under `HOME`,
/// as `<provider>-api-key`. Each belongs to a workspace or project whose spend
/// limit the provider enforces.
const PAID_KEYS: &str = ".config/agent-plus/paid-checks";
/// The limit for the whole run, inside the scheduled task's timeout of 2 hours,
/// which also covers building `xtask`. A step with a limit of its own stops
/// there, or once only the report's share of this is left.
const SECONDS: u64 = 6600;
const SECONDS_OVERRIDE: &str = "XTASK_NIGHTLY_SECONDS";
/// The report's share: building and posting it, the status, and the roadmap.
const REPORT_SECONDS: u64 = 600;

pub fn run() -> Result<Verdict, Problem> {
    let deadline =
        limit::Deadline::after(limit::seconds(SECONDS_OVERRIDE, SECONDS)?, REPORT_SECONDS);
    let root = workspace::root()?;
    let commit = tested_commit(&root)?;
    gate::require_tools(&root)?;
    mutation::require_tool(&root)?;

    // A nightly has no PR, so nothing can give a reason for a slowdown. Comparing
    // `main` with itself only measures it, which is why the bench comes first,
    // on the quietest machine.
    let mut checks = vec![("bench", state(bench::compare(&root, Some(""), &|_| Ok(()))))];
    let hold = lock::testing(&root)?;
    let (gated, score) = match gate::check(&root, &hold, None) {
        Ok((failed, score)) => (state(Ok(gate::verdict(&failed))), Some(score)),
        Err(problem) => (state(Err(problem)), None),
    };
    checks.push(("gate", gated));
    checks.push(("t2", t2(&root)));
    checks.push(("release", release(&root)));
    // T3 has no tests yet. The journey's spend steps are its first checks.
    checks.push(("t3", SKIP));
    let walked = paid_keys().and_then(|keys| journey::outcomes(&root, &keys));
    checks.push(("journey", state(journey_holds(&root, &walked))));
    checks.push((
        "fuzz",
        fuzz::run(&root, &hold, deadline)
            .transpose()
            .map_or(SKIP, state),
    ));
    let mutated = mutation::run(&root, &hold, deadline).transpose();
    checks.push(("mutation", mutated.map_or(SKIP, state)));
    drop(hold);

    // A report that can't be built still leaves its result on the commit.
    let built = report::build(&root).map(|report| match &walked {
        Ok(outcomes) => report.with_left_out(
            outcomes
                .iter()
                .filter(|o| o.spends && o.state == SKIP)
                .count(),
        ),
        Err(_) => report,
    });
    let failed = built.is_err() || checks.iter().any(|(_, state)| *state == FAIL);
    let posted = built.and_then(|report| post(&root, &report.comment(&checks)));
    let (verdict, status, word) = if failed {
        (Verdict::Fail, "failure", FAIL)
    } else {
        (Verdict::Pass, "success", PASS)
    };
    Gh::new(&root).post_status(&commit, STATUS, status, None, None)?;
    let responded = if gated == FAIL {
        bisect::respond(&root, &commit, STATUS)
    } else {
        Ok(())
    };
    // The roadmap shows the journey and the checks this night ran. One that
    // couldn't run has already failed its check.
    let written = match (&walked, &score) {
        (Ok(outcomes), Some(score)) => roadmap::write(&root, outcomes, score),
        _ => Ok(()),
    };
    posted?;
    responded?;
    written?;
    for (name, state) in &checks {
        println!("{state} {name}");
    }
    println!("nightly: {word}");
    Ok(verdict)
}

/// The keys for the paid checks that the owner has put in place. A key is
/// never printed, so no message here quotes a file.
fn paid_keys() -> Result<journey::Keys, Problem> {
    let mut keys = journey::Keys::new();
    let Some(home) = env::var_os("HOME") else {
        return Ok(keys);
    };
    for (provider, _) in journey::PROVIDERS {
        let path = PathBuf::from(&home)
            .join(PAID_KEYS)
            .join(format!("{provider}-api-key"));
        match fs::read_to_string(&path) {
            Ok(text) if !text.trim().is_empty() => {
                keys.insert(provider, text.trim().to_string());
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(Problem(format!("couldn't read {}: {e}", path.display()))),
        }
    }
    Ok(keys)
}

/// HEAD, which has to be `main` as everyone else sees it, or the report and the
/// status would describe something else.
fn tested_commit(root: &Path) -> Result<String, Problem> {
    let name = |name: &str| {
        let text = base::git_text(root, &["rev-parse", name])?;
        Ok::<String, Problem>(text.trim().to_string())
    };
    let (head, main) = (name("HEAD")?, name(base::MAIN)?);
    if head != main {
        return Err(Problem(format!(
            "`nightly` tests `main`, but HEAD is {head} and origin/main is {main}. Fetch and check out origin/main"
        )));
    }
    let changed = ledger::changes(root)?;
    if !changed.is_empty() {
        return Err(Problem(format!(
            "`nightly` tests a commit, but the tree differs from it. Commit or discard these first:\n{}",
            changed.trim_end()
        )));
    }
    Ok(head)
}

/// A step that couldn't run hasn't passed, and the report should still go out.
fn state(result: Result<Verdict, Problem>) -> &'static str {
    match result {
        Ok(Verdict::Pass) => PASS,
        Ok(Verdict::Fail) => FAIL,
        Err(Problem(message)) => {
            eprintln!("nightly: {message}");
            FAIL
        }
    }
}

fn t2(root: &Path) -> &'static str {
    let packages = match workspace::packages(root) {
        Ok(packages) => packages,
        Err(problem) => return state(Err(problem)),
    };
    if !packages.iter().any(|package| package.name == "t2") {
        return SKIP;
    }
    let passed = gate::cargo_passes(root, &["test", "--locked", "--package", "t2"]);
    state(passed.map(|passed| if passed { Verdict::Pass } else { Verdict::Fail }))
}

/// The tests that the gate leaves out because they download from a real
/// release.
fn release(root: &Path) -> &'static str {
    let args = [
        "test", "--locked", "--package", "t1", "--test", "fetch", "--", "--ignored",
    ];
    let passed = gate::cargo_passes(root, &args);
    state(passed.map(|passed| if passed { Verdict::Pass } else { Verdict::Fail }))
}

/// A step that isn't built yet fails today, so what counts is a count lower
/// than the ledger's. The ledger leaves out the steps that spend, so the
/// comparison does too.
fn journey_holds(root: &Path, walked: &Result<Vec<Outcome>, Problem>) -> Result<Verdict, Problem> {
    let outcomes = walked
        .as_ref()
        .map_err(|Problem(message)| Problem(message.clone()))?;
    let (all, steps) = journey::tally(outcomes);
    println!("journey {all} of {steps}");
    let passing = outcomes
        .iter()
        .filter(|o| !o.spends && o.state == PASS)
        .count();
    let floor = ledger::at_head(root)?.map_or(0, |record| record.passing);
    if passing < floor {
        eprintln!("journey: {passing} steps pass, but the ledger says {floor}");
        return Ok(Verdict::Fail);
    }
    Ok(Verdict::Pass)
}

/// Comments on the one pinned report issue. Pinning is separate from finding
/// it, so that a run that stopped between the two is put right by the next.
fn post(root: &Path, report: &str) -> Result<(), Problem> {
    let gh = Gh::new(root);
    let found = gh
        .open_issues()?
        .into_iter()
        .find(|issue| issue.title == ISSUE_TITLE);
    let (number, pinned) = match found {
        Some(issue) => (issue.number, issue.pinned),
        None => (gh.create_issue(ISSUE_TITLE, ISSUE_BODY)?, false),
    };
    if !pinned {
        gh.pin_issue(number)?;
    }
    gh.post_comment(number, report)?;
    Ok(())
}
