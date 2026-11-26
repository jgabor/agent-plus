//! What the test binaries share.

use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// How long a test waits for what it expects before it fails. A run that is
/// slow to get there only makes it wait longer.
#[allow(dead_code, reason = "each test binary uses only some of this module")]
pub const LIMIT: Duration = Duration::from_secs(120);

/// Writes an executable that a test can run at once. A child process does the
/// writing. If this process held the file open for writing, a `fork` on another
/// thread could copy that descriptor, and until that child reached `exec`,
/// running the file would fail with `Text file busy`.
#[allow(dead_code, reason = "each test binary uses only some of this module")]
pub fn write_tool(path: &Path, content: &str) {
    let mut child = Command::new("sh")
        .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    input.write_all(content.as_bytes()).unwrap();
    drop(input);
    assert!(child.wait().unwrap().success(), "{}", path.display());
}

/// The caller's toolchain, found where rustup would find it, for a test that
/// gives its command a `HOME` of its own.
#[allow(dead_code, reason = "each test binary uses only some of this module")]
pub fn rustup_home() -> PathBuf {
    std::env::var_os("RUSTUP_HOME").map_or_else(
        || PathBuf::from(std::env::var_os("HOME").expect("HOME is set")).join(".rustup"),
        PathBuf::from,
    )
}

/// The directory of the `cargo` that runs these tests, which holds its `rustc`
/// too. A fixture that finds them here skips rustup's proxy, which would
/// otherwise start before every `cargo` and `rustc` the fixture runs.
#[allow(dead_code, reason = "each test binary uses only some of this module")]
pub fn toolchain() -> PathBuf {
    let cargo = env::var_os("CARGO").expect("cargo runs these tests");
    Path::new(&cargo)
        .parent()
        .expect("cargo is in a directory")
        .to_path_buf()
}

/// A `PATH` that searches `first`, then the toolchain, then the caller's own.
#[allow(dead_code, reason = "each test binary uses only some of this module")]
pub fn path(first: &[&Path]) -> OsString {
    let toolchain = toolchain();
    let rest = env::var_os("PATH").expect("PATH is set");
    let dirs = first
        .iter()
        .map(|dir| dir.to_path_buf())
        .chain([toolchain])
        .chain(env::split_paths(&rest));
    env::join_paths(dirs).expect("no directory on PATH holds a colon")
}

/// Runs `case` on every item at once, each on a thread of its own, and
/// panics after all of them finish if any one did.
#[allow(dead_code, reason = "each test binary uses only some of this module")]
pub fn each<T: Sync>(items: &[T], case: impl Fn(&T) + Sync) {
    thread::scope(|scope| {
        for item in items {
            let case = &case;
            scope.spawn(move || case(item));
        }
    });
}

/// Makes a FIFO, where a test and what it runs can wait for each other.
#[allow(dead_code, reason = "each test binary uses only some of this module")]
pub fn fifo(path: &Path) {
    let made = Command::new("mkfifo").arg(path).status().unwrap();
    assert!(made.success(), "{}", path.display());
}

/// Opens a FIFO, which waits for the other side to open it too, so neither
/// side needs a sleep. A reader reads until the writer closes it. Fails the
/// test if the other side never comes.
#[allow(dead_code, reason = "each test binary uses only some of this module")]
pub fn meet(path: &Path, write: bool) {
    let path = path.to_path_buf();
    let (met, done) = mpsc::channel();
    thread::spawn(move || {
        if write {
            fs::write(&path, "").unwrap();
        } else {
            fs::read(&path).unwrap();
        }
        let _ = met.send(());
    });
    assert!(
        done.recv_timeout(LIMIT).is_ok(),
        "nothing met this test at the FIFO"
    );
}

/// Whether something holds a lock on the file at `path`, shared or alone.
#[allow(dead_code, reason = "each test binary uses only some of this module")]
pub fn locked(path: &Path) -> bool {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .unwrap();
    match file.try_lock() {
        Ok(()) => false,
        Err(TryLockError::WouldBlock) => true,
        Err(TryLockError::Error(e)) => panic!("{}: {e}", path.display()),
    }
}

/// Returns once `ready` is true, or fails the test with `why` at the limit.
#[allow(dead_code, reason = "each test binary uses only some of this module")]
pub fn wait_until(why: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + LIMIT;
    while !ready() {
        assert!(Instant::now() < deadline, "{why}");
        thread::sleep(Duration::from_millis(20));
    }
}
