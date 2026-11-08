//! Runs each package's tests, and keeps the result of every test by name.

use crate::Problem;
use crate::lock::Hold;
use crate::workspace::{self, no_cargo};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};

pub struct Results {
    by_test: HashMap<(String, String), bool>,
    all_passed: bool,
}

impl Results {
    pub fn all_passed(&self) -> bool {
        self.all_passed
    }

    /// True when every test with this function name in the package passed.
    pub fn passed(&self, package: &str, function: &str) -> bool {
        let key = (package.to_string(), function.to_string());
        self.by_test.get(&key).copied().unwrap_or(false)
    }
}

pub fn run(root: &Path, hold: &Hold) -> Result<Results, Problem> {
    let mut results = Results {
        by_test: HashMap::new(),
        all_passed: true,
    };
    for package in workspace::packages(root)? {
        run_package(root, &package.name, hold, &mut results)?;
    }
    Ok(results)
}

/// Test output goes to our stderr as it arrives, so that stdout carries only
/// the result. `CC` and `CXX` point at a program that fails, as in the gate.
fn run_package(
    root: &Path,
    package: &str,
    hold: &Hold,
    results: &mut Results,
) -> Result<(), Problem> {
    let mut command = Command::new("cargo");
    command
        .args(["test", "--package", package, "--locked", "--no-fail-fast"])
        .current_dir(root)
        .env("CC", "false")
        .env("CXX", "false")
        .stdout(Stdio::piped());
    hold.pass_to(&mut command);
    let mut child = command.spawn().map_err(no_cargo)?;
    let stdout = child.stdout.take().expect("stdout was piped");
    let mut reader = BufReader::new(stdout);
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        let read = reader
            .read_until(b'\n', &mut bytes)
            .map_err(|e| Problem(format!("couldn't read test output: {e}")))?;
        if read == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&bytes);
        eprint!("{line}");
        if let Some((function, passed)) = parse_result(&line) {
            results
                .by_test
                .entry((package.to_string(), function.to_string()))
                .and_modify(|all| *all &= passed)
                .or_insert(passed);
        }
    }
    let status = child
        .wait()
        .map_err(|e| Problem(format!("couldn't wait for `cargo test`: {e}")))?;
    results.all_passed &= status.success();
    Ok(())
}

/// Reads a `test path::to::name ... ok` line into the function name and
/// whether it passed. An ignored test didn't pass.
fn parse_result(line: &str) -> Option<(&str, bool)> {
    let rest = line.strip_prefix("test ")?;
    let (name, status) = rest.rsplit_once(" ... ")?;
    let path = name.split_whitespace().next()?;
    Some((path.rsplit("::").next()?, status.trim() == "ok"))
}

#[cfg(test)]
mod tests {
    use super::parse_result;

    #[test]
    fn reads_a_passing_test() {
        assert_eq!(parse_result("test a::b::c ... ok\n"), Some(("c", true)));
    }

    #[test]
    fn reads_a_failing_test() {
        assert_eq!(parse_result("test c ... FAILED\n"), Some(("c", false)));
    }

    #[test]
    fn an_ignored_test_did_not_pass() {
        assert_eq!(parse_result("test c ... ignored\n"), Some(("c", false)));
        assert_eq!(
            parse_result("test c ... ignored, flaky: #12\n"),
            Some(("c", false))
        );
    }

    #[test]
    fn reads_a_should_panic_test() {
        assert_eq!(
            parse_result("test a::c - should panic ... ok\n"),
            Some(("c", true))
        );
    }

    #[test]
    fn other_lines_are_not_results() {
        assert_eq!(parse_result("running 1 test\n"), None);
        assert_eq!(parse_result("test result: ok. 1 passed\n"), None);
    }
}
