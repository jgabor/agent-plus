//! `cargo xtask nightly`: the slow checks on `main`, then the report.

use crate::forge::{Forge, Gh};
use crate::report::ISSUE_TITLE;
use crate::{
    Problem, Verdict, base, bench, bisect, fuzz, gate, journey, ledger, lock, mutation, report,
    workspace,
};
use std::path::Path;

const ISSUE_BODY: &str = "Each night's report is a comment on this issue.";
const STATUS: &str = "xtask/nightly";
const PASS: &str = "pass";
const FAIL: &str = "fail";
const SKIP: &str = "skip";

pub fn run() -> Result<Verdict, Problem> {
    let root = workspace::root()?;
    let commit = tested_commit(&root)?;
    gate::require_tools(&root)?;

    // A nightly has no PR, so nothing can give a reason for a slowdown. Comparing
    // `main` with itself only measures it, which is why the bench comes first,
    // on the quietest machine.
    let mut checks = vec![("bench", state(bench::compare(&root, Some(""))))];
    let hold = lock::shared(&root)?;
    let gated = gate::check(&root, &hold, None).map(|(verdict, _)| verdict);
    let gated = state(gated);
    checks.push(("gate", gated));
    checks.push(("t2", t2(&root)));
    // No unattended run may spend money until HARNESS.md sets a limit.
    checks.push(("t3", SKIP));
    checks.push(("journey", state(journey_holds(&root))));
    checks.push(("fuzz", fuzz::run(&root).transpose().map_or(SKIP, state)));
    let mutated = mutation::run(&root).transpose();
    checks.push(("mutation", mutated.map_or(SKIP, state)));
    drop(hold);

    // A report that can't be built still leaves its result on the commit.
    let built = report::build(&root);
    let failed = built.is_err() || checks.iter().any(|(_, state)| *state == FAIL);
    let posted = built.and_then(|report| post(&root, &report.markdown(&checks)));
    let (verdict, status, word) = if failed {
        (Verdict::Fail, "failure", FAIL)
    } else {
        (Verdict::Pass, "success", PASS)
    };
    Gh::new(&root).post_status(&commit, STATUS, status)?;
    let responded = if gated == FAIL {
        bisect::respond(&root, &commit, STATUS)
    } else {
        Ok(())
    };
    posted?;
    responded?;
    for (name, state) in &checks {
        println!("{state} {name}");
    }
    println!("nightly: {word}");
    Ok(verdict)
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

/// A step that isn't built yet fails today, so what counts is a count lower
/// than the ledger's.
fn journey_holds(root: &Path) -> Result<Verdict, Problem> {
    let (passing, steps) = journey::count(root)?;
    println!("journey {passing} of {steps}");
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
