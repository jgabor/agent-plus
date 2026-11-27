//! Runs each package's tests, a few packages at a time, and keeps the result
//! of every test by name.

use crate::lock::{self, Hold};
use crate::{Problem, limit, workspace};
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::iter::Peekable;
use std::path::Path;
use std::process::{Child, Stdio};
use std::slice::Iter;
use std::sync::mpsc::{Sender, channel};
use std::thread;

/// How many packages' tests run at once. On 16 CPUs, four at a time took a
/// run of every package from 413 seconds to 190.
const AT_ONCE: usize = 4;
/// Replaces `AT_ONCE`, so that a test can run fewer.
const AT_ONCE_OVERRIDE: &str = "XTASK_PACKAGES_AT_ONCE";

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

    fn record(&mut self, package: &str, ended: Ended) {
        for (function, passed) in ended.tests {
            self.by_test
                .entry((package.to_string(), function))
                .and_modify(|all| *all &= passed)
                .or_insert(passed);
        }
        self.all_passed &= ended.passed;
    }
}

/// What one package's `cargo test` reported: each test's function name and
/// whether it passed, and whether the whole run passed.
struct Ended {
    tests: Vec<(String, bool)>,
    passed: bool,
}

pub fn run(root: &Path, hold: &mut Hold) -> Result<Results, Problem> {
    run_except(root, hold, None)
}

/// Runs every package's tests but those of `left_out`.
pub fn run_except(
    root: &Path,
    hold: &mut Hold,
    left_out: Option<&str>,
) -> Result<Results, Problem> {
    let mut results = Results {
        by_test: HashMap::new(),
        all_passed: true,
    };
    let packages: Vec<String> = workspace::packages(root)?
        .into_iter()
        .map(|package| package.name)
        .filter(|name| Some(name.as_str()) != left_out)
        .collect();
    run_packages(root, &packages, hold, &mut results)?;
    Ok(results)
}

/// Says whether the package's tests passed.
pub fn run_package(
    root: &Path,
    package: &str,
    hold: &mut Hold,
    results: &mut Results,
) -> Result<bool, Problem> {
    run_packages(root, &[package.to_string()], hold, results)
}

/// Runs the packages' tests, up to `AT_ONCE` packages at a time, each in a
/// `cargo test --package` of its own, so that no package is built with
/// another's features. Says whether they all passed. A package that fails
/// leaves the others to finish.
///
/// Before each package, waiting work goes first, as `Hold::between` says. It
/// can let go of the place and the activity lock, which every package that
/// runs shares, so it runs only while none does. While some run and work
/// waits, the run starts no more until they have ended.
fn run_packages(
    root: &Path,
    packages: &[String],
    hold: &mut Hold,
    results: &mut Results,
) -> Result<bool, Problem> {
    if !hold.has_place() {
        return Err(Problem(
            "a run of the tests needs a place. Take its hold with `lock::testing`".to_string(),
        ));
    }
    let at_once = lock::count(AT_ONCE_OVERRIDE, AT_ONCE)?;
    let (sender, receiver) = channel();
    let mut waiting = packages.iter().peekable();
    let mut running = 0;
    let mut passed = true;
    let mut problem = None;
    loop {
        if problem.is_none()
            && let Err(e) = start(root, hold, at_once, &mut waiting, &mut running, &sender)
        {
            problem = Some(e);
        }
        if running == 0 {
            break;
        }
        let (package, ended) = receiver.recv().expect("each package sends once");
        running -= 1;
        match ended {
            Ok(ended) => {
                passed &= ended.passed;
                results.record(&package, ended);
            }
            Err(e) => {
                passed = false;
                results.all_passed = false;
                problem.get_or_insert(e);
            }
        }
    }
    match problem {
        Some(problem) => Err(problem),
        None => Ok(passed),
    }
}

/// Starts waiting packages until `at_once` run, or until work waits for the
/// hold while a package runs.
fn start(
    root: &Path,
    hold: &mut Hold,
    at_once: usize,
    waiting: &mut Peekable<Iter<String>>,
    running: &mut usize,
    sender: &Sender<(String, Result<Ended, Problem>)>,
) -> Result<(), Problem> {
    while *running < at_once && waiting.peek().is_some() {
        if *running == 0 {
            hold.between()?;
        } else if hold.waits()? {
            return Ok(());
        }
        let package = waiting.next().expect("a package waits").clone();
        let child = spawn(root, &package, hold)?;
        let sender = sender.clone();
        thread::spawn(move || {
            let ended = watch(child);
            let _ = sender.send((package, ended));
        });
        *running += 1;
    }
    Ok(())
}

/// Starts the package's `cargo test`. `CC` and `CXX` point at a program that
/// fails, as in the gate. The package's build and tests share one memory
/// limit, and a test that goes over it is killed, which fails the run. In a
/// merge, our input is the pipe to the supervisor, which the tests mustn't
/// reach.
fn spawn(root: &Path, package: &str, hold: &Hold) -> Result<Child, Problem> {
    let arguments = ["test", "--package", package, "--locked", "--no-fail-fast"];
    let mut command = limit::scope("cargo", &arguments, None)?;
    command
        .current_dir(root)
        .env("CC", "false")
        .env("CXX", "false")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    hold.pass_to(&mut command);
    command
        .spawn()
        .map_err(|e| Problem(format!("couldn't run `systemd-run`: {e}")))
}

/// Reads a package's results from its stdout as its tests run. Both of its
/// streams go to our stderr a whole line at a time, so that stdout carries
/// only the result, and a line from one package is never broken by another's.
fn watch(mut child: Child) -> Result<Ended, Problem> {
    let stderr = child.stderr.take().expect("stderr was piped");
    let errors = thread::spawn(move || forward(stderr, |_| {}));
    let stdout = child.stdout.take().expect("stdout was piped");
    let mut tests = Vec::new();
    let read = forward(stdout, |line| {
        if let Some((function, passed)) = parse_result(line) {
            tests.push((function.to_string(), passed));
        }
    });
    let errors = errors.join().expect("forwarding stderr doesn't panic");
    let status = child
        .wait()
        .map_err(|e| Problem(format!("couldn't wait for `cargo test`: {e}")))?;
    read.and(errors)?;
    Ok(Ended {
        tests,
        passed: status.success(),
    })
}

/// Copies `stream` to our stderr a line at a time, and hands each line to
/// `each`. A last line without a newline gets one, so that the next line on
/// stderr, which can be another package's, starts a line of its own.
fn forward(stream: impl Read, mut each: impl FnMut(&str)) -> Result<(), Problem> {
    let mut reader = BufReader::new(stream);
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        let read = reader
            .read_until(b'\n', &mut bytes)
            .map_err(|e| Problem(format!("couldn't read test output: {e}")))?;
        if read == 0 {
            return Ok(());
        }
        if !bytes.ends_with(b"\n") {
            bytes.push(b'\n');
        }
        let _ = io::stderr().lock().write_all(&bytes);
        each(&String::from_utf8_lossy(&bytes));
    }
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
