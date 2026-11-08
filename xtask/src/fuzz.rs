//! The nightly fuzz step: runs `crates/fuzz` inside its limits.

use crate::workspace::{self, no_cargo};
use crate::{Problem, Verdict, limit};
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};

const PACKAGE: &str = "fuzz";
const SECONDS: u64 = 3600;
const SECONDS_OVERRIDE: &str = "XTASK_FUZZ_SECONDS";
/// How long past its own deadline a target may run before it counts as hung.
const GRACE: u64 = 300;

/// `None` when the workspace has no fuzzer.
pub fn run(root: &Path) -> Result<Option<Verdict>, Problem> {
    let packages = workspace::packages(root)?;
    if !packages.iter().any(|package| package.name == PACKAGE) {
        return Ok(None);
    }
    let seconds = limit::seconds(SECONDS_OVERRIDE, SECONDS)?;
    let target = root.join("target");
    let built = Command::new("cargo")
        .args(["build", "--release", "--locked", "--package", PACKAGE])
        .arg("--target-dir")
        .arg(&target)
        .current_dir(root)
        // An overflow is a bug the fuzzer should find, so it stays a panic.
        .env("CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS", "true")
        .env("CARGO_PROFILE_RELEASE_OVERFLOW_CHECKS", "true")
        .env("CC", "false")
        .env("CXX", "false")
        .stdout(Stdio::from(io::stderr()))
        .status()
        .map_err(no_cargo)?;
    if !built.success() {
        return Err(Problem("couldn't build the fuzzer".to_string()));
    }
    let finished = limit::run(
        root,
        &target.join("release").join(PACKAGE),
        &[seconds.to_string()],
        &[],
        seconds + seconds.min(GRACE),
    )?;
    if finished.timed_out {
        eprintln!("fuzz: a target hung past its {seconds} second limit");
        return Ok(Some(Verdict::Fail));
    }
    Ok(Some(if finished.status.success() {
        Verdict::Pass
    } else {
        Verdict::Fail
    }))
}
