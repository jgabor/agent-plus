//! `ap secret`, from SPEC.md's `ap secret` section. On Linux, v0.1.0 has no
//! secret store, so each subcommand says so, points to the environment
//! variable that `SECRET` reads first, and exits 1.

#![cfg(target_os = "linux")]

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const VALUE: &str = "s3cr3t-value-that-must-not-land-on-disk";

/// A test's own directory, with a home directory and every XDG directory
/// in it.
struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("secret")
            .join(name);
        let _ = fs::remove_dir_all(&root);
        for dir in ["home", "data", "config", "state", "cache", "runtime"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        Scratch {
            root: root.canonicalize().unwrap(),
        }
    }

    /// Runs `ap` with only the scratch directories in its environment, and
    /// writes `stdin` to its standard input.
    fn ap(&self, args: &[&str], stdin: &str) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_ap"))
            .args(args)
            .current_dir(&self.root)
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        // `ap` may exit before it reads, which closes the pipe.
        match input.write_all(stdin.as_bytes()) {
            Err(error) if error.kind() != io::ErrorKind::BrokenPipe => panic!("{error}"),
            _ => {}
        }
        drop(input);
        child.wait_with_output().unwrap()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Every file under `dir`, at any depth.
fn files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(files(&path));
        } else {
            found.push(path);
        }
    }
    found
}

// spec(ap secret): On Linux, v0.1.0 has no secret store, and `ap secret set`, `ap secret rm`, and `ap secret ls` each say so on standard error, say to give the secret in the environment variable that [SECRET](#secret) reads first, and exit 1.
#[test]
fn each_subcommand_says_linux_has_no_secret_store() {
    let scratch = Scratch::new("no-store");
    for (args, variable) in [
        (&["secret", "set", "GITHUB_TOKEN"][..], "GITHUB_TOKEN"),
        (&["secret", "rm", "STRIPE_API_KEY"][..], "STRIPE_API_KEY"),
        (&["secret", "ls"][..], "named after it"),
    ] {
        let output = scratch.ap(args, VALUE);
        let message = stderr(&output);
        assert_eq!(output.status.code(), Some(1), "{args:?}: {message}");
        assert!(output.stdout.is_empty(), "{args:?}");
        assert!(message.contains("no secret store on Linux"), "{message}");
        assert!(
            message.contains(&format!("environment variable {variable}")),
            "{message}"
        );
        assert!(message.contains("SECRET reads first"), "{message}");
        assert!(!message.contains("usage"), "{message}");
    }
}

// spec(ap secret): Nothing falls back to a file, so `ap` never keeps a secret on disk in the clear.
#[test]
fn set_keeps_nothing_on_disk() {
    let scratch = Scratch::new("no-file");
    let output = scratch.ap(&["secret", "set", "GITHUB_TOKEN"], VALUE);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    for file in files(&scratch.root) {
        let bytes = fs::read(&file).unwrap();
        let holds = bytes
            .windows(VALUE.len())
            .any(|window| window == VALUE.as_bytes());
        assert!(!holds, "{} holds the secret", file.display());
    }
}

// spec(ap secret): It never takes the value as an argument, so the value stays out of your shell history and the process list.
#[test]
fn set_refuses_a_value_argument_without_printing_it() {
    let scratch = Scratch::new("argument");
    for args in [
        &["secret", "set", "GITHUB_TOKEN", VALUE][..],
        &["secret", "set", "GITHUB_TOKEN", "--value", VALUE][..],
    ] {
        let output = scratch.ap(args, "");
        let message = stderr(&output);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {message}");
        assert!(message.contains("usage"), "{message}");
        assert!(!message.contains(VALUE), "{message}");
        assert!(!String::from_utf8_lossy(&output.stdout).contains(VALUE));
    }
}

#[test]
fn bad_usage_prints_usage_and_exits_2() {
    let scratch = Scratch::new("usage");
    for args in [
        &["secret"][..],
        &["secret", "set"][..],
        &["secret", "rm"][..],
        &["secret", "rm", "A", "B"][..],
        &["secret", "ls", "A"][..],
        &["secret", "set", "-n"][..],
        &["secret", "nope"][..],
    ] {
        let output = scratch.ap(args, "");
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(stderr(&output).contains("usage"), "{args:?}");
    }
}
