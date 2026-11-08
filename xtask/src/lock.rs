//! The activity lock: builds and tests share it, and a bench holds it alone.

use crate::{Problem, Verdict, workspace};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Set to the lock directory on a command that runs under a hold, so a nested
/// `xtask` doesn't wait behind a pending bench for a lock its parent has.
const INHERITED: &str = "XTASK_ACTIVITY_HELD";

/// The kernel drops these locks when the files close, including when the
/// process is killed.
pub struct Hold {
    _files: Vec<File>,
    directory: PathBuf,
}

impl Hold {
    /// Lets a child process, and any `xtask` it starts, share this hold.
    pub fn pass_to(&self, command: &mut Command) {
        command.env(INHERITED, &self.directory);
    }
}

pub fn shared(root: &Path) -> Result<Hold, Problem> {
    let directory = directory(root)?;
    if inherited(&directory) {
        return Ok(Hold {
            _files: Vec::new(),
            directory,
        });
    }
    take_shared(&directory)
}

fn inherited(directory: &Path) -> bool {
    env::var_os(INHERITED).is_some_and(|held| Path::new(&held) == directory)
}

pub fn exclusive(root: &Path) -> Result<Hold, Problem> {
    let directory = directory(root)?;
    if inherited(&directory) {
        return Err(Problem(
            "a bench can't run under `cargo xtask run`, because it would wait for its own parent"
                .to_string(),
        ));
    }
    take_exclusive(&directory)
}

/// Held while a claim reads and writes GitHub, so that two claims on this
/// machine can't interleave. Rules on the comments cover anything else.
pub fn claim_slot(root: &Path) -> Result<Hold, Problem> {
    slot(root, "claim.lock")
}

/// Held for a whole merge, so that only one runs at a time and each is tested
/// on top of the last.
pub fn merge_slot(root: &Path) -> Result<Hold, Problem> {
    slot(root, "merge.lock")
}

fn slot(root: &Path, name: &str) -> Result<Hold, Problem> {
    let directory = directory(root)?;
    let file = open(&directory, name)?;
    file.lock()
        .map_err(|e| Problem(format!("couldn't take {name}: {e}")))?;
    Ok(Hold {
        _files: vec![file],
        directory,
    })
}

pub fn run(command: &[&str]) -> Result<Verdict, Problem> {
    let (program, arguments) = command
        .split_first()
        .ok_or_else(|| Problem("`run` needs a command to run".to_string()))?;
    let root = workspace::root()?;
    let hold = shared(&root)?;
    let mut child = Command::new(program);
    child.args(arguments);
    hold.pass_to(&mut child);
    let status = child
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
        directory: directory.to_path_buf(),
    })
}

fn take_exclusive(directory: &Path) -> Result<Hold, Problem> {
    let turnstile = open(directory, "turnstile.lock")?;
    turnstile.lock().map_err(failed)?;
    let activity = open(directory, "activity.lock")?;
    activity.lock().map_err(failed)?;
    Ok(Hold {
        _files: vec![turnstile, activity],
        directory: directory.to_path_buf(),
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
    fs::canonicalize(&directory)
        .map_err(|e| Problem(format!("couldn't resolve {}: {e}", directory.display())))
}

#[cfg(test)]
mod tests {
    use super::{open, take_exclusive, take_shared};
    use std::fs::{self, TryLockError};
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::{Receiver, channel};
    use std::thread;
    use std::time::{Duration, Instant};

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

    /// Returns once something holds the turnstile.
    fn wait_for_turnstile(path: &Path) {
        let probe = open(path, "turnstile.lock").map_err(|e| e.0).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match probe.try_lock() {
                Err(TryLockError::WouldBlock) => return,
                Ok(()) => probe.unlock().unwrap(),
                Err(error) => panic!("{error}"),
            }
            assert!(Instant::now() < deadline, "nothing took the turnstile");
            thread::yield_now();
        }
    }

    #[test]
    fn a_bench_waits_for_holders_and_new_holders_wait_for_the_bench() {
        let path = directory("priority");
        let first = take_shared(&path).map_err(|e| e.0).unwrap();

        let p = path.clone();
        let bench = holder(move || take_exclusive(&p).map_err(|e| e.0).unwrap());
        wait_for_turnstile(&path);
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
