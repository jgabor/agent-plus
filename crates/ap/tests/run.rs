//! What `ap run` refuses before its VM starts. Nothing here boots, because a
//! test binary is GNU-linked, so a run that got as far as booting would fail
//! on that instead. The boots themselves are in the package `t1`.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The status `ap run` exits with when it fails before the harness starts.
const FAILED: i32 = 125;

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
        Command::new(env!("CARGO_BIN_EXE_ap"))
            .args(args)
            .current_dir(&self.root)
            .env_clear()
            .env("HOME", &self.home)
            .envs(env.iter().copied())
            .output()
            .unwrap()
    }
}

/// Fails unless `output` is a failure before the harness started, whose error
/// has `says`, and which got nowhere near a VM.
fn refused(output: &Output, says: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(FAILED), "{stderr}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(stderr.contains(says), "{stderr}");
    assert!(!stderr.contains("dynamically linked"), "{stderr}");
}

#[test]
fn bad_arguments_print_usage_and_exit_2() {
    let scratch = Scratch::new("usage");
    for args in [
        &["run"][..],
        &["run", "a", "b"],
        &["run", "-p"],
        &["run", "a", "-p", "x", "-p", "y"],
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
    assert!(stderr.contains("dynamically linked"), "{stderr}");

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
        String::from_utf8_lossy(&set.stderr).contains("dynamically linked"),
        "{set:?}"
    );
    refused(
        &scratch.ap(&["run", "needs", "-p", ""], &[]),
        "an empty prompt opens the interactive interface, which is not built yet",
    );
}

#[test]
fn a_flag_that_isnt_built_yet_is_named() {
    let scratch = Scratch::new("flags");
    scratch.build("runnable", RUNNABLE);
    for flag in [
        "-i",
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
