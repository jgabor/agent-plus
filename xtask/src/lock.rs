//! The activity lock: builds and tests share it, and a bench holds it alone.

use crate::{Problem, Verdict, workspace};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Set on a command that runs under a hold, so a nested `xtask` doesn't wait
/// behind a pending bench for a lock its parent already has.
const INHERITED: &str = "XTASK_ACTIVITY_HELD";

/// The kernel drops these locks when the files close, including when the
/// process is killed.
pub struct Hold {
    _files: Vec<File>,
}

pub fn shared(root: &Path) -> Result<Hold, Problem> {
    if env::var_os(INHERITED).is_some() {
        return Ok(Hold { _files: Vec::new() });
    }
    take_shared(&directory(root)?)
}

/// `cargo xtask bench` takes this in #15.
#[allow(dead_code)]
pub fn exclusive(root: &Path) -> Result<Hold, Problem> {
    if env::var_os(INHERITED).is_some() {
        return Err(Problem(
            "a bench can't run under `cargo xtask run`, because it would wait for its own parent"
                .to_string(),
        ));
    }
    take_exclusive(&directory(root)?)
}

pub fn run(command: &[&str]) -> Result<Verdict, Problem> {
    let (program, arguments) = command
        .split_first()
        .ok_or_else(|| Problem("`run` needs a command to run".to_string()))?;
    let root = workspace::root()?;
    let _hold = shared(&root)?;
    let status = Command::new(program)
        .args(arguments)
        .env(INHERITED, "1")
        .status()
        .map_err(|e| Problem(format!("couldn't run `{program}`: {e}")))?;
    Ok(if status.success() {
        Verdict::Pass
    } else {
        Verdict::Fail
    })
}

/// Every holder passes through the turnstile, which a pending bench keeps, so
/// a bench isn't starved by a stream of new holders.
fn take_shared(directory: &Path) -> Result<Hold, Problem> {
    let turnstile = open(directory, "turnstile.lock")?;
    turnstile.lock().map_err(failed)?;
    let activity = open(directory, "activity.lock")?;
    activity.lock_shared().map_err(failed)?;
    Ok(Hold {
        _files: vec![activity],
    })
}

fn take_exclusive(directory: &Path) -> Result<Hold, Problem> {
    let turnstile = open(directory, "turnstile.lock")?;
    turnstile.lock().map_err(failed)?;
    let activity = open(directory, "activity.lock")?;
    activity.lock().map_err(failed)?;
    Ok(Hold {
        _files: vec![turnstile, activity],
    })
}

fn failed(error: std::io::Error) -> Problem {
    Problem(format!("couldn't take the activity lock: {error}"))
}

fn open(directory: &Path, name: &str) -> Result<File, Problem> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(directory.join(name))
        .map_err(|e| Problem(format!("couldn't open {name}: {e}")))
}

/// The shared git directory, so that every worktree of a repository uses the
/// same lock. A workspace that isn't a repository uses its target directory.
fn directory(root: &Path) -> Result<PathBuf, Problem> {
    let base = if root.join(".git").exists() {
        let output = Command::new("git")
            .args(["rev-parse", "--git-common-dir"])
            .current_dir(root)
            .output()
            .map_err(|e| Problem(format!("couldn't run `git`: {e}")))?;
        if !output.status.success() {
            return Err(Problem(format!(
                "`git rev-parse --git-common-dir` failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        root.join(String::from_utf8_lossy(&output.stdout).trim())
    } else {
        root.join("target")
    };
    let directory = base.join("xtask");
    fs::create_dir_all(&directory)
        .map_err(|e| Problem(format!("couldn't create {}: {e}", directory.display())))?;
    Ok(directory)
}

#[cfg(test)]
mod tests {
    use super::{take_exclusive, take_shared};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::mpsc::{Receiver, channel};
    use std::thread;
    use std::time::Duration;

    const WAIT: Duration = Duration::from_millis(300);

    fn directory(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("xtask-lock-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    /// Takes a hold on another thread. The receiver gets a sender when the
    /// hold is taken, and the hold ends when that sender is used.
    fn holder<H: Send + 'static>(
        take: impl FnOnce() -> H + Send + 'static,
    ) -> Receiver<std::sync::mpsc::Sender<()>> {
        let (taken, receiver) = channel();
        thread::spawn(move || {
            let hold = take();
            let (release, released) = channel();
            taken.send(release).unwrap();
            released.recv().unwrap();
            drop(hold);
        });
        receiver
    }

    #[test]
    fn a_bench_waits_for_holders_and_new_holders_wait_for_the_bench() {
        let path = directory("priority");
        let first = take_shared(&path).map_err(|e| e.0).unwrap();

        let p = path.clone();
        let bench = holder(move || take_exclusive(&p).map_err(|e| e.0).unwrap());
        assert!(bench.recv_timeout(WAIT).is_err(), "the bench didn't wait");

        let p = path.clone();
        let later = holder(move || take_shared(&p).map_err(|e| e.0).unwrap());
        assert!(
            later.recv_timeout(WAIT).is_err(),
            "it went ahead of the bench"
        );

        drop(first);
        let bench_release = bench.recv().unwrap();
        assert!(later.recv_timeout(WAIT).is_err(), "it ran during the bench");

        bench_release.send(()).unwrap();
        later.recv().unwrap().send(()).unwrap();
        fs::remove_dir_all(&path).unwrap();
    }

    #[test]
    fn holders_share_the_lock() {
        let path = directory("shared");
        let first = take_shared(&path).map_err(|e| e.0).unwrap();
        let second = take_shared(&path).map_err(|e| e.0).unwrap();
        drop((first, second));
        fs::remove_dir_all(&path).unwrap();
    }
}
