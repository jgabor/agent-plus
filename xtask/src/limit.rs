//! Runs a program that can run away as one contained job, with limits on its
//! memory and its time.
//!
//! A mutant can allocate without end, and one took a whole session down with it
//! before these limits existed. The job is a systemd scope because nothing less
//! covers it: `ulimit` counts each process alone, and `cargo mutants` starts
//! its workers in process groups of their own. If there's no scope, nothing
//! runs.

use crate::Problem;
use std::env;
use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// For the whole job. A process that takes it over the limit is killed, and the
/// others go on, so a mutant's test fails and the mutant counts as caught.
const MEMORY: &str = "MemoryMax=8G";
/// What a job gets to wind down, once the time limit has asked it to stop.
const STOP_SECONDS: u64 = 30;

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

/// Runs `program` in `root`, sending its output to stderr. If `xtask` dies, the
/// job's limits still hold.
pub fn run(
    root: &Path,
    program: &Path,
    arguments: &[String],
    variables: &[(&str, &OsStr)],
    limit: u64,
) -> Result<Finished, Problem> {
    let started = Instant::now();
    let status = Command::new("systemd-run")
        .args(["--user", "--scope", "--quiet", "--collect"])
        .args([
            "-p",
            MEMORY,
            "-p",
            "MemorySwapMax=0",
            "-p",
            "OOMPolicy=continue",
        ])
        .arg("-p")
        .arg(format!("RuntimeMaxSec={limit}"))
        .arg("-p")
        .arg(format!("TimeoutStopSec={STOP_SECONDS}"))
        .arg("--")
        .arg(program)
        .args(arguments)
        .envs(variables.iter().copied())
        .current_dir(root)
        .stdout(Stdio::from(io::stderr()))
        .status()
        .map_err(|error| Problem(format!("couldn't run `systemd-run`: {error}")))?;
    Ok(Finished {
        status,
        timed_out: started.elapsed() >= Duration::from_secs(limit),
    })
}
