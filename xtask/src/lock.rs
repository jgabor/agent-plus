//! The activity lock: builds and tests share it, and a bench holds it alone.
//! A run of the tests also takes one of a set number of places, so that only
//! that many run at once.

use crate::{Problem, Verdict, limit, workspace};
use std::env;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::sync_channel;
use std::thread;

/// Set to the lock directory on a command that runs under a hold, so a nested
/// `xtask` doesn't wait behind a pending bench for a lock its parent has.
const INHERITED: &str = "XTASK_ACTIVITY_HELD";
/// Set to the lock directory on a command that runs in a place, so a nested
/// `xtask` doesn't wait for a second one.
const PLACE_INHERITED: &str = "XTASK_PLACE_HELD";
/// How many runs of the tests share the machine at once. On 16 CPUs, gates
/// three at a time failed their tests in 13 of 18 runs, and two at a time in
/// none of 6.
const PLACES: usize = 2;
/// Replaces the number of places, so that a test needs few holders.
const PLACES_OVERRIDE: &str = "XTASK_PLACES";

/// The kernel drops these locks when the files close, including when the
/// process is killed.
pub struct Hold {
    files: Vec<File>,
    directory: PathBuf,
    activity: bool,
    place: bool,
}

impl Hold {
    /// Lets a child process, and any `xtask` it starts, share this hold.
    pub fn pass_to(&self, command: &mut Command) {
        if self.activity {
            command.env(INHERITED, &self.directory);
        }
        if self.place {
            command.env(PLACE_INHERITED, &self.directory);
        }
    }

    pub fn has_place(&self) -> bool {
        self.place
    }

    /// Adds a shared hold of the activity lock to a place. A place comes
    /// first, so a holder never waits for a place while a bench waits for it.
    pub fn share(self) -> Result<Hold, Problem> {
        let mut shared = shared_in(&self.directory)?;
        shared.files.extend(self.files);
        shared.place = self.place;
        Ok(shared)
    }

    /// For a child that must keep this hold until it exits, even if this
    /// process dies first. The kernel ties a lock to the open file, which a
    /// child's stdin shares. A hold inherited from a parent has no file, and
    /// the parent keeps it.
    pub fn as_stdin(&self) -> Result<Stdio, Problem> {
        let Some(file) = self.files.first() else {
            return Ok(Stdio::null());
        };
        let file = file
            .try_clone()
            .map_err(|e| Problem(format!("couldn't share the lock: {e}")))?;
        Ok(Stdio::from(file))
    }
}

pub fn shared(root: &Path) -> Result<Hold, Problem> {
    shared_in(&directory(root)?)
}

fn shared_in(directory: &Path) -> Result<Hold, Problem> {
    if inherited(INHERITED, directory) {
        return Ok(Hold {
            files: Vec::new(),
            directory: directory.to_path_buf(),
            activity: true,
            place: false,
        });
    }
    take_shared(directory)
}

/// For a run of the tests: a place, and then a shared hold.
pub fn testing(root: &Path) -> Result<Hold, Problem> {
    place(root)?.share()
}

/// One of the places, which a caller then shares with `Hold::share`. A merge
/// waits for its place apart from the rest, so that the wait doesn't count
/// against its timeout.
pub fn place(root: &Path) -> Result<Hold, Problem> {
    let directory = directory(root)?;
    let mut files = Vec::new();
    if !inherited(PLACE_INHERITED, &directory) {
        if inherited(INHERITED, &directory) {
            return Err(Problem(
                "the tests can't wait for a place under a hold without one, because a pending bench would wait for them".to_string(),
            ));
        }
        files.push(take_place(&directory, places()?)?);
    }
    Ok(Hold {
        files,
        directory,
        activity: false,
        place: true,
    })
}

fn places() -> Result<usize, Problem> {
    let Some(value) = env::var_os(PLACES_OVERRIDE) else {
        return Ok(PLACES);
    };
    value
        .to_str()
        .and_then(|text| text.parse().ok())
        .filter(|&count| count > 0)
        .ok_or_else(|| Problem(format!("{PLACES_OVERRIDE} must be a number above 0")))
}

fn inherited(name: &str, directory: &Path) -> bool {
    env::var_os(name).is_some_and(|held| Path::new(&held) == directory)
}

pub fn exclusive(root: &Path) -> Result<Hold, Problem> {
    let directory = directory(root)?;
    if inherited(INHERITED, &directory) {
        return Err(Problem(
            "a bench can't run under `cargo xtask run`, because it would wait for its own parent"
                .to_string(),
        ));
    }
    take_exclusive(&directory)
}

/// Whether a bench waits for holders that share the activity lock: something
/// keeps the turnstile, and the lock is held, but only shared. So a process
/// that is only told a bench waits can see for itself that it does. A bench
/// that holds the lock, or waits behind another bench, doesn't count. Each
/// try lets go once this returns. `directory` is the lock directory.
pub fn bench_waits(directory: &Path) -> Result<bool, Problem> {
    let free = |file: &File, shared: bool| {
        let tried = if shared {
            file.try_lock_shared()
        } else {
            file.try_lock()
        };
        match tried {
            Ok(()) => Ok(true),
            Err(TryLockError::WouldBlock) => Ok(false),
            Err(TryLockError::Error(e)) => Err(failed(e)),
        }
    };
    let turnstile = open(directory, "turnstile.lock")?;
    if free(&turnstile, false)? {
        return Ok(false);
    }
    let activity = open(directory, "activity.lock")?;
    Ok(!free(&activity, false)? && free(&activity, true)?)
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

/// Held for a whole response to a failing gate, so that two can't share the
/// tree its bisect checks commits out in, or file the same issue.
pub fn response_slot(root: &Path) -> Result<Hold, Problem> {
    slot(root, "nightly.lock")
}

fn slot(root: &Path, name: &str) -> Result<Hold, Problem> {
    let directory = directory(root)?;
    let file = open(&directory, name)?;
    file.lock()
        .map_err(|e| Problem(format!("couldn't take {name}: {e}")))?;
    Ok(Hold {
        files: vec![file],
        directory,
        activity: false,
        place: false,
    })
}

pub fn run(command: &[&str]) -> Result<Verdict, Problem> {
    let (program, arguments) = command
        .split_first()
        .ok_or_else(|| Problem("`run` needs a command to run".to_string()))?;
    let root = workspace::root()?;
    let hold = testing(&root)?;
    let path = find(program)
        .ok_or_else(|| Problem(format!("couldn't run `{program}`: no such executable")))?;
    if let Some(interpreter) = missing_interpreter(&path) {
        return Err(Problem(format!(
            "couldn't run `{program}`: no such interpreter `{interpreter}`"
        )));
    }
    let mut child = limit::scope(program, arguments, None)?;
    hold.pass_to(&mut child);
    let status = child
        .status()
        .map_err(|e| Problem(format!("couldn't run `systemd-run`: {e}")))?;
    Ok(if status.success() {
        Verdict::Pass
    } else {
        Verdict::Fail
    })
}

/// `systemd-run` exits 1 for a program it can't start, which would look like a
/// failed check. A name without a slash is looked up on `PATH`, as `exec` does.
fn find(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let path = PathBuf::from(program);
        return is_executable(&path).then_some(path);
    }
    env::split_paths(&env::var_os("PATH")?)
        .map(|directory| directory.join(program))
        .find(|path| is_executable(path))
}

/// The interpreter a script's `#!` line names, if it can't be run. The kernel
/// reads the first 256 bytes, and takes the first word as a path, with no
/// `PATH` lookup. A word cut off by that limit doesn't run as a script at all,
/// so it is left to `exec`.
fn missing_interpreter(script: &Path) -> Option<String> {
    const LIMIT: usize = 256;
    let mut start = Vec::with_capacity(LIMIT);
    File::open(script)
        .and_then(|f| f.take(LIMIT as u64).read_to_end(&mut start))
        .ok()?;
    let line = start.strip_prefix(b"#!")?;
    let blank = |b: &u8| matches!(b, b' ' | b'\t');
    let begin = line.iter().position(|b| !blank(b))?;
    let word = &line[begin..];
    let length = word.iter().position(|b| blank(b) || matches!(b, b'\n' | 0));
    if length.is_none() && start.len() == LIMIT {
        return None;
    }
    let word = &word[..length.unwrap_or(word.len())];
    let path = Path::new(OsStr::from_bytes(word));
    (!word.is_empty() && !is_executable(path)).then(|| path.display().to_string())
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Every holder passes through the turnstile, which a pending bench keeps, so
/// a bench isn't starved by a stream of new holders.
fn take_shared(directory: &Path) -> Result<Hold, Problem> {
    let turnstile = open(directory, "turnstile.lock")?;
    turnstile.lock().map_err(failed)?;
    let activity = open(directory, "activity.lock")?;
    activity.lock_shared().map_err(failed)?;
    Ok(Hold {
        files: vec![activity],
        directory: directory.to_path_buf(),
        activity: true,
        place: false,
    })
}

fn take_exclusive(directory: &Path) -> Result<Hold, Problem> {
    let turnstile = open(directory, "turnstile.lock")?;
    turnstile.lock().map_err(failed)?;
    let activity = open(directory, "activity.lock")?;
    activity.lock().map_err(failed)?;
    Ok(Hold {
        files: vec![turnstile, activity],
        directory: directory.to_path_buf(),
        activity: true,
        place: false,
    })
}

/// Waiters queue for the first free place one at a time. `flock` can't wait
/// for any one of several files, so the first in the queue waits on each
/// place in a thread of its own, and keeps the first it gets. A thread that
/// gets one later finds nobody to hand it to, and frees it at once.
fn take_place(directory: &Path, count: usize) -> Result<File, Problem> {
    let couldnt = |e: std::io::Error| Problem(format!("couldn't take a place: {e}"));
    let queue = open(directory, "places.lock")?;
    queue.lock().map_err(couldnt)?;
    let names: Vec<String> = (0..count).map(|i| format!("place-{i}.lock")).collect();
    for name in &names {
        let file = open(directory, name)?;
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(e)) => return Err(couldnt(e)),
        }
    }
    let (taken, first) = sync_channel(0);
    for name in names {
        let file = open(directory, &name)?;
        let taken = taken.clone();
        thread::spawn(move || {
            if file.lock().is_ok() {
                let _ = taken.send(file);
            }
        });
    }
    drop(taken);
    first
        .recv()
        .map_err(|_| Problem("couldn't take a place: every wait failed".to_string()))
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
pub fn directory(root: &Path) -> Result<PathBuf, Problem> {
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
    use super::{bench_waits, open, take_exclusive, take_shared};
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
    fn a_bench_waits_only_while_the_turnstile_is_kept_and_the_lock_is_shared() {
        let path = directory("bench-waits");
        let waits = || bench_waits(&path).map_err(|e| e.0).unwrap();
        assert!(!waits(), "nothing holds the lock");
        let shared = take_shared(&path).map_err(|e| e.0).unwrap();
        assert!(!waits(), "no bench keeps the turnstile");
        let turnstile = open(&path, "turnstile.lock").map_err(|e| e.0).unwrap();
        turnstile.lock().unwrap();
        assert!(waits(), "a bench waits for a holder that shares the lock");
        drop((shared, turnstile));
        let bench = take_exclusive(&path).map_err(|e| e.0).unwrap();
        assert!(!waits(), "the bench holds the lock");
        drop(bench);
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
