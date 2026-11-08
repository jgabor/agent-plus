//! Runs a program that can run away, inside limits on its memory and its time.
//!
//! A mutant can allocate without end, and one took a whole session down with it
//! before these limits existed.

use crate::Problem;
use std::env;
use std::ffi::OsStr;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

/// Each process's address space, in KiB. A program that passes it fails to
/// allocate and aborts, which for a mutant's test counts as caught.
const MEMORY_KIB: u64 = 8 * 1024 * 1024;
/// Runs with a limit set, or doesn't run at all.
const WITH_MEMORY_LIMIT: &str = r#"ulimit -v "$1" && shift && exec "$@""#;

pub struct Finished {
    pub status: ExitStatus,
    pub timed_out: bool,
}

/// How long a step may run: `variable` if it is set, or else `default`.
pub fn seconds(variable: &str, default: u64) -> Result<u64, Problem> {
    let Some(value) = env::var_os(variable) else {
        return Ok(default);
    };
    value
        .to_str()
        .and_then(|text| text.parse().ok())
        .ok_or_else(|| Problem(format!("{variable} must be a whole number of seconds")))
}

/// Runs `program` in `root`, sending its output to stderr. At the time limit it
/// kills the program and everything the program started.
pub fn run(
    root: &Path,
    program: &Path,
    arguments: &[String],
    variables: &[(&str, &OsStr)],
    limit: u64,
) -> Result<Finished, Problem> {
    let mut child = Command::new("/bin/sh")
        .args(["-c", WITH_MEMORY_LIMIT, "sh"])
        .arg(MEMORY_KIB.to_string())
        .arg(program)
        .args(arguments)
        .envs(variables.iter().copied())
        .current_dir(root)
        .stdout(Stdio::from(io::stderr()))
        .process_group(0)
        .spawn()
        .map_err(|error| Problem(format!("couldn't start `{}`: {error}", program.display())))?;
    let group = format!("-{}", child.id());
    let (finished, wait) = mpsc::channel::<()>();
    let watchdog = thread::spawn(move || {
        let waited = wait.recv_timeout(Duration::from_secs(limit));
        if !matches!(waited, Err(RecvTimeoutError::Timeout)) {
            return false;
        }
        let killed = Command::new("kill")
            .args(["-s", "KILL", "--", &group])
            .status();
        if !killed.is_ok_and(|status| status.success()) {
            eprintln!("limit: couldn't kill process group {group}");
        }
        true
    });
    let status = child.wait().map_err(|error| {
        Problem(format!(
            "couldn't wait for `{}`: {error}",
            program.display()
        ))
    });
    drop(finished);
    let timed_out = watchdog
        .join()
        .map_err(|_| Problem("the time limit's thread panicked".to_string()))?;
    Ok(Finished {
        status: status?,
        timed_out,
    })
}
