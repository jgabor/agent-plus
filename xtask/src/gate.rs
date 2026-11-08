//! `cargo xtask gate`: the checks a change must pass.

use crate::{Problem, Verdict, floors};
use std::io;
use std::path::{Path, PathBuf};
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
    Check {
        name: "tests",
        args: &["test", "--workspace", "--locked", "--no-fail-fast"],
    },
];

pub fn run() -> Result<Verdict, Problem> {
    let root = workspace_root()?;
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
    let word = match verdict {
        Verdict::Pass => "pass",
        Verdict::Fail => "fail",
    };
    println!("gate: {word}");
    Ok(verdict)
}

fn record(name: &str, passed: bool, verdict: &mut Verdict) {
    println!("{} {name}", if passed { "pass" } else { "fail" });
    if !passed {
        *verdict = Verdict::Fail;
    }
}

fn no_cargo(error: io::Error) -> Problem {
    Problem(format!(
        "couldn't run `cargo`: {error}. Install Rust, or add cargo to PATH"
    ))
}

pub fn workspace_root() -> Result<PathBuf, Problem> {
    let output = Command::new("cargo")
        .args(["locate-project", "--workspace", "--message-format", "plain"])
        .output()
        .map_err(no_cargo)?;
    if !output.status.success() {
        return Err(Problem(format!(
            "not inside a Cargo workspace: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let manifest = String::from_utf8_lossy(&output.stdout);
    let manifest = Path::new(manifest.trim());
    let root = manifest
        .parent()
        .ok_or_else(|| Problem(format!("`{}` has no parent", manifest.display())))?;
    Ok(root.to_path_buf())
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
