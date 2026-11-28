//! A command that runs in a process group of its own, which everything it
//! starts joins, so that the whole group can be stopped, even once `xtask`
//! has died.

use std::io::{self, PipeWriter};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

/// Stops the group when `xtask` dies, as on Ctrl-C, which reaches only the
/// group of `xtask`. Only `xtask` holds the other end of its input, so the
/// input ends when `xtask` does. It runs the same `kill` as [`stop`], because
/// dash's builtin refuses `--`. A merge's supervisor has the same watcher, for
/// the same reason.
const WATCHER: &str = "read _; env kill -STOP -- \"-$1\"; env kill -KILL -- \"-$1\"";

/// Stops the group if `xtask` dies, until [`Watcher::end`].
pub struct Watcher {
    process: Child,
    _alive: PipeWriter,
}

impl Watcher {
    /// Ends the watcher without its stopping anything. Called before the
    /// leader is reaped, while the group is still the command's, the watcher
    /// never signals a group that is gone.
    pub fn end(mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// Starts `command` as the leader of a process group of its own, with its
/// watcher. The group's id is the leader's.
pub fn start(command: &mut Command) -> io::Result<(Child, Watcher)> {
    // Made first, so that nothing can fail between the command's start and
    // its watcher's.
    let (watched, alive) = io::pipe()?;
    let mut leader = command.process_group(0).spawn()?;
    let group = leader.id();
    let watcher = Command::new("sh")
        .args(["-c", WATCHER, "sh", &group.to_string()])
        .stdin(watched)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();
    match watcher {
        Ok(process) => Ok((
            leader,
            Watcher {
                process,
                _alive: alive,
            },
        )),
        Err(error) => {
            stop(group);
            let _ = leader.wait();
            Err(error)
        }
    }
}

/// Stops the group first, so that none of it runs while the kill goes through
/// it. `xtask` has no dependencies, so `kill` sends the signals, and it fails
/// on a group that is gone.
pub fn stop(group: u32) {
    let target = format!("-{group}");
    for signal in ["-STOP", "-KILL"] {
        let _ = Command::new("kill")
            .args([signal, "--", &target])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}
