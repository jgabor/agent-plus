//! Runs `xtask run`, and plays the part of a bench by locking the same files.

#[cfg(target_os = "linux")]
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

mod common;

const HOLD_ENV: &str = "XTASK_ACTIVITY_HELD";
const PLACE_ENV: &str = "XTASK_PLACE_HELD";
const PLACES_ENV: &str = "XTASK_PLACES";
const AT_ONCE_ENV: &str = "XTASK_PACKAGES_AT_ONCE";
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
        .env_remove(PLACE_ENV)
        .env_remove(PLACES_ENV)
        .env_remove(AT_ONCE_ENV)
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
    start_holder_in(root, 2)
}

/// A holder in a host with this many places.
fn start_holder_in(root: &Path, places: usize) -> Holder {
    let mut child = xtask(root, &["run", "sh", "-c", "echo held; read line"])
        .env(PLACES_ENV, places.to_string())
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

/// macOS has nothing that limits the memory of a whole process tree, so the
/// command runs without a limit, and the run says so.
#[cfg(target_os = "macos")]
#[test]
fn on_macos_a_run_says_it_has_no_memory_limit() {
    let root = workspace("memory-macos");
    let output = run(&root, &["sh", "-c", "echo ran"]);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "ran\n");
    assert_eq!(output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("without a memory limit"),
        "{output:?}"
    );
}

#[cfg(target_os = "linux")]
/// Allocates about 256 MiB and stops, so that without a limit it says so and
/// exits 0 instead of taking the machine with it.
const ALLOCATE: &str =
    "x=aaaaaaaa; i=0; while [ $i -lt 25 ]; do x=$x$x; i=$((i+1)); done; echo survived";

#[cfg(target_os = "linux")]
#[test]
fn a_command_over_the_memory_limit_is_killed_and_xtask_fails() {
    let root = workspace("memory");
    let output = xtask(&root, &["run", "sh", "-c", ALLOCATE])
        .env("XTASK_MEMORY_MAX", "64M")
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    assert_eq!(output.status.code(), Some(1));
}

#[cfg(target_os = "linux")]
#[test]
fn a_command_under_the_memory_limit_is_left_alone() {
    let root = workspace("memory-roomy");
    let output = xtask(&root, &["run", "sh", "-c", ALLOCATE])
        .env("XTASK_MEMORY_MAX", "2G")
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout), "survived\n");
    assert_eq!(output.status.code(), Some(0));
}

#[cfg(target_os = "linux")]
#[test]
fn an_empty_memory_limit_is_a_problem() {
    let root = workspace("memory-empty");
    let output = xtask(&root, &["run", "sh", "-c", "echo ran"])
        .env("XTASK_MEMORY_MAX", "")
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("XTASK_MEMORY_MAX"));
}

#[cfg(target_os = "linux")]
#[test]
fn without_a_user_session_nothing_runs() {
    let root = workspace("no-session");
    let output = xtask(&root, &["run", "sh", "-c", "echo ran"])
        .env("XDG_RUNTIME_DIR", root.join("no-such-directory"))
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    assert_eq!(output.status.code(), Some(1));
}

/// A slice of the user manager for one test, stopped when the test ends. A
/// slice outlives the scopes in it, and a dash in its name would nest it.
#[cfg(target_os = "linux")]
struct Slice(String);

#[cfg(target_os = "linux")]
impl Slice {
    fn new(test: &str) -> Self {
        Slice(format!("xtask_{test}_{}.slice", std::process::id()))
    }
}

#[cfg(target_os = "linux")]
impl Drop for Slice {
    fn drop(&mut self) {
        let _ = Command::new("systemctl")
            .args(["--user", "stop", &self.0])
            .status();
    }
}

/// Prints the cgroup of the job, and then of `xtask`, which started it.
#[cfg(target_os = "linux")]
const CGROUPS: &str = "cut -d: -f3 /proc/self/cgroup; cut -d: -f3 /proc/$PPID/cgroup";

/// Limits set on the caller's slice must reach the job, so it can't land in
/// the user manager's `app.slice` instead.
#[cfg(target_os = "linux")]
#[test]
fn a_job_stays_in_the_slice_of_its_caller() {
    let root = workspace("slice");
    let slice = Slice::new("lock");
    let output = Command::new("systemd-run")
        .args(["--user", "--scope", "--quiet", "--collect"])
        .arg(format!("--slice={}", slice.0))
        .arg("--")
        .arg(env!("CARGO_BIN_EXE_xtask"))
        .args(["run", "sh", "-c", CGROUPS])
        .current_dir(&root)
        .env_remove(HOLD_ENV)
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{stdout}");
    let parents: Vec<&str> = stdout
        .lines()
        .map(|cgroup| cgroup.rsplit_once('/').unwrap().0)
        .collect();
    assert_eq!(parents.len(), 2, "{stdout}");
    assert!(parents[1].ends_with(&format!("/{}", slice.0)), "{stdout}");
    assert_eq!(parents[0], parents[1], "{stdout}");
}

#[test]
fn a_program_that_is_not_executable_is_a_problem() {
    let root = workspace("not-executable");
    fs::write(root.join("plain"), "").unwrap();
    let output = run(&root, &["./plain"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("./plain"));
}

#[test]
fn a_script_whose_interpreter_is_missing_is_a_problem() {
    let root = workspace("missing-interpreter");
    let script = root.join("script");
    fs::write(&script, "#!/no/such/interpreter\necho ran\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let output = run(&root, &["./script"]);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("/no/such/interpreter"));
}

/// The kernel ends the interpreter's name at a NUL, takes it as bytes, and
/// runs a script whose name is cut off at 256 bytes with `sh` instead. The
/// kernel on macOS starts none of these, and its file systems refuse a name
/// that isn't UTF-8.
#[cfg(target_os = "linux")]
#[test]
fn a_script_the_kernel_can_start_is_left_alone() {
    let root = workspace("odd-interpreters");
    let odd = root.join(OsStr::from_bytes(b"true-\xff"));
    std::os::unix::fs::symlink("/bin/true", &odd).unwrap();
    let long = format!("#!/{}\nexit 0\n", "x".repeat(300));
    let cases: [(&str, Vec<u8>); 3] = [
        ("nul", b"#!/bin/true\0ignored\n".to_vec()),
        ("bytes", [b"#!", odd.as_os_str().as_bytes(), b"\n"].concat()),
        ("long", long.into_bytes()),
    ];
    for (name, body) in cases {
        let script = root.join(name);
        fs::write(&script, body).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let output = run(&root, &[&format!("./{name}")]);
        assert_eq!(output.status.code(), Some(0), "{name}: {output:?}");
    }
}

#[test]
fn run_passes_stdin_through() {
    let root = workspace("stdin");
    let mut child = xtask(&root, &["run", "cat"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"hello\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout), "hello\n");
    assert_eq!(output.status.code(), Some(0));
}

/// `xtask run true` in a host with this many places.
fn waiter(root: &Path, places: usize) -> Child {
    xtask(root, &["run", "true"])
        .env(PLACES_ENV, places.to_string())
        .spawn()
        .unwrap()
}

#[test]
fn a_run_past_the_places_waits_for_one_to_free() {
    let root = workspace("places-full");
    let first = start_holder_in(&root, 2);
    let second = start_holder_in(&root, 2);
    assert_blocked_until_released(waiter(&root, 2), || second.finish());
    first.finish();
}

#[test]
fn a_killed_holder_frees_its_place() {
    let root = workspace("place-killed");
    let mut holder = start_holder_in(&root, 1);
    assert_blocked_until_released(waiter(&root, 1), || {
        holder.child.kill().unwrap();
        holder.child.wait().unwrap();
    });
}

#[test]
fn a_nested_xtask_shares_its_parents_place() {
    let root = workspace("place-nested");
    let script = format!("exec timeout 20 {} run true", env!("CARGO_BIN_EXE_xtask"));
    let output = xtask(&root, &["run", "sh", "-c", &script])
        .env(PLACES_ENV, "1")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
}

/// Takes the lock within a while, or says that it couldn't.
fn takes_within(file: &File, limit: Duration) -> bool {
    let deadline = std::time::Instant::now() + limit;
    while std::time::Instant::now() < deadline {
        if file.try_lock().is_ok() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

/// One waiting for a place holds nothing a bench needs, so the bench goes
/// first once the holders are done, and the waiter goes after the bench.
#[test]
fn a_bench_does_not_wait_for_a_run_waiting_for_a_place() {
    let root = workspace("place-bench");
    let holder = start_holder_in(&root, 1);
    let mut later = waiter(&root, 1);
    thread::sleep(WAIT);
    let pending = turnstile(&root);
    assert!(takes_within(&pending, Duration::from_secs(30)));
    holder.finish();
    let bench = activity(&root);
    assert!(
        takes_within(&bench, Duration::from_secs(30)),
        "the bench waited for a run that waits for a place"
    );
    thread::sleep(WAIT);
    assert!(
        later.try_wait().unwrap().is_none(),
        "it ran during the bench"
    );
    drop((bench, pending));
    assert!(later.wait().unwrap().success());
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

#[test]
fn a_hold_on_another_workspace_does_not_count() {
    let root = workspace("other-hold");
    let bench = activity(&root);
    bench.lock().unwrap();
    let child = xtask(&root, &["run", "true"])
        .env(HOLD_ENV, "/some/other/lock/directory")
        .spawn()
        .unwrap();
    assert_blocked_until_released(child, || bench.unlock().unwrap());
}

#[test]
fn tests_run_by_score_inherit_its_hold() {
    let root = workspace("score-inherits");
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
    fs::write(
        root.join("demo/src/lib.rs"),
        "// spec(A): One.\n#[test]\nfn one() {\n    assert!(std::env::var_os(\"XTASK_ACTIVITY_HELD\").is_some());\n}\n",
    )
    .unwrap();
    let lockfile = Command::new("cargo")
        .arg("generate-lockfile")
        .current_dir(&root)
        .status()
        .unwrap();
    assert!(lockfile.success());
    let output = xtask(&root, &["score"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout), "spec 1 of 1\n");
}

/// Takes the lock on another thread, and gives it back once it holds it, or
/// `None` at the limit.
fn take(file: File) -> Option<File> {
    let (taken, held) = std::sync::mpsc::channel();
    thread::spawn(move || {
        file.lock().unwrap();
        let _ = taken.send(file);
    });
    held.recv_timeout(common::LIMIT).ok()
}

/// A workspace of packages, each a name and the `lib.rs` it holds.
fn packages(name: &str, crates: &[(&str, &str)]) -> PathBuf {
    let root = workspace(name);
    common::crates(&root, crates);
    root
}

/// A workspace of two packages, whose tests run a package at a time. The tests
/// of whichever runs first say so at the FIFO `ready`, and then wait at `go`.
/// The second package's tests write `second`.
fn two_packages(name: &str) -> PathBuf {
    let marks = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("lock")
        .join(name);
    let test = format!(
        "#[test]\nfn takes_turns() {{\n    let marks = std::path::Path::new({:?});\n    if std::fs::File::create_new(marks.join(\"first\")).is_ok() {{\n        std::fs::write(marks.join(\"ready\"), \"\").unwrap();\n        std::fs::read(marks.join(\"go\")).unwrap();\n    }} else {{\n        std::fs::write(marks.join(\"second\"), \"\").unwrap();\n    }}\n}}\n",
        marks.display().to_string()
    );
    let root = packages(name, &[("a", &test), ("b", &test)]);
    for fifo in ["ready", "go"] {
        common::fifo(&root.join(fifo));
    }
    root
}

/// `xtask score`, which runs each package's tests, in a host with this many
/// places, a package at a time.
fn score(root: &Path, places: usize) -> Child {
    xtask(root, &["score"])
        .env(PLACES_ENV, places.to_string())
        .env(AT_ONCE_ENV, "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

/// Stops a run that kept what the test waited for, so the test can fail.
fn kept(mut run: Child, what: &str) -> ! {
    let _ = run.kill();
    let _ = run.wait();
    panic!("{what}");
}

#[test]
fn a_test_run_lets_a_waiting_bench_go_between_packages() {
    let root = two_packages("bench-between");
    let run = score(&root, 2);
    common::meet(&root.join("ready"), false);
    let pending = turnstile(&root);
    pending.lock().unwrap();
    common::meet(&root.join("go"), true);
    let Some(bench) = take(activity(&root)) else {
        kept(run, "the run kept the activity lock for its next package");
    };
    assert!(
        !root.join("second").exists(),
        "the next package ran before the bench"
    );
    drop((bench, pending));
    let mut run = run;
    assert!(run.wait().unwrap().success());
    assert!(root.join("second").exists());
}

/// The merge slot's holder keeps `ahead.lock` while it waits for a place.
#[test]
fn a_test_run_gives_its_place_to_the_merge_slots_holder_between_packages() {
    let root = two_packages("place-between");
    let run = score(&root, 1);
    common::meet(&root.join("ready"), false);
    let ahead = lock_file(&root, "ahead.lock");
    ahead.lock().unwrap();
    common::meet(&root.join("go"), true);
    let Some(place) = take(lock_file(&root, "place-0.lock")) else {
        kept(run, "the run kept its place for its next package");
    };
    assert!(
        !root.join("second").exists(),
        "the next package ran before the merge slot's holder had the place"
    );
    drop((place, ahead));
    let mut run = run;
    assert!(run.wait().unwrap().success());
    assert!(root.join("second").exists());
}

/// Says at the FIFO `ready-<package>` that its tests run, waits at
/// `go-<package>`, and writes `done-<package>` as it ends.
fn waits_to_end(package: &str) -> String {
    format!(
        "{}\n#[test]\nfn waits_to_end() {{\n    meet(\"ready-{package}\", true);\n    meet(\"go-{package}\", false);\n    let marks = std::path::Path::new(env!(\"CARGO_MANIFEST_DIR\")).join(\"..\");\n    std::fs::write(marks.join(\"done-{package}\"), \"\").unwrap();\n}}\n",
        common::MEET
    )
}

const THIRD: &str = "#[test]\nfn third() {\n    let marks = std::path::Path::new(env!(\"CARGO_MANIFEST_DIR\")).join(\"..\");\n    std::fs::write(marks.join(\"third\"), \"\").unwrap();\n}\n";

/// Two packages run side by side, and a bench comes while they run. Once the
/// first ends, the run still keeps the activity lock while the second runs.
/// The bench has it once both have ended, and before the third package runs.
#[test]
fn a_test_run_side_by_side_lets_a_waiting_bench_go_before_its_next_package() {
    let (a, b) = (waits_to_end("a"), waits_to_end("b"));
    let root = packages("bench-side-by-side", &[("a", &a), ("b", &b), ("c", THIRD)]);
    for fifo in ["ready-a", "go-a", "ready-b", "go-b"] {
        common::fifo(&root.join(fifo));
    }
    let mut run = xtask(&root, &["score"])
        .env(PLACES_ENV, "2")
        .env(AT_ONCE_ENV, "2")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let lines = stderr_lines(&mut run);
    common::meet(&root.join("ready-a"), false);
    common::meet(&root.join("ready-b"), false);
    let pending = turnstile(&root);
    pending.lock().unwrap();
    common::meet(&root.join("go-a"), true);
    // a's tests and doc tests each end in a result line, and b's tests wait.
    for _ in 0..2 {
        loop {
            match lines.recv_timeout(common::LIMIT) {
                Ok(text) if text.starts_with("test result:") => break,
                Ok(_) => {}
                Err(_) => kept(run, "a never ended"),
            }
        }
    }
    if takes_within(&activity(&root), WAIT) {
        kept(run, "the run let go of the activity lock while b still ran");
    }
    common::meet(&root.join("go-b"), true);
    let Some(bench) = take(activity(&root)) else {
        kept(run, "the run kept the activity lock for its next package");
    };
    assert!(root.join("done-b").exists(), "b still ran");
    assert!(
        !root.join("third").exists(),
        "the next package ran before the bench"
    );
    drop((bench, pending));
    assert!(run.wait().unwrap().success());
    assert!(root.join("third").exists());
}

/// A channel that gets each line of `child`'s stderr, and ends as it closes.
fn stderr_lines(child: &mut Child) -> Receiver<String> {
    let stderr = BufReader::new(child.stderr.take().unwrap());
    let (line, lines) = std::sync::mpsc::channel();
    thread::spawn(move || {
        for text in stderr.lines() {
            let _ = line.send(text.unwrap());
        }
    });
    lines
}

/// The status and stderr of `xtask run` with `command`, in a host with one
/// place. A run that waits for the place fails the test instead of hanging it,
/// and the limit only ends a test that already failed.
fn exits_without_waiting(root: &Path, command: &[&str]) -> (Option<i32>, String) {
    let mut run = xtask(root, &[&["run"][..], command].concat())
        .env(PLACES_ENV, "1")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let lines = stderr_lines(&mut run);
    let mut stderr = String::new();
    loop {
        match lines.recv_timeout(common::LIMIT) {
            Ok(text) => stderr += &(text + "\n"),
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => kept(run, "it waited for a place"),
        }
    }
    (run.wait().unwrap().code(), stderr)
}

/// A command that can't run is refused before the run waits for a place, so
/// it ends at once even when every place is held.
#[test]
fn a_bad_command_ends_at_once_while_every_place_is_held() {
    let root = workspace("bad-command-held");
    let holder = start_holder_in(&root, 1);
    for (command, says) in [
        (&[][..], "needs a command"),
        (&["--help"][..], "--help"),
        (&["no-such-program-here"][..], "no-such-program-here"),
    ] {
        let (code, stderr) = exits_without_waiting(&root, command);
        assert_eq!(code, Some(2), "{command:?}: {stderr}");
        assert!(stderr.contains(says), "{command:?}: {stderr}");
        assert!(!stderr.contains("waiting"), "{command:?}: {stderr}");
    }
    holder.finish();
}

/// `xtask run true`, and a channel that gets each line of its stderr.
fn waiter_heard(root: &Path, places: usize) -> (Child, Receiver<String>) {
    let mut child = xtask(root, &["run", "true"])
        .env(PLACES_ENV, places.to_string())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let lines = stderr_lines(&mut child);
    (child, lines)
}

/// Reads stderr up to the line that says the run waits, which the run prints
/// before it waits, so this needs no fixed wait. Then lets the run go on, and
/// returns everything the run printed on stderr. A run that never says it
/// waits is stopped, and the limit only ends a test that already failed.
fn heard_waiting(mut child: Child, lines: Receiver<String>, release: impl FnOnce()) -> Vec<String> {
    let mut heard = Vec::new();
    loop {
        let Ok(line) = lines.recv_timeout(common::LIMIT) else {
            kept(child, &format!("it never said it waits: {heard:?}"));
        };
        let waits = line.contains("waiting");
        heard.push(line);
        if waits {
            break;
        }
    }
    release();
    let status = child.wait().unwrap();
    heard.extend(lines);
    assert!(status.success(), "{heard:?}");
    heard
}

fn count(heard: &[String], text: &str) -> usize {
    heard.iter().filter(|line| line.contains(text)).count()
}

#[test]
fn a_run_that_waits_for_a_place_says_so_once_and_how_long_it_waited() {
    let root = workspace("says-place");
    let holder = start_holder_in(&root, 1);
    let (child, stderr) = waiter_heard(&root, 1);
    let heard = heard_waiting(child, stderr, || holder.finish());
    assert_eq!(count(&heard, "waiting for a place"), 1, "{heard:?}");
    assert_eq!(count(&heard, "waiting"), 1, "{heard:?}");
    assert_eq!(count(&heard, "waited "), 1, "{heard:?}");
}

#[test]
fn a_run_that_waits_for_the_activity_lock_says_so_once_and_how_long_it_waited() {
    let root = workspace("says-activity");
    let bench = activity(&root);
    bench.lock().unwrap();
    let (child, stderr) = waiter_heard(&root, 1);
    let heard = heard_waiting(child, stderr, || bench.unlock().unwrap());
    assert_eq!(
        count(&heard, "waiting for the activity lock"),
        1,
        "{heard:?}"
    );
    assert_eq!(count(&heard, "waiting"), 1, "{heard:?}");
    assert_eq!(count(&heard, "waited "), 1, "{heard:?}");
}

#[test]
fn a_run_that_does_not_wait_says_nothing_of_waits() {
    let root = workspace("says-nothing");
    let output = run(&root, &["true"]);
    assert_eq!(output.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("wait"), "{stderr}");
}
