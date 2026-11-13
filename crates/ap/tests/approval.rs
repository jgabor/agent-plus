//! `ap build` asks you to approve an Agentfile's grants, by "How approval
//! works" under SPEC.md's Grants section: on a terminal, without one, and
//! with `--approve=<file>`.

mod common;

use std::ffi::CStr;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;
use std::thread;

use store::{Agents, Approvals};

/// A test's own directory, with a home directory in it.
struct Scratch {
    root: PathBuf,
    home: PathBuf,
}

/// What a run of `ap` on a terminal did.
struct Run {
    code: Option<i32>,
    /// Everything it wrote to the terminal.
    screen: String,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("approval")
            .join(name);
        let _ = fs::remove_dir_all(&root);
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        common::seed_os(&home.join(".local/share/ap"));
        Scratch {
            root: root.canonicalize().unwrap(),
            home: home.canonicalize().unwrap(),
        }
    }

    /// Makes the directory `name` with an Agentfile and a file to add, and
    /// returns its path.
    fn project(&self, name: &str, agentfile: &str) -> PathBuf {
        let dir = self.root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("Agentfile"), agentfile).unwrap();
        common::pin_harness(&self.home, &dir);
        fs::write(dir.join("data"), "one").unwrap();
        dir
    }

    fn command(&self, dir: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ap"));
        command
            .args(args)
            .current_dir(dir)
            .env_clear()
            .env("HOME", &self.home);
        command
    }

    /// Runs `ap` in `dir` with no terminal: standard input is empty.
    fn ap(&self, dir: &Path, args: &[&str]) -> Output {
        self.command(dir, args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// Runs `ap` in `dir` on a new terminal, after typing `typed` into it.
    fn on_terminal(&self, dir: &Path, args: &[&str], typed: &str) -> Run {
        let (mut master, slave) = terminal();
        let mut command = self.command(dir, args);
        command
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave);
        master.write_all(typed.as_bytes()).unwrap();
        let mut child = command.spawn().unwrap();
        // The child holds the only copies of the terminal's other end now, so
        // reading ends when it exits.
        drop(command);
        let reader = thread::spawn(move || {
            let mut screen = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                match master.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => screen.extend_from_slice(&buffer[..n]),
                    // Linux reports the other end closing as an error.
                    Err(_) => break,
                }
            }
            String::from_utf8_lossy(&screen).replace("\r\n", "\n")
        });
        let status = child.wait().unwrap();
        Run {
            code: status.code(),
            screen: reader.join().unwrap(),
        }
    }

    fn store(&self) -> PathBuf {
        self.home.join(".local/share/ap")
    }

    fn built(&self, name: &str) -> bool {
        Agents::new(self.store()).get(name).unwrap().is_some()
    }

    /// The grants approved for the Agentfile in `dir`, as their text form.
    fn approved(&self, dir: &Path) -> Option<String> {
        let agentfile = dir.join("Agentfile").canonicalize().unwrap();
        let approvals = Approvals::new(self.store());
        approvals
            .get(&agentfile)
            .unwrap()
            .map(|grants| grants.to_string())
    }
}

/// Opens a new pseudo-terminal, and returns both of its ends.
fn terminal() -> (File, File) {
    // `ptsname` returns a static buffer on some hosts, so one test at a time
    // calls it.
    static PTSNAME: Mutex<()> = Mutex::new(());
    // SAFETY: each call is checked, and the descriptor is owned by the
    // returned `File` alone. The name is copied out under the lock.
    unsafe {
        let fd = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(fd >= 0, "posix_openpt failed");
        let master = File::from_raw_fd(fd);
        assert_eq!(libc::grantpt(fd), 0, "grantpt failed");
        assert_eq!(libc::unlockpt(fd), 0, "unlockpt failed");
        let name = {
            let _lock = PTSNAME.lock().unwrap();
            let name = libc::ptsname(fd);
            assert!(!name.is_null(), "ptsname failed");
            CStr::from_ptr(name).to_str().unwrap().to_string()
        };
        let slave = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(name)
            .unwrap();
        (master, slave)
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

const NOTES: &str = "AGENT claude\nOS minimal\nADD ./data\nNETWORK -\nNETWORK +github.com\n";

// spec(Grants): On the first `ap build` of an Agentfile, `ap` lists its grants and asks you to approve them.
#[test]
fn yes_on_a_terminal_builds_and_keeps_the_approval() {
    let scratch = Scratch::new("yes");
    let dir = scratch.project("notes", NOTES);
    let run = scratch.on_terminal(&dir, &["build"], "y\n");
    assert_eq!(run.code, Some(0), "{}", run.screen);
    assert!(
        run.screen
            .contains("asks for these grants:\n  NETWORK -\n  NETWORK +github.com\n  POLICY +\n"),
        "{}",
        run.screen
    );
    assert!(
        run.screen.contains("Approve them? [y/N] "),
        "{}",
        run.screen
    );
    assert!(run.screen.contains("agent notes"), "{}", run.screen);
    assert!(scratch.built("notes"));
    assert_eq!(
        scratch.approved(&dir).as_deref(),
        Some("NETWORK -\nNETWORK +github.com\nPOLICY +\n")
    );
}

// spec(Grants): On the first `ap build` of an Agentfile, `ap` lists its grants and asks you to approve them.
#[test]
fn no_on_a_terminal_builds_nothing() {
    let scratch = Scratch::new("no");
    let dir = scratch.project("notes", NOTES);
    for typed in ["n\n", "\n", "maybe\n", "\u{4}"] {
        let run = scratch.on_terminal(&dir, &["build"], typed);
        assert_eq!(run.code, Some(1), "{typed:?}: {}", run.screen);
        assert!(
            run.screen.contains("Approve them? [y/N] "),
            "{}",
            run.screen
        );
        assert!(run.screen.contains("nothing was built"), "{}", run.screen);
        assert!(!run.screen.contains("built ADD"), "{}", run.screen);
    }
    assert!(!scratch.built("notes"));
    // The store holds only the OS layer it started with.
    let layers = fs::read_dir(scratch.store().join("zstd-layers")).unwrap();
    assert_eq!(layers.count(), 1);
    assert_eq!(scratch.approved(&dir), None);
}

// spec(Grants): A build that only narrows grants does not ask, and neither does a change that touches no grant, such as a new `PLUGIN`.
#[test]
fn a_later_build_with_the_same_grants_does_not_ask() {
    let scratch = Scratch::new("same");
    let dir = scratch.project("notes", NOTES);
    assert_eq!(scratch.on_terminal(&dir, &["build"], "y\n").code, Some(0));
    // An answer is typed, so a prompt would get `n` and fail the build.
    let run = scratch.on_terminal(&dir, &["build"], "n\n");
    assert_eq!(run.code, Some(0), "{}", run.screen);
    assert!(!run.screen.contains("Approve"), "{}", run.screen);
    fs::write(
        dir.join("Agentfile"),
        "AGENT claude\nOS minimal\nADD ./data\nMODEL opus\nNETWORK -\n",
    )
    .unwrap();
    let run = scratch.on_terminal(&dir, &["build"], "n\n");
    assert_eq!(run.code, Some(0), "{}", run.screen);
    assert!(!run.screen.contains("Approve"), "{}", run.screen);
}

// spec(Grants): A later build that **widens** a grant asks again and shows what changed.
// spec(Grants): It doesn't matter who edited the Agentfile, so a wider grant that arrives through `git pull` or `ap apply` is caught at your next build.
#[test]
fn a_wider_grant_asks_again_and_shows_only_the_change() {
    let scratch = Scratch::new("wider");
    let dir = scratch.project("notes", NOTES);
    assert_eq!(scratch.on_terminal(&dir, &["build"], "y\n").code, Some(0));
    fs::write(
        dir.join("Agentfile"),
        format!("{NOTES}NETWORK +crates.io\n"),
    )
    .unwrap();
    let run = scratch.on_terminal(&dir, &["build"], "n\n");
    assert_eq!(run.code, Some(1), "{}", run.screen);
    assert!(
        run.screen.contains(
            "asks for more than you approved:\n  NETWORK +crates.io\nApprove them? [y/N] "
        ),
        "{}",
        run.screen
    );
    let run = scratch.on_terminal(&dir, &["build"], "yes\n");
    assert_eq!(run.code, Some(0), "{}", run.screen);
    assert!(
        scratch
            .approved(&dir)
            .unwrap()
            .contains("NETWORK +crates.io\n")
    );
}

// spec(Grants): The approval is stored with the Agentfile's canonical path, not with the agent's name, so another directory never inherits it.
#[test]
fn the_same_agentfile_in_another_directory_asks_again() {
    let scratch = Scratch::new("another");
    let dir = scratch.project("notes", NOTES);
    assert_eq!(scratch.on_terminal(&dir, &["build"], "y\n").code, Some(0));
    let copy = scratch.project("copy", NOTES);
    let run = scratch.on_terminal(&copy, &["build", "-t", "notes", "--replace"], "n\n");
    assert_eq!(run.code, Some(1), "{}", run.screen);
    assert!(
        run.screen.contains("asks for these grants:"),
        "{}",
        run.screen
    );
    // A link to the same directory reaches the same Agentfile.
    std::os::unix::fs::symlink(&dir, scratch.root.join("link")).unwrap();
    let link = scratch.root.join("link");
    let run = scratch.on_terminal(&link, &["build"], "n\n");
    assert_eq!(run.code, Some(0), "{}", run.screen);
}

// spec(Grants): Without a terminal, a build or run that needs approval fails.
#[test]
fn without_a_terminal_a_first_build_fails_and_lists_the_grants() {
    let scratch = Scratch::new("no-terminal");
    let dir = scratch.project("notes", NOTES);
    let output = scratch.ap(&dir, &["build"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert_eq!(
        stderr(&output),
        "ap: Agentfile asks for these grants:\n  NETWORK -\n  NETWORK +github.com\n  POLICY +\n\
         ap: approve them on a terminal, or with `--approve=<file>`\n"
    );
    assert!(!scratch.built("notes"));
    assert_eq!(scratch.approved(&dir), None);
}

// spec(Grants): `--approve=<file>`, on `ap build` and `ap run`, approves exactly the grants listed in `<file>`, in the format that `ap inspect --grants` prints, and fails if the Agentfile asks for more.
#[test]
fn approve_with_a_file_builds_or_lists_what_is_more() {
    let scratch = Scratch::new("file");
    let dir = scratch.project("notes", NOTES);
    let file = scratch.root.join("grants");
    let approve = format!("--approve={}", file.display());
    fs::write(&file, "NETWORK -\nPOLICY +\n").unwrap();
    let output = scratch.ap(&dir, &["build", &approve]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        format!(
            "ap: Agentfile asks for more than {} approves:\n  NETWORK +github.com\n",
            file.display()
        )
    );
    assert!(!scratch.built("notes"));

    fs::write(&file, "# CI\nNETWORK -\nNETWORK +github.com\nPOLICY +\n").unwrap();
    let output = scratch.ap(&dir, &["build", &approve]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(scratch.built("notes"));
    // The build is approved now, so it needs no file.
    let output = scratch.ap(&dir, &["build"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
}

#[test]
fn a_bad_approve_file_is_an_error() {
    let scratch = Scratch::new("bad-file");
    let dir = scratch.project("notes", NOTES);
    let file = scratch.root.join("grants");
    fs::write(&file, "NETWORK -\nSHARE ./x\nPOLICY +\n").unwrap();
    let output = scratch.ap(&dir, &["build", &format!("--approve={}", file.display())]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        format!("{}:2: unknown grant `SHARE`\n", file.display())
    );
    let output = scratch.ap(&dir, &["build", "--approve=missing"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).starts_with("ap: missing: "),
        "{}",
        stderr(&output)
    );
    for args in [
        &["build", "--approve="][..],
        &["build", "--approve=a", "--approve=b"],
    ] {
        assert_eq!(scratch.ap(&dir, args).status.code(), Some(2), "{args:?}");
    }
}

// spec(Grants): Approval prompts escape control characters and bidirectional overrides in paths, names, and hosts, so a grant shows as it is.
#[test]
fn the_prompt_escapes_what_would_hide() {
    let scratch = Scratch::new("escapes");
    let hidden = scratch.root.join("\u{202e}txt.sh");
    fs::write(&hidden, "two").unwrap();
    scratch.project(
        "n\u{202e}otes",
        "AGENT claude\nOS minimal\nADD \"../\u{202e}txt.sh\" /x\nENV A \"$B\"\nNETWORK -\n",
    );
    let args = ["build", "-t", "notes", "n\u{202e}otes"];
    let run = scratch.on_terminal(&scratch.root, &args, "n\n");
    assert_eq!(run.code, Some(1), "{}", run.screen);
    let shown = format!("  PATH {}/\\u{{202e}}txt.sh\n", scratch.root.display());
    assert!(run.screen.contains(&shown), "{}", run.screen);
    assert!(
        run.screen
            .contains("n\\u{202e}otes/Agentfile asks for these grants:"),
        "{}",
        run.screen
    );
    assert!(!run.screen.contains('\u{202e}'), "{}", run.screen);
}

// spec(Grants): The approval is stored with the Agentfile's canonical path, not with the agent's name, so another directory never inherits it.
#[test]
fn a_linked_agentfile_is_approved_by_where_it_really_is() {
    let scratch = Scratch::new("linked");
    let real = scratch.project("real", NOTES);
    let dir = scratch.project("notes", NOTES);
    fs::remove_file(dir.join("Agentfile")).unwrap();
    std::os::unix::fs::symlink(real.join("Agentfile"), dir.join("Agentfile")).unwrap();
    let run = scratch.on_terminal(&dir, &["build"], "y\n");
    assert_eq!(run.code, Some(0), "{}", run.screen);
    assert!(scratch.approved(&real).is_some());
    let approvals = Approvals::new(scratch.store());
    assert_eq!(approvals.get(&dir.join("Agentfile")).unwrap(), None);
}
