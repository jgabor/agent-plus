//! `ap init`, from SPEC.md's `ap init` section and the lines that AUTH,
//! NETWORK, and POLICY say it writes.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A test's own directory, with a home directory in it.
struct Scratch {
    root: PathBuf,
    home: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("init")
            .join(name);
        let _ = fs::remove_dir_all(&root);
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        Scratch {
            root: root.canonicalize().unwrap(),
            home: home.canonicalize().unwrap(),
        }
    }

    fn ap(&self, dir: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ap"))
            .args(args)
            .current_dir(dir)
            .env_clear()
            .env("HOME", &self.home)
            .output()
            .unwrap()
    }

    /// Runs `ap init` with `args` in the root, fails the test unless it
    /// exits 0, and returns the Agentfile it wrote into `dir`.
    fn init(&self, args: &[&str], dir: &Path) -> String {
        let mut all = vec!["init"];
        all.extend_from_slice(args);
        let output = self.ap(&self.root, &all);
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        fs::read_to_string(dir.join("Agentfile")).unwrap()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The instruction lines of an Agentfile, with comments and blank lines
/// left out.
fn instructions(agentfile: &str) -> Vec<&str> {
    agentfile
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect()
}

// spec(ap init): `ap init [dir]` writes an Agentfile into `dir`.
#[test]
fn init_writes_an_agentfile_into_dir() {
    let scratch = Scratch::new("into-dir");
    let dir = scratch.root.join("project");
    fs::create_dir(&dir).unwrap();
    let agentfile = scratch.init(&["project"], &dir);
    assert_eq!(instructions(&agentfile)[0], "AGENT claude:latest");
    let names: Vec<_> = fs::read_dir(&dir).unwrap().collect();
    assert_eq!(names.len(), 1);
}

#[test]
fn init_creates_a_missing_dir() {
    let scratch = Scratch::new("missing-dir");
    let dir = scratch.root.join("a").join("b");
    scratch.init(&["a/b"], &dir);
}

#[test]
fn init_defaults_to_the_current_directory() {
    let scratch = Scratch::new("current");
    scratch.init(&[], &scratch.root);
}

// spec(AUTH): `ap init` writes `AUTH login` into new Agentfiles.
#[test]
fn init_writes_auth_login() {
    let scratch = Scratch::new("auth");
    let agentfile = scratch.init(&[], &scratch.root);
    assert!(instructions(&agentfile).contains(&"AUTH login"));
}

// spec(NETWORK): `ap init` writes `NETWORK -` into new Agentfiles, so the agents you create start closed.
#[test]
fn init_writes_network_closed() {
    let scratch = Scratch::new("network");
    let agentfile = scratch.init(&[], &scratch.root);
    let network: Vec<_> = instructions(&agentfile)
        .into_iter()
        .filter(|line| line.starts_with("NETWORK"))
        .collect();
    assert_eq!(network, ["NETWORK -"]);
}

// spec(POLICY): `ap init` writes `POLICY -` into new Agentfiles, so the agents you create start closed.
#[test]
fn init_writes_policy_closed() {
    let scratch = Scratch::new("policy");
    let agentfile = scratch.init(&[], &scratch.root);
    let policy: Vec<_> = instructions(&agentfile)
        .into_iter()
        .filter(|line| line.starts_with("POLICY"))
        .collect();
    assert_eq!(policy, ["POLICY -"]);
}

/// The file is formatted, and builds with no errors or warnings.
#[test]
fn init_writes_a_formatted_agentfile_that_builds() {
    let scratch = Scratch::new("valid");
    let dir = scratch.root.join("project");
    scratch.init(&["project"], &dir);
    let output = scratch.ap(&dir, &["fmt", "--check"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    // Approval has tests of its own, so the build approves what it asks for.
    let source = fs::read_to_string(dir.join("Agentfile")).unwrap();
    let instructions = agentfile::parse(&source).unwrap();
    let context = dir.canonicalize().unwrap();
    let grants = ap::approval::grants(&instructions, |text| {
        ap::paths::HostPath::resolve(text, &context, &scratch.home)
    })
    .unwrap();
    let file = scratch.root.join("approved-grants");
    fs::write(&file, grants.to_string()).unwrap();
    let approve = format!("--approve={}", file.display());
    let output = scratch.ap(&dir, &["build", "--strict", &approve]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
}

#[test]
fn init_never_overwrites_an_agentfile() {
    let scratch = Scratch::new("existing");
    let dir = scratch.root.join("project");
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("Agentfile"), "AGENT codex\n").unwrap();
    let output = scratch.ap(&scratch.root, &["init", "project"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "ap: project/Agentfile already exists\n");
    assert_eq!(
        fs::read_to_string(dir.join("Agentfile")).unwrap(),
        "AGENT codex\n"
    );
}

#[test]
fn init_never_writes_through_a_symlink() {
    let scratch = Scratch::new("symlink");
    let target = scratch.root.join("target");
    std::os::unix::fs::symlink(&target, scratch.root.join("Agentfile")).unwrap();
    let output = scratch.ap(&scratch.root, &["init"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "ap: Agentfile already exists\n");
    assert!(!target.exists());
}

#[test]
fn init_refuses_a_dir_that_is_a_file() {
    let scratch = Scratch::new("file");
    fs::write(scratch.root.join("project"), "").unwrap();
    let output = scratch.ap(&scratch.root, &["init", "project"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "ap: project isn't a directory\n");
}

#[test]
fn init_with_bad_arguments_prints_usage() {
    let scratch = Scratch::new("usage");
    for args in [&["init", "a", "b"][..], &["init", "--force"]] {
        let output = scratch.ap(&scratch.root, args);
        assert_eq!(output.status.code(), Some(2));
        assert!(
            stderr(&output).contains("ap init [dir]"),
            "{}",
            stderr(&output)
        );
    }
    assert!(!scratch.root.join("a").exists());
    assert!(!scratch.root.join("Agentfile").exists());
}
