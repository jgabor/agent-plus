//! Runs a program that can run away as one contained job, with limits on its
//! memory and its time.
//!
//! A mutant can allocate without end, and one took a whole session down with it
//! before these limits existed. The job is a systemd scope because nothing less
//! covers it: `ulimit` counts each process alone, and `cargo mutants` starts
//! its workers in process groups of their own. If there's no scope, nothing
//! runs.
//!
//! A job started inside a job keeps its own limit, and doesn't count against
//! the enclosing one. Each scope is a new unit under the user manager, so the
//! two are siblings, and a scope can't hold another unit. Only a slice can, so
//! a shared limit would need a slice for every job, with its limit set by
//! `systemctl set-property`. That leaves files in the manager's runtime
//! directory, and an `xtask` that dies wouldn't remove them. It would also
//! change what a job over its limit does, because the kernel then picks from
//! every job in the slice, so an innocent job can die for a nested one.
//!
//! So the total is at most `MEMORY` for each job alive at once. Jobs nest only
//! when a test of `xtask` runs `xtask`, and those fixtures are small.

use crate::Problem;
use crate::lock::Hold;
use std::env;
use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// For the whole job. A process that takes it over the limit is killed, and the
/// others go on, so a mutant's test fails and the mutant counts as caught.
const MEMORY: &str = "8G";
/// Replaces `MEMORY`, so that a test needn't allocate that much.
const MEMORY_OVERRIDE: &str = "XTASK_MEMORY_MAX";
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

/// A command that runs `program` as a contained job, with a limit on its memory
/// and, if `limit` is given, on its seconds. The job replaces `systemd-run`, so
/// the command's exit status is the job's. Without a systemd user session
/// `systemd-run` fails and the job doesn't start.
pub fn scope(
    program: impl AsRef<OsStr>,
    arguments: &[impl AsRef<OsStr>],
    limit: Option<u64>,
) -> Result<Command, Problem> {
    let mut command = Command::new("systemd-run");
    command
        .args(["--user", "--scope", "--quiet", "--collect"])
        .arg("--expand-environment=no")
        .arg("-p")
        .arg(format!("MemoryMax={}", memory()?))
        .args(["-p", "MemorySwapMax=0", "-p", "OOMPolicy=continue"]);
    if let Some(limit) = limit {
        command
            .arg("-p")
            .arg(format!("RuntimeMaxSec={limit}"))
            .arg("-p")
            .arg(format!("TimeoutStopSec={STOP_SECONDS}"));
    }
    command.arg("--").arg(program).args(arguments);
    Ok(command)
}

/// An empty `MemoryMax=` would mean no limit at all, so it's refused.
fn memory() -> Result<String, Problem> {
    let Some(value) = env::var_os(MEMORY_OVERRIDE) else {
        return Ok(MEMORY.to_string());
    };
    value
        .to_str()
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .ok_or_else(|| Problem(format!("{MEMORY_OVERRIDE} must be a size, such as 64M")))
}

/// Runs `program` in `root`, sending its output to stderr. If `xtask` dies, the
/// job's limits still hold, and so does the activity lock until the job ends.
pub fn run(
    hold: &Hold,
    root: &Path,
    program: &Path,
    arguments: &[String],
    variables: &[(&str, &OsStr)],
    limit: u64,
) -> Result<Finished, Problem> {
    let started = Instant::now();
    let status = scope(program, arguments, Some(limit))?
        .envs(variables.iter().copied())
        .current_dir(root)
        .stdin(hold.as_stdin()?)
        .stdout(Stdio::from(io::stderr()))
        .status()
        .map_err(|error| Problem(format!("couldn't run `systemd-run`: {error}")))?;
    Ok(Finished {
        status,
        timed_out: started.elapsed() >= Duration::from_secs(limit),
    })
}
