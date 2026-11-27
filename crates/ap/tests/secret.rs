//! `ap secret`, from SPEC.md's `ap secret` section. It keeps, shows, lists,
//! and removes the commands that print secrets' values, in a file under
//! the test's own home. v0.1.0 has no secret store, so `ap secret set`
//! without `--command` says so and fails.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
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

    /// Where `ap secret` keeps its commands.
    fn file(&self) -> PathBuf {
        self.root.join("home/.config/ap/secrets")
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

    /// Runs `ap` as [`Scratch::ap`] does, and fails unless it exits 0 and
    /// says nothing on standard error. Returns its standard output.
    fn ok(&self, args: &[&str]) -> String {
        let output = self.ap(args, "");
        assert_eq!(output.status.code(), Some(0), "{args:?}: {output:?}");
        assert!(output.stderr.is_empty(), "{args:?}: {}", stderr(&output));
        String::from_utf8(output.stdout).unwrap()
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

/// Fails if a file under `dir`, other than `except`, holds `VALUE`.
fn no_file_holds_the_value(dir: &Path, except: &[&Path]) {
    for file in files(dir) {
        if except.contains(&file.as_path()) {
            continue;
        }
        let bytes = fs::read(&file).unwrap();
        let holds = bytes
            .windows(VALUE.len())
            .any(|window| window == VALUE.as_bytes());
        assert!(!holds, "{} holds the secret", file.display());
    }
}

// spec(ap secret): `ap secret set <name> --command <command>` keeps the command, and no value, in `~/.config/ap/secrets`, a file that only you can read.
// spec(ap secret): The command is kept in the clear, so it mustn't hold the secret itself.
// spec(ap secret): `ap` never keeps a secret's value on disk in the clear.
#[test]
fn set_keeps_the_command_in_a_file_only_you_can_read() {
    let scratch = Scratch::new("set");
    let kept = scratch.root.join("kept");
    fs::write(&kept, VALUE).unwrap();
    let command = format!("touch ran; cat {}", kept.display());
    assert_eq!(
        scratch.ok(&["secret", "set", "GITHUB_TOKEN", "--command", &command]),
        ""
    );
    let file = scratch.file();
    let mode = fs::metadata(&file).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "{mode:o}");
    let text = fs::read_to_string(&file).unwrap();
    assert!(text.contains("GITHUB_TOKEN"), "{text}");
    assert!(text.contains(&command), "{text}");
    // It kept the command and ran nothing, so no value reached a file.
    assert!(!scratch.root.join("ran").exists());
    no_file_holds_the_value(&scratch.root, &[&kept]);
    // It never reads standard input.
    let output = scratch.ap(&["secret", "set", "OTHER", "--command", "true"], VALUE);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    no_file_holds_the_value(&scratch.root, &[&kept]);
}

// spec(ap secret): `ap secret show <name>` prints the name's command, as it was given.
// spec(ap secret): It never runs the command and never prints a value.
#[test]
fn show_prints_the_command_as_given_and_never_runs_it() {
    let scratch = Scratch::new("show");
    // Spaces, quotes, a backslash, a tab, a line break, a bidirectional
    // control, and a leading `#` come back as they went in.
    for command in [
        "touch ran; printf '%s\\n' \"$HOME\"\tx\npass show \"my token\"",
        "#touch ran \u{202e}",
    ] {
        scratch.ok(&["secret", "set", "TOKEN", "--command", command]);
        assert_eq!(
            scratch.ok(&["secret", "show", "TOKEN"]),
            format!("{command}\n")
        );
    }
    assert!(!scratch.root.join("ran").exists());
}

// spec(ap secret): `ap secret set`, `ap secret show`, `ap secret ls`, and `ap secret rm` keep, show, list, and remove the commands that print secrets' values.
// spec(ap secret): `ap secret ls` prints the names that have a command, one per line, and never their commands.
// spec(ap secret): `ap secret rm <name>` removes the name and its command.
// spec(ap secret): Setting a name that has a command replaces its command.
// spec(ap secret): `ap secret` works the same on every host.
#[test]
fn ls_lists_the_names_and_rm_removes_one() {
    let scratch = Scratch::new("ls");
    assert_eq!(scratch.ok(&["secret", "ls"]), "");
    scratch.ok(&[
        "secret",
        "set",
        "STRIPE_API_KEY",
        "--command",
        "pass show stripe",
    ]);
    scratch.ok(&[
        "secret",
        "set",
        "GITHUB_TOKEN",
        "--command",
        "pass show github",
    ]);
    let listed = scratch.ok(&["secret", "ls"]);
    assert_eq!(listed, "GITHUB_TOKEN\nSTRIPE_API_KEY\n");
    scratch.ok(&["secret", "set", "GITHUB_TOKEN", "--command", "op read gh"]);
    assert_eq!(scratch.ok(&["secret", "ls"]), listed);
    assert_eq!(
        scratch.ok(&["secret", "show", "GITHUB_TOKEN"]),
        "op read gh\n"
    );
    assert_eq!(scratch.ok(&["secret", "rm", "GITHUB_TOKEN"]), "");
    assert_eq!(scratch.ok(&["secret", "ls"]), "STRIPE_API_KEY\n");
    assert_eq!(
        scratch.ok(&["secret", "show", "STRIPE_API_KEY"]),
        "pass show stripe\n"
    );
}

// spec(ap secret): For a name that has no command, `ap secret show <name>` and `ap secret rm <name>` say so on standard error and exit 1.
#[test]
fn show_and_rm_fail_for_a_name_with_no_command() {
    let scratch = Scratch::new("missing");
    for kept in [false, true] {
        if kept {
            // Another name's command is no command for this one.
            scratch.ok(&["secret", "set", "OTHER", "--command", "true"]);
        }
        for command in ["show", "rm"] {
            let output = scratch.ap(&["secret", command, "GITHUB_TOKEN"], "");
            let message = stderr(&output);
            assert_eq!(output.status.code(), Some(1), "{command}: {message}");
            assert!(output.stdout.is_empty(), "{command}");
            assert!(message.contains("GITHUB_TOKEN"), "{message}");
            assert!(message.contains("no command"), "{message}");
            assert!(!message.contains("usage"), "{message}");
        }
    }
    assert_eq!(scratch.ok(&["secret", "ls"]), "OTHER\n");
}

// spec(ap secret): v0.1.0 has no secret store on any host.
// spec(ap secret): `ap secret set <name>` without `--command` says that v0.1.0 has no secret store, names `--command` beside the environment variable that [SECRET](#secret) reads first, and exits 1.
// spec(ap secret): It reads no value.
// spec(ap secret): `ap` never keeps a secret's value on disk in the clear.
#[test]
fn set_without_a_command_says_there_is_no_store() {
    let scratch = Scratch::new("no-store");
    let output = scratch.ap(&["secret", "set", "GITHUB_TOKEN"], VALUE);
    let message = stderr(&output);
    assert_eq!(output.status.code(), Some(1), "{message}");
    assert!(output.stdout.is_empty());
    assert!(message.contains("v0.1.0 has no secret store"), "{message}");
    assert!(!message.contains("Linux"), "{message}");
    assert!(
        message.contains("environment variable GITHUB_TOKEN"),
        "{message}"
    );
    assert!(
        message.contains("ap secret set GITHUB_TOKEN --command"),
        "{message}"
    );
    assert!(!message.contains("usage"), "{message}");
    assert!(!message.contains(VALUE), "{message}");
    assert!(!scratch.file().exists());
    no_file_holds_the_value(&scratch.root, &[]);
}

// spec(ap secret): `ap secret set` never takes a value as an argument, so a value stays out of your shell history and the process list.
#[test]
fn set_refuses_a_value_argument_without_printing_it() {
    let scratch = Scratch::new("argument");
    let assignment = format!("GITHUB_TOKEN={VALUE}");
    for args in [
        &["secret", "set", "GITHUB_TOKEN", VALUE][..],
        &["secret", "set", "GITHUB_TOKEN", "--value", VALUE][..],
        &["secret", "set", &assignment][..],
        &["secret", "set", &assignment, "--command", "true"][..],
        &["secret", "set", "GITHUB_TOKEN", "--command", "true", VALUE][..],
    ] {
        let output = scratch.ap(args, "");
        let message = stderr(&output);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {message}");
        assert!(message.contains("usage"), "{message}");
        assert!(!message.contains(VALUE), "{message}");
        assert!(!String::from_utf8_lossy(&output.stdout).contains(VALUE));
    }
    assert!(!scratch.file().exists());
}

#[test]
fn bad_usage_prints_usage_and_exits_2() {
    let scratch = Scratch::new("usage");
    for args in [
        &["secret"][..],
        &["secret", "set"][..],
        &["secret", "show"][..],
        &["secret", "rm"][..],
        &["secret", "rm", "A", "B"][..],
        &["secret", "show", "A", "B"][..],
        &["secret", "ls", "A"][..],
        &["secret", "set", "-n"][..],
        &["secret", "set", "A", "--command"][..],
        &["secret", "set", "A", "--command", ""][..],
        &["secret", "set", "A", "--cmd", "true"][..],
        &["secret", "nope"][..],
    ] {
        let output = scratch.ap(args, "");
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(stderr(&output).contains("usage"), "{args:?}");
    }
}
