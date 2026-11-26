//! The activity lock: builds and tests share it, and a bench holds it alone.
//! A run of the tests also takes one of a set number of places, so that only
//! that many run at once.

use crate::{Problem, Verdict, limit, workspace};
use std::env;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
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
/// Kept by the merge slot's holder while it waits for a place, which every
/// other run lets it have first.
const AHEAD: &str = "ahead.lock";

/// What a take waits for, when it can't have its lock at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    Place,
    Activity,
}

/// Hears each wait of this process for a place or the activity lock: what it
/// waits for as the wait starts, and `None` once the take has what it waited
/// for.
pub type Watcher = Box<dyn Fn(Option<Wait>) + Send + Sync>;

static WATCHER: OnceLock<Watcher> = OnceLock::new();

/// Lets this process hear its waits. Only the first watcher counts.
pub fn watch(watcher: Watcher) {
    let _ = WATCHER.set(watcher);
}

fn tell(wait: Option<Wait>) {
    if let Some(watcher) = WATCHER.get() {
        watcher(wait);
    }
}

/// A wait that started, which ends as this drops.
struct Waiting;

impl Waiting {
    /// Starts a wait for `what`, unless the take already waits.
    fn start(waiting: &mut Option<Waiting>, what: Wait) {
        if waiting.is_none() {
            tell(Some(what));
            *waiting = Some(Waiting);
        }
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        tell(None);
    }
}

/// Locks `file`, shared or alone, and starts a wait for `what` if it has to.
fn take(file: &File, shared: bool, what: Wait, waiting: &mut Option<Waiting>) -> io::Result<()> {
    let tried = if shared {
        file.try_lock_shared()
    } else {
        file.try_lock()
    };
    match tried {
        Ok(()) => return Ok(()),
        Err(TryLockError::WouldBlock) => {}
        Err(TryLockError::Error(e)) => return Err(e),
    }
    Waiting::start(waiting, what);
    if shared {
        file.lock_shared()
    } else {
        file.lock()
    }
}

/// How this process holds one of the locks: not at all, through a parent that
/// passed it down, or by a file of its own. The kernel drops a lock when its
/// file closes, including when the process is killed.
enum Held {
    No,
    Inherited,
    Own(File),
}

impl Held {
    fn any(&self) -> bool {
        !matches!(self, Held::No)
    }
}

pub struct Hold {
    directory: PathBuf,
    place: Held,
    /// The activity lock, shared for a build or a run of the tests, and held
    /// alone by a bench.
    activity: Held,
    /// A bench's turnstile, or a slot.
    files: Vec<File>,
    /// The merge slot's holder took the place, and never gives it up.
    ahead: bool,
}

impl Hold {
    fn new(directory: &Path) -> Hold {
        Hold {
            directory: directory.to_path_buf(),
            place: Held::No,
            activity: Held::No,
            files: Vec::new(),
            ahead: false,
        }
    }

    /// Lets a child process, and any `xtask` it starts, share this hold.
    pub fn pass_to(&self, command: &mut Command) {
        if self.activity.any() {
            command.env(INHERITED, &self.directory);
        }
        if self.place.any() {
            command.env(PLACE_INHERITED, &self.directory);
        }
    }

    pub fn has_place(&self) -> bool {
        self.place.any()
    }

    /// Adds a shared hold of the activity lock to a place. A place comes
    /// first, so a holder never waits for a place while a bench waits for it.
    pub fn share(mut self) -> Result<Hold, Problem> {
        self.activity = shared_activity(&self.directory)?;
        Ok(self)
    }

    /// Lets go of the activity lock, and keeps the place.
    pub fn keep_place(mut self) -> Hold {
        self.activity = Held::No;
        self
    }

    /// Between two parts of a run, such as two packages' tests, lets others
    /// go first. When the merge slot's holder waits for a place, it gets this
    /// one, and when a bench waits for the activity lock, it gets that. Then
    /// this takes back what it let go, in the usual order. Only a place of
    /// this process's own lets anything go, so the merge slot's holder, and a
    /// run whose parent passed its place down, go on.
    pub fn between(&mut self) -> Result<(), Problem> {
        if self.ahead || !matches!(self.place, Held::Own(_)) {
            return Ok(());
        }
        let shared = self.activity.any();
        if waits_ahead(&self.directory)? {
            self.activity = Held::No;
            self.place = Held::No;
            self.place = Held::Own(take_place(&self.directory, places()?, false)?);
        } else if shared && bench_waits(&self.directory)? {
            self.activity = Held::No;
        } else {
            return Ok(());
        }
        if shared {
            self.activity = Held::Own(take_shared(&self.directory)?);
        }
        Ok(())
    }

    /// For a child that must keep this hold until it exits, even if this
    /// process dies first. The kernel ties a lock to the open file, which a
    /// child's stdin shares. A hold inherited from a parent has no file, and
    /// the parent keeps it.
    pub fn as_stdin(&self) -> Result<Stdio, Problem> {
        let file = match &self.activity {
            Held::Own(file) => file,
            _ => match self.files.first() {
                Some(file) => file,
                None => return Ok(Stdio::null()),
            },
        };
        let file = file
            .try_clone()
            .map_err(|e| Problem(format!("couldn't share the lock: {e}")))?;
        Ok(Stdio::from(file))
    }
}

pub fn shared(root: &Path) -> Result<Hold, Problem> {
    let directory = directory(root)?;
    let mut hold = Hold::new(&directory);
    hold.activity = shared_activity(&directory)?;
    Ok(hold)
}

fn shared_activity(directory: &Path) -> Result<Held, Problem> {
    if inherited(INHERITED, directory) {
        return Ok(Held::Inherited);
    }
    Ok(Held::Own(take_shared(directory)?))
}

/// For a run of the tests: a place, and then a shared hold.
pub fn testing(root: &Path) -> Result<Hold, Problem> {
    place(root, None)?.share()
}

/// One of the places, which a caller then shares with `Hold::share`. A merge
/// waits for its place apart from the rest, so that the wait doesn't count
/// against its timeout. The holder of the merge slot, which it passes as
/// `slot`, gets its place ahead of every run that waits for one, and keeps
/// it to the end, because every merge waits for a proof under the slot.
pub fn place(root: &Path, slot: Option<&Hold>) -> Result<Hold, Problem> {
    let directory = directory(root)?;
    let mut hold = Hold::new(&directory);
    hold.ahead = slot.is_some();
    if inherited(PLACE_INHERITED, &directory) {
        hold.place = Held::Inherited;
        return Ok(hold);
    }
    if inherited(INHERITED, &directory) {
        return Err(Problem(
            "the tests can't wait for a place under a hold without one, because a pending bench would wait for them".to_string(),
        ));
    }
    hold.place = Held::Own(take_place(&directory, places()?, hold.ahead)?);
    Ok(hold)
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
    slot(root, "claim.lock", || {})
}

/// Held for a whole merge, so that only one runs at a time and each is tested
/// on top of the last. `waiting` runs before a wait, if another run holds it.
pub fn merge_slot(root: &Path, waiting: impl FnOnce()) -> Result<Hold, Problem> {
    slot(root, "merge.lock", waiting)
}

/// Held for a whole response to a failing gate, so that two can't share the
/// tree its bisect checks commits out in, or file the same issue.
pub fn response_slot(root: &Path) -> Result<Hold, Problem> {
    slot(root, "nightly.lock", || {})
}

fn slot(root: &Path, name: &str, waiting: impl FnOnce()) -> Result<Hold, Problem> {
    let directory = directory(root)?;
    let file = open(&directory, name)?;
    let couldnt = |e| Problem(format!("couldn't take {name}: {e}"));
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            waiting();
            file.lock().map_err(couldnt)?;
        }
        Err(TryLockError::Error(e)) => return Err(couldnt(e)),
    }
    let mut hold = Hold::new(&directory);
    hold.files.push(file);
    Ok(hold)
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
    let status = child.status().map_err(|e| {
        let launcher = child.get_program().to_string_lossy();
        Problem(format!("couldn't run `{launcher}`: {e}"))
    })?;
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
fn take_shared(directory: &Path) -> Result<File, Problem> {
    let mut waiting = None;
    let turnstile = open(directory, "turnstile.lock")?;
    take(&turnstile, false, Wait::Activity, &mut waiting).map_err(failed)?;
    let activity = open(directory, "activity.lock")?;
    take(&activity, true, Wait::Activity, &mut waiting).map_err(failed)?;
    Ok(activity)
}

fn take_exclusive(directory: &Path) -> Result<Hold, Problem> {
    let mut waiting = None;
    let turnstile = open(directory, "turnstile.lock")?;
    take(&turnstile, false, Wait::Activity, &mut waiting).map_err(failed)?;
    let activity = open(directory, "activity.lock")?;
    take(&activity, false, Wait::Activity, &mut waiting).map_err(failed)?;
    let mut hold = Hold::new(directory);
    hold.activity = Held::Own(activity);
    hold.files.push(turnstile);
    Ok(hold)
}

/// Waiters queue for a place one at a time, behind the merge slot's holder,
/// which waits apart from the queue and keeps this file while it does.
/// `flock` wakes waiters in no set order, so another waiter that gets a place
/// while the holder waits gives it up, and waits until the holder has one.
fn take_place(directory: &Path, count: usize, ahead: bool) -> Result<File, Problem> {
    let couldnt = |e: std::io::Error| Problem(format!("couldn't take a place: {e}"));
    let mut waiting = None;
    let turn = open(directory, if ahead { AHEAD } else { "places.lock" })?;
    take(&turn, false, Wait::Place, &mut waiting).map_err(couldnt)?;
    loop {
        let place = free_place(directory, count, &mut waiting)?;
        if ahead || !waits_ahead(directory)? {
            return Ok(place);
        }
        drop(place);
        let behind = open(directory, AHEAD)?;
        take(&behind, true, Wait::Place, &mut waiting).map_err(couldnt)?;
    }
}

/// The first place that is free, or that frees. `flock` can't wait for any one
/// of several files, so this waits on each place in a thread of its own, and
/// keeps the first it gets. A thread that gets one later finds nobody to hand
/// it to, and frees it at once.
fn free_place(
    directory: &Path,
    count: usize,
    waiting: &mut Option<Waiting>,
) -> Result<File, Problem> {
    let couldnt = |e: std::io::Error| Problem(format!("couldn't take a place: {e}"));
    let names: Vec<String> = (0..count).map(|i| format!("place-{i}.lock")).collect();
    for name in &names {
        let file = open(directory, name)?;
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(e)) => return Err(couldnt(e)),
        }
    }
    Waiting::start(waiting, Wait::Place);
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

/// Whether the merge slot's holder waits for a place.
fn waits_ahead(directory: &Path) -> Result<bool, Problem> {
    match open(directory, AHEAD)?.try_lock_shared() {
        Ok(()) => Ok(false),
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(e)) => Err(Problem(format!("couldn't read {AHEAD}: {e}"))),
    }
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
    use super::{bench_waits, open, take_exclusive, take_place, take_shared};
    use std::fs::{self, TryLockError};
    use std::os::unix::fs::MetadataExt;
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
        wait_for(path, "turnstile.lock");
    }

    /// Returns once something holds the lock on `name`.
    fn wait_for(path: &Path, name: &str) {
        let probe = open(path, name).map_err(|e| e.0).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match probe.try_lock() {
                Err(TryLockError::WouldBlock) => return,
                Ok(()) => probe.unlock().unwrap(),
                Err(error) => panic!("{error}"),
            }
            assert!(Instant::now() < deadline, "nothing took {name}");
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

    /// Returns once something waits to share the lock on `name`, which
    /// `/proc/locks` lists as a blocked request on the file's inode.
    fn wait_for_a_sharer(path: &Path, name: &str) {
        let inode = fs::metadata(path.join(name)).unwrap().ino().to_string();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let locks = fs::read_to_string("/proc/locks").unwrap();
            let waits = locks.lines().any(|line| {
                let words: Vec<&str> = line.split_whitespace().collect();
                words.get(1) == Some(&"->")
                    && words.contains(&"READ")
                    && words
                        .iter()
                        .any(|word| word.rsplit(':').next() == Some(&inode) && word.contains(':'))
            });
            if waits {
                return;
            }
            assert!(Instant::now() < deadline, "nothing waited to share {name}");
            thread::yield_now();
        }
    }

    /// The test keeps `ahead.lock`, as the merge slot's holder does while it
    /// waits for a place. The one place frees while a run waits for it in the
    /// queue, and that run gets it, gives it up, and waits for the holder.
    #[test]
    fn a_run_in_the_queue_lets_the_merge_slots_holder_have_the_next_place() {
        let path = directory("ahead");
        let held = take_place(&path, 1, false).map_err(|e| e.0).unwrap();
        let p = path.clone();
        let queued = holder(move || take_place(&p, 1, false).map_err(|e| e.0).unwrap());
        wait_for(&path, "places.lock");
        let ahead = open(&path, "ahead.lock").map_err(|e| e.0).unwrap();
        ahead.lock().unwrap();

        drop(held);
        wait_for_a_sharer(&path, "ahead.lock");
        let next = open(&path, "place-0.lock").map_err(|e| e.0).unwrap();
        assert!(next.try_lock().is_ok(), "the queue kept the place");
        assert!(queued.try_recv().is_err(), "both have the one place");

        drop((ahead, next));
        queued.recv().unwrap().send(()).unwrap();
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
