//! `cargo xtask gate`: the checks a change must pass.

use crate::forge::Gh;
use crate::lock::Hold;
use crate::workspace::{self, no_cargo};
use crate::{Problem, Verdict, base, floors, ledger, lock, score, testrun};
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};

struct Check {
    name: &'static str,
    args: &'static [&'static str],
}

const CHECKS: &[Check] = &[
    Check {
        name: "format",
        args: &["fmt", "--all", "--check"],
    },
    Check {
        name: "lint",
        args: &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--locked",
            "--",
            "-D",
            "warnings",
        ],
    },
];

/// What a merge already knows, which changes two of the checks.
pub struct Merging<'a> {
    pub labels: &'a [String],
    pub owner: bool,
}

pub fn run() -> Result<Verdict, Problem> {
    let root = workspace::root()?;
    let hold = lock::shared(&root)?;
    let before = pushed_clean_head(&root);
    let (verdict, _) = check(&root, &hold, None)?;
    if let Err(Problem(message)) = post_result(&root, before, verdict) {
        eprintln!("gate: couldn't post `xtask/gate`, so the result is only here: {message}");
    }
    Ok(verdict)
}

/// HEAD, if the tree is what HEAD holds and a remote has that commit. Only then
/// does a result say something about a commit others can see.
fn pushed_clean_head(root: &Path) -> Result<Option<String>, Problem> {
    if !root.join(".git").exists()
        || !base::commit_exists(root, "HEAD")?
        || !ledger::changes(root)?.is_empty()
    {
        return Ok(None);
    }
    let remotes = base::git_text(root, &["branch", "-r", "--contains", "HEAD"])?;
    if remotes.trim().is_empty() {
        return Ok(None);
    }
    let head = base::git_text(root, &["rev-parse", "HEAD"])?;
    Ok(Some(head.trim().to_string()))
}

/// Posts the result, if the head that was checked is still the head.
fn post_result(
    root: &Path,
    before: Result<Option<String>, Problem>,
    verdict: Verdict,
) -> Result<(), Problem> {
    let Some(head) = before? else {
        return Ok(());
    };
    if pushed_clean_head(root)?.as_ref() != Some(&head) {
        return Ok(());
    }
    let state = match verdict {
        Verdict::Pass => "success",
        Verdict::Fail => "failure",
    };
    Gh::new(root).post_status(&head, "xtask/gate", state)
}

/// Runs every check, and returns the verdict and the spec score it earned.
pub fn check(
    root: &Path,
    hold: &Hold,
    merging: Option<&Merging>,
) -> Result<(Verdict, score::Score), Problem> {
    require_tools(root)?;
    let mut verdict = Verdict::Pass;
    let findings = floors::check(root)?;
    for finding in &findings {
        eprintln!("floor: {finding}");
    }
    record("floors", findings.is_empty(), &mut verdict);
    for check in CHECKS {
        let passed = cargo_passes(root, check.args)?;
        record(check.name, passed, &mut verdict);
    }
    let results = testrun::run(root, hold)?;
    record("tests", results.all_passed(), &mut verdict);
    let score = score::compute(root, &results)?;
    score::report_findings(&score);
    record("spec", score.findings.is_empty(), &mut verdict);
    compare_with_base(root, &score, merging, &mut verdict)?;
    let word = match verdict {
        Verdict::Pass => "pass",
        Verdict::Fail => "fail",
    };
    println!("gate: {word}");
    Ok((verdict, score))
}

fn compare_with_base(
    root: &Path,
    score: &score::Score,
    merging: Option<&Merging>,
    verdict: &mut Verdict,
) -> Result<(), Problem> {
    let Some(commit) = base::find(root)? else {
        println!("skip base");
        eprintln!("base: no origin/main in this repository, so nothing was compared");
        return Ok(());
    };
    let findings = base::removed_checks(root, &commit, score)?;
    for finding in &findings {
        eprintln!("floor: {finding}");
    }
    record("checks-kept", findings.is_empty(), verdict);
    let floor = ledger::newest(root, &commit)?.map_or(0, |base| base.proven);
    if score.proven < floor {
        eprintln!(
            "hills: proven promises fell from {floor} to {}. Restore what proved them, or hand the PR to the owner",
            score.proven
        );
    }
    let allowed = merging.is_some_and(|merging| merging.owner);
    record("hills", score.proven >= floor || allowed, verdict);
    let missing = match merging {
        Some(merging) => Some(base::missing_label_on(root, &commit, merging.labels)?),
        None => base::missing_label(root, &commit)?,
    };
    match missing {
        Some(findings) => {
            for finding in &findings {
                eprintln!("labels: {finding}");
            }
            record("labels", findings.is_empty(), verdict);
        }
        None => {
            println!("skip labels");
            eprintln!("labels: this branch has no open PR yet, so its label wasn't checked");
        }
    }
    Ok(())
}

pub fn record(name: &str, passed: bool, verdict: &mut Verdict) {
    println!("{} {name}", if passed { "pass" } else { "fail" });
    if !passed {
        *verdict = Verdict::Fail;
    }
}

/// Fails before any check runs when a tool is missing, because a missing tool
/// says nothing about the change.
pub fn require_tools(root: &Path) -> Result<(), Problem> {
    for (tool, component) in [("fmt", "rustfmt"), ("clippy", "clippy")] {
        let output = Command::new("cargo")
            .args([tool, "--version"])
            .current_dir(root)
            .output()
            .map_err(no_cargo)?;
        if !output.status.success() {
            return Err(Problem(format!(
                "`cargo {tool}` can't run: {}. Install it with `rustup component add {component}`",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
    }
    Ok(())
}

/// Runs a cargo command, with its output on our stderr, so that stdout carries
/// only the result. `CC` and `CXX` point at a program that fails, so a build
/// that tries to compile C fails even if the floors missed it.
pub fn cargo_passes(root: &Path, args: &[&str]) -> Result<bool, Problem> {
    let status = Command::new("cargo")
        .args(args)
        .current_dir(root)
        .env("CC", "false")
        .env("CXX", "false")
        .stdout(Stdio::from(io::stderr()))
        .status()
        .map_err(no_cargo)?;
    Ok(status.success())
}
