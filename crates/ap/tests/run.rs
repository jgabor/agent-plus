//! What `ap run` refuses before its VM starts. Nothing here boots, because
//! the store holds only a stand-in for the `OS minimal` layer, so a run that
//! got past its checks fails when it reads that layer. The boots themselves
//! are in the package `t1`.

mod common;

use std::fs::{self, File};
use std::io::Read;
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;

use guest::linux::{open_terminal, take_terminal};
use guest::protocol::Size;

/// The status `ap run` exits with when it fails before the harness starts.
const FAILED: i32 = 125;

/// What a run that got past its checks says, when it reads the stand-in layer.
const PAST_CHECKS: &str = "can't read the layer for OS minimal";

/// An agent that `ap run` can start, apart from the boot.
const RUNNABLE: &str = "AGENT claude:0.0.0\nOS minimal\nNETWORK -\n";

/// A test's own home, with a store that agents get built into.
struct Scratch {
    root: PathBuf,
    home: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("run")
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

    /// Builds the agent `name` from this Agentfile, approving what it asks.
    fn build(&self, name: &str, agentfile: &str) {
        let dir = self.root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("Agentfile"), agentfile).unwrap();
        common::pin_harness(&self.home, &dir);
        let instructions = agentfile::parse(agentfile).unwrap();
        let grants = ap::approval::grants(&instructions, |text| {
            ap::paths::HostPath::resolve(text, &dir, &self.home)
        })
        .unwrap();
        let approved = self.root.join("approved-grants");
        fs::write(&approved, grants.to_string()).unwrap();
        let output = self.ap(
            &[
                "build",
                &format!("--approve={}", approved.display()),
                dir.to_str().unwrap(),
            ],
            &[],
        );
        assert_eq!(output.status.code(), Some(0), "{output:?}");
    }

    /// Runs `ap` with only `HOME`, and these variables, in its environment.
    fn ap(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        self.command(args, env).output().unwrap()
    }

    fn command(&self, args: &[&str], env: &[(&str, &str)]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ap"));
        command
            .args(args)
            .current_dir(&self.root)
            .env_clear()
            .env("HOME", &self.home)
            .envs(env.iter().copied());
        command
    }

    /// Runs `ap` as [`Scratch::ap`] does, in a new terminal that is its
    /// controlling one. Returns its status, what the terminal showed, and
    /// whether the terminal's modes were the same afterwards.
    fn ap_in_terminal(&self, args: &[&str]) -> (Option<i32>, String, bool) {
        let size = Size {
            rows: 24,
            columns: 80,
        };
        let (controller, terminal) = open_terminal(size).unwrap();
        let before = modes(controller.as_fd());
        let mut command = self.command(args, &[]);
        command
            .stdin(terminal.try_clone().unwrap())
            .stdout(terminal.try_clone().unwrap())
            .stderr(terminal);
        // SAFETY: `take_terminal` only makes system calls.
        unsafe { command.pre_exec(take_terminal) };
        let mut child = command.spawn().unwrap();
        // The terminal ends when the last of `ap`'s processes lets go of it.
        drop(command);
        let shown = thread::spawn(move || read_until_closed(controller));
        let status = child.wait().unwrap().code();
        let (shown, after) = shown.join().unwrap();
        (status, shown, after == before)
    }
}

/// The local modes of the terminal that `side` is either side of.
fn modes(side: BorrowedFd) -> libc::tcflag_t {
    let mut modes = MaybeUninit::uninit();
    // SAFETY: `tcgetattr` fills the whole struct when it returns 0.
    unsafe {
        assert_eq!(libc::tcgetattr(side.as_raw_fd(), modes.as_mut_ptr()), 0);
        modes.assume_init().c_lflag
    }
}

/// Everything shown on the terminal that `controller` controls, until the
/// last program in it lets go, and its local modes just before that.
fn read_until_closed(mut controller: File) -> (String, libc::tcflag_t) {
    let mut shown = Vec::new();
    let mut buffer = [0; 4096];
    let mut last = modes(controller.as_fd());
    while let Ok(count @ 1..) = controller.read(&mut buffer) {
        shown.extend_from_slice(&buffer[..count]);
        last = modes(controller.as_fd());
    }
    (String::from_utf8_lossy(&shown).into_owned(), last)
}

/// Fails unless `output` is a failure before the harness started, whose error
/// has `says`, and which got nowhere near a VM.
fn refused(output: &Output, says: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(FAILED), "{stderr}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(stderr.contains(says), "{stderr}");
    assert!(!stderr.contains(PAST_CHECKS), "{stderr}");
}

#[test]
fn bad_arguments_print_usage_and_exit_2() {
    let scratch = Scratch::new("usage");
    for args in [
        &["run"][..],
        &["run", "a", "b"],
        &["run", "-p"],
        &["run", "a", "-p", "x", "-p", "y"],
        &["run", "a", "-i", "-i"],
        &["run", "--nope", "a"],
    ] {
        let output = scratch.ap(args, &[]);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("usage"),
            "{args:?}"
        );
    }
}

// spec(Run): Anything else is an agent name.
#[test]
fn an_argument_that_isnt_a_path_names_a_built_agent() {
    let scratch = Scratch::new("name");
    scratch.build("runnable", RUNNABLE);
    let missing = scratch.ap(&["run", "unbuilt", "-p", "hi"], &[]);
    refused(&missing, "no agent is named unbuilt");
    // `runnable` is a directory here too, but a name never reads as one.
    let built = scratch.ap(&["run", "runnable", "-p", "hi"], &[]);
    assert_eq!(built.status.code(), Some(FAILED), "{built:?}");
    let stderr = String::from_utf8_lossy(&built.stderr);
    assert!(stderr.contains(PAST_CHECKS), "{stderr}");

    for path in ["./runnable", "../runnable", "/runnable", "~/runnable"] {
        let output = scratch.ap(&["run", path, "-p", "hi"], &[]);
        refused(
            &output,
            "running an agent from its directory is not built yet",
        );
    }
}

// spec(Run): **Exit status.** If `ap` fails before the harness starts, for example on a missing secret or a refused approval, it exits with status 125.
#[test]
fn a_run_that_fails_before_the_harness_starts_exits_125() {
    let scratch = Scratch::new("before");
    scratch.build("needs", &format!("{RUNNABLE}ENV GREETING \"hi $WHO\"\n"));
    refused(
        &scratch.ap(&["run", "needs", "-p", "hi"], &[]),
        "`ENV GREETING` reads $WHO, which isn't set",
    );
    // With the variable set, the run gets as far as the boot.
    let set = scratch.ap(&["run", "needs", "-p", "hi"], &[("WHO", "you")]);
    assert!(
        String::from_utf8_lossy(&set.stderr).contains(PAST_CHECKS),
        "{set:?}"
    );
}

// spec(Run): A run without a prompt opens the harness's interactive interface in your terminal.
// spec(Run): `ap run -i` forces interactive and submits the prompt as the first message.
// spec(PROMPT): An agent with a `PROMPT` runs headless: `ap run <agent>` gives the prompt to the harness, streams the output, and exits with the harness's status.
// spec(PROMPT): An empty prompt clears `PROMPT` for that run.
#[test]
fn a_run_without_a_prompt_or_with_i_is_interactive_and_needs_a_terminal() {
    let scratch = Scratch::new("interactive");
    scratch.build("open", RUNNABLE);
    scratch.build("prompted", &format!("{RUNNABLE}PROMPT Look around.\n"));
    let interactive: [&[&str]; 5] = [
        &["run", "open"],
        &["run", "open", "-p", ""],
        &["run", "prompted", "-p", ""],
        &["run", "prompted", "-i"],
        &["run", "-i", "open", "-p", "hi"],
    ];
    for args in interactive {
        refused(
            &scratch.ap(args, &[]),
            "an interactive run needs a terminal on standard input",
        );
        // In a terminal, the run gets as far as the boot, and leaves the
        // terminal as it found it.
        let (status, shown, restored) = scratch.ap_in_terminal(args);
        assert_eq!(status, Some(FAILED), "{args:?}: {shown}");
        assert!(shown.contains(PAST_CHECKS), "{args:?}: {shown}");
        assert!(restored, "{args:?} left the terminal changed");
    }
    // A prompt, given or built in, makes a run headless, which needs no
    // terminal.
    for args in [&["run", "open", "-p", "hi"][..], &["run", "prompted"]] {
        let output = scratch.ap(args, &[]);
        assert_eq!(output.status.code(), Some(FAILED), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(PAST_CHECKS), "{args:?}: {stderr}");
    }
}

#[test]
fn a_flag_that_isnt_built_yet_is_named() {
    let scratch = Scratch::new("flags");
    scratch.build("runnable", RUNNABLE);
    for flag in [
        "-t",
        "--share",
        "--network",
        "--resume",
        "--gui",
        "--approve=grants",
    ] {
        let output = scratch.ap(&["run", "runnable", flag, "-p", "hi"], &[]);
        let name = flag.split('=').next().unwrap();
        refused(&output, &format!("`ap run {name}` is not built yet"));
    }
}

#[test]
fn an_agent_with_any_network_but_none_fails_before_its_vm_starts() {
    let scratch = Scratch::new("network");
    for (name, network) in [
        ("open", "NETWORK +\n"),
        ("default", ""),
        ("rule", "NETWORK -\nNETWORK +example.com\n"),
    ] {
        scratch.build(name, &format!("AGENT claude:0.0.0\nOS minimal\n{network}"));
        let output = scratch.ap(&["run", name, "-p", "hi"], &[]);
        refused(&output, "network rules are not built yet");
    }
}

#[test]
fn an_agent_with_an_instruction_that_isnt_built_yet_fails_before_its_vm_starts() {
    let scratch = Scratch::new("instructions");
    fs::create_dir_all(scratch.root.join("shared/app")).unwrap();
    scratch.build("shared", &format!("{RUNNABLE}SHARE ./app\n"));
    refused(
        &scratch.ap(&["run", "shared", "-p", "hi"], &[]),
        "`SHARE` is not built yet",
    );
    scratch.build("signed", &format!("{RUNNABLE}AUTH login\n"));
    refused(
        &scratch.ap(&["run", "signed", "-p", "hi"], &[]),
        "`AUTH` is not built yet",
    );
}
