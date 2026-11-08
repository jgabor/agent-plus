//! `cargo xtask gate`: the checks a change must pass.

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

pub fn run() -> Result<Verdict, Problem> {
    let root = workspace::root()?;
    let hold = lock::shared(&root)?;
    require_tools(&root)?;
    let mut verdict = Verdict::Pass;
    let findings = floors::check(&root)?;
    for finding in &findings {
        eprintln!("floor: {finding}");
    }
    record("floors", findings.is_empty(), &mut verdict);
    for check in CHECKS {
        let passed = cargo_passes(&root, check.args)?;
        record(check.name, passed, &mut verdict);
    }
    let results = testrun::run(&root, &hold)?;
    record("tests", results.all_passed(), &mut verdict);
    let score = score::compute(&root, &results)?;
    score::report_findings(&score);
    record("spec", score.findings.is_empty(), &mut verdict);
    compare_with_base(&root, &score, &mut verdict)?;
    let word = match verdict {
        Verdict::Pass => "pass",
        Verdict::Fail => "fail",
    };
    println!("gate: {word}");
    Ok(verdict)
}

fn compare_with_base(
    root: &Path,
    score: &score::Score,
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
    record("hills", score.proven >= floor, verdict);
    match base::missing_label(root, &commit)? {
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

fn record(name: &str, passed: bool, verdict: &mut Verdict) {
    println!("{} {name}", if passed { "pass" } else { "fail" });
    if !passed {
        *verdict = Verdict::Fail;
    }
}

/// Fails before any check runs when a tool is missing, because a missing tool
/// says nothing about the change.
fn require_tools(root: &Path) -> Result<(), Problem> {
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
fn cargo_passes(root: &Path, args: &[&str]) -> Result<bool, Problem> {
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
