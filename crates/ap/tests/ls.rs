//! `ap ls`, from SPEC.md's `ap ls` section: one line for each built agent,
//! with its name and the directory it was built from.

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
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ls").join(name);
        let _ = fs::remove_dir_all(&root);
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        Scratch {
            root: root.canonicalize().unwrap(),
            home: home.canonicalize().unwrap(),
        }
    }

    /// Runs `ap` in the root, with only `HOME` in its environment.
    fn ap(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ap"))
            .args(args)
            .current_dir(&self.root)
            .env_clear()
            .env("HOME", &self.home)
            .output()
            .unwrap()
    }

    /// Makes the directory `path` under the root with an Agentfile, builds
    /// it, and returns its path.
    fn build(&self, path: &str) -> PathBuf {
        let dir = self.root.join(path);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("Agentfile"), "AGENT claude\n").unwrap();
        let output = self.ap(&["build", dir.to_str().unwrap()]);
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        dir
    }

    /// Runs `ap ls`, fails the test unless it exits 0 with nothing on
    /// standard error, and returns its standard output.
    fn ls(&self) -> String {
        let output = self.ap(&["ls"]);
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        assert_eq!(stderr(&output), "");
        String::from_utf8(output.stdout).unwrap()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

// spec(ap ls): `ap ls` lists the built agents.
#[test]
fn ls_lists_the_built_agents() {
    let scratch = Scratch::new("lists");
    let zeta = scratch.build("zeta");
    let alpha = scratch.build("alpha");
    assert_eq!(
        scratch.ls(),
        format!("alpha {}\nzeta {}\n", alpha.display(), zeta.display())
    );
}

#[test]
fn ls_prints_nothing_for_an_empty_store() {
    let scratch = Scratch::new("empty");
    assert_eq!(scratch.ls(), "");
}

#[test]
fn ls_escapes_control_characters_in_a_directory() {
    let scratch = Scratch::new("escapes");
    scratch.build("a\nb\u{202e}c/notes");
    assert_eq!(
        scratch.ls(),
        format!(
            "notes {}/a\\u{{a}}b\\u{{202e}}c/notes\n",
            scratch.root.display()
        )
    );
}

#[test]
fn ls_quotes_a_directory_with_a_space() {
    let scratch = Scratch::new("quotes");
    scratch.build("my projects/notes");
    assert_eq!(
        scratch.ls(),
        format!("notes \"{}/my projects/notes\"\n", scratch.root.display())
    );
}

#[test]
fn ls_takes_no_arguments() {
    let scratch = Scratch::new("arguments");
    let output = scratch.ap(&["ls", "notes"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("ap ls\n"), "{}", stderr(&output));
}

#[test]
fn usage_lists_ls() {
    let scratch = Scratch::new("usage");
    let output = scratch.ap(&[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("ap ls\n"), "{}", stderr(&output));
}
