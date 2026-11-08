//! Runs `xtask run`, and plays the part of a bench by locking the same files.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::thread;
use std::time::Duration;

const HOLD_ENV: &str = "XTASK_ACTIVITY_HELD";
/// Long enough for `xtask` to reach the lock. A slow machine can only make a
/// check that something stays blocked pass, never fail.
const WAIT: Duration = Duration::from_millis(400);

fn workspace(name: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("lock")
        .join(name);
    if root.exists() {
        fs::remove_dir_all(&root).unwrap();
    }
    fs::create_dir_all(root.join("target/xtask")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = []\nresolver = \"3\"\n",
    )
    .unwrap();
    fs::write(root.join("SPEC.md"), "# A\n\nOne.\n").unwrap();
    root
}

fn lock_file(root: &Path, name: &str) -> File {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join("target/xtask").join(name))
        .unwrap()
}

fn activity(root: &Path) -> File {
    lock_file(root, "activity.lock")
}

fn turnstile(root: &Path) -> File {
    lock_file(root, "turnstile.lock")
}

fn xtask(root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_xtask"));
    command
        .args(args)
        .current_dir(root)
        .env_remove(HOLD_ENV)
        .env_remove("CARGO_TARGET_DIR");
    command
}

fn run(root: &Path, args: &[&str]) -> Output {
    let mut all = vec!["run"];
    all.extend_from_slice(args);
    xtask(root, &all).output().unwrap()
}

/// `xtask run` holding the lock, with a child that has said it started.
struct Holder {
    child: Child,
    stdin: ChildStdin,
    _stdout: BufReader<ChildStdout>,
}

fn start_holder(root: &Path) -> Holder {
    let mut child = xtask(root, &["run", "sh", "-c", "echo held; read line"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    assert_eq!(line, "held\n");
    Holder {
        child,
        stdin,
        _stdout: stdout,
    }
}

impl Holder {
    fn finish(mut self) {
        writeln!(self.stdin).unwrap();
        assert!(self.child.wait().unwrap().success());
    }
}

fn assert_blocked_until_released(mut child: Child, release: impl FnOnce()) {
    thread::sleep(WAIT);
    assert!(child.try_wait().unwrap().is_none(), "it didn't wait");
    release();
    assert!(child.wait().unwrap().success());
}

fn is_locked(file: &File) -> bool {
    matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
}

#[test]
fn run_passes_when_the_command_passes() {
    let root = workspace("passes");
    assert_eq!(run(&root, &["true"]).status.code(), Some(0));
}

#[test]
fn run_fails_when_the_command_fails() {
    let root = workspace("fails");
    assert_eq!(run(&root, &["false"]).status.code(), Some(1));
}

#[test]
fn run_passes_arguments_through() {
    let root = workspace("arguments");
    let output = run(&root, &["sh", "-c", "echo \"$1\"", "_", "a b"]);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "a b\n");
}

#[test]
fn run_without_a_command_is_bad_usage() {
    let root = workspace("no-command");
    assert_eq!(run(&root, &[]).status.code(), Some(2));
}

#[test]
fn a_command_that_cannot_start_is_a_problem() {
    let root = workspace("missing");
    let output = run(&root, &["no-such-program-here"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("no-such-program-here"));
}

#[test]
fn a_holder_shares_the_lock() {
    let root = workspace("shared");
    let holder = start_holder(&root);
    let other = start_holder(&root);
    let probe = activity(&root);
    assert!(is_locked(&probe));
    other.finish();
    assert!(is_locked(&probe));
    holder.finish();
    assert!(!is_locked(&probe));
}

#[test]
fn a_killed_holder_releases_the_lock() {
    let root = workspace("killed");
    let mut holder = start_holder(&root);
    let probe = activity(&root);
    assert!(is_locked(&probe));
    holder.child.kill().unwrap();
    holder.child.wait().unwrap();
    assert!(!is_locked(&probe));
}

#[test]
fn a_running_bench_blocks_a_new_holder() {
    let root = workspace("bench-running");
    let bench = activity(&root);
    bench.lock().unwrap();
    let child = xtask(&root, &["run", "true"]).spawn().unwrap();
    assert_blocked_until_released(child, || bench.unlock().unwrap());
}

#[test]
fn a_pending_bench_blocks_a_new_holder() {
    let root = workspace("bench-pending");
    let bench = turnstile(&root);
    bench.lock().unwrap();
    let child = xtask(&root, &["run", "true"]).spawn().unwrap();
    assert_blocked_until_released(child, || bench.unlock().unwrap());
}

#[test]
fn a_pending_bench_does_not_block_a_nested_xtask() {
    let root = workspace("nested");
    let script = format!(
        "echo held; read line; exec timeout 20 {} run true",
        env!("CARGO_BIN_EXE_xtask")
    );
    let mut child = xtask(&root, &["run", "sh", "-c", &script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let bench = turnstile(&root);
    bench.lock().unwrap();
    writeln!(stdin).unwrap();
    assert!(child.wait().unwrap().success());
}

#[test]
fn score_waits_for_a_running_bench() {
    let root = workspace("score-waits");
    let bench = activity(&root);
    bench.lock().unwrap();
    let child = xtask(&root, &["score"])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    assert_blocked_until_released(child, || bench.unlock().unwrap());
}

#[test]
fn gate_waits_for_a_running_bench() {
    let root = workspace("gate-waits");
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"demo\"]\nresolver = \"3\"\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("demo/src")).unwrap();
    fs::write(
        root.join("demo/Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(root.join("demo/src/lib.rs"), "pub fn f() {}\n").unwrap();
    let lockfile = Command::new("cargo")
        .arg("generate-lockfile")
        .current_dir(&root)
        .status()
        .unwrap();
    assert!(lockfile.success());
    let bench = activity(&root);
    bench.lock().unwrap();
    let child = xtask(&root, &["gate"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    assert_blocked_until_released(child, || bench.unlock().unwrap());
}

#[test]
fn a_repository_keeps_the_lock_in_its_shared_git_directory() {
    let root = workspace("in-git");
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(&root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .unwrap();
        assert!(status.success());
    };
    git(&["init", "-q", "-b", "main"]);
    assert_eq!(run(&root, &["true"]).status.code(), Some(0));
    assert!(root.join(".git/xtask/activity.lock").exists());
    assert!(root.join(".git/xtask/turnstile.lock").exists());
}
