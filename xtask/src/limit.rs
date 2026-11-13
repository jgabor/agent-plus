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
//! a shared limit would need a slice for every job, set up and cleaned up
//! apart from `systemd-run`, in a way that holds if `xtask` dies. It would also
//! change what going over the limit does: the kernel picks from every job in
//! the slice, so a job that stayed small can die for a nested one.
//!
//! That cost buys little. A runaway still dies at its own job's limit, and
//! only the total can pass one limit: it can reach one for each job alive at
//! once. Jobs nest when a test of `xtask` runs `xtask`, which is small, or
//! when someone wraps a step, as in `cargo xtask run cargo xtask gate`.

use crate::Problem;
use crate::lock::Hold;
use std::env;
use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// For the whole job. A process that takes it over the limit is killed, and the
/// others go on, so a runaway test fails. A job from `job_script` dies whole.
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
        .args(options(limit, "continue")?)
        .arg("--")
        .arg(program)
        .args(arguments);
    Ok(command)
}

/// A shell script that runs its arguments as a job of its own. Going over the
/// job's memory limit kills the whole job at once. Otherwise the kernel can
/// kill a process and leave its children running, or pick the script's caller.
/// The job is stopped with the job that ran the script, which keeps its time
/// limit.
pub fn job_script() -> Result<String, Problem> {
    let options: Vec<String> = options(None, "kill")?.iter().map(|o| quote(o)).collect();
    Ok(format!(
        "#!/bin/sh\nouter=\"$(basename \"$(cut -d: -f3 /proc/self/cgroup)\")\"\nexec systemd-run {} -p \"BindsTo=$outer\" -p \"After=$outer\" -- \"$@\"\n",
        options.join(" ")
    ))
}

fn options(limit: Option<u64>, out_of_memory: &str) -> Result<Vec<String>, Problem> {
    let mut options: Vec<String> = ["--user", "--scope", "--quiet", "--collect"]
        .map(String::from)
        .to_vec();
    options.push("--expand-environment=no".to_string());
    for property in [
        format!("MemoryMax={}", memory()?),
        "MemorySwapMax=0".to_string(),
        format!("OOMPolicy={out_of_memory}"),
    ] {
        options.push("-p".to_string());
        options.push(property);
    }
    if let Some(limit) = limit {
        for property in [
            format!("RuntimeMaxSec={limit}"),
            format!("TimeoutStopSec={STOP_SECONDS}"),
        ] {
            options.push("-p".to_string());
            options.push(property);
        }
    }
    Ok(options)
}

/// Quotes `word` for `sh`.
fn quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
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
