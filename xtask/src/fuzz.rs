//! The nightly fuzz step: runs `crates/fuzz` inside its limits.

use crate::lock::Hold;
use crate::workspace;
use crate::{Problem, Verdict, limit};
use std::ffi::OsStr;
use std::path::Path;

const PACKAGE: &str = "fuzz";
const SECONDS: u64 = 3600;
const SECONDS_OVERRIDE: &str = "XTASK_FUZZ_SECONDS";
/// Taken off the fuzzer's own deadline, so only a hung target meets the limit.
const MARGIN: u64 = 300;
const BUILD_SECONDS: u64 = 300;

/// `None` when the workspace has no fuzzer.
pub fn run(root: &Path, hold: &Hold) -> Result<Option<Verdict>, Problem> {
    let packages = workspace::packages(root)?;
    if !packages.iter().any(|package| package.name == PACKAGE) {
        return Ok(None);
    }
    let seconds = limit::seconds(SECONDS_OVERRIDE, SECONDS)?;
    let target = root.join("target");
    let build: Vec<String> = [
        "build",
        "--release",
        "--locked",
        "--package",
        PACKAGE,
        "--target-dir",
    ]
    .map(String::from)
    .into_iter()
    .chain([target.display().to_string()])
    .collect();
    let on = OsStr::new("true");
    let no_compiler = OsStr::new("false");
    let built = limit::run(
        hold,
        root,
        Path::new("cargo"),
        &build,
        // An overflow is a bug the fuzzer should find, so it stays a panic.
        &[
            ("CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS", on),
            ("CARGO_PROFILE_RELEASE_OVERFLOW_CHECKS", on),
            ("CC", no_compiler),
            ("CXX", no_compiler),
        ],
        BUILD_SECONDS,
    )?;
    if !built.status.success() {
        return Err(Problem("couldn't build the fuzzer".to_string()));
    }
    let finished = limit::run(
        hold,
        root,
        &target.join("release").join(PACKAGE),
        &[(seconds - MARGIN.min(seconds / 2)).to_string()],
        &[],
        seconds,
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
