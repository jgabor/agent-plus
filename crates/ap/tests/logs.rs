//! `ap logs`, from SPEC.md's `ap logs` section. Nothing here boots: a test
//! writes a run's log the way `ap run` does, and reads it back with `ap
//! logs`.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use ap::runs;
use store::Runs;

/// A test's own directory, which holds a home with a store.
struct Scratch {
    home: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("logs")
            .join(name);
        let _ = fs::remove_dir_all(&root);
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        common::seed_os(&home.join(".local/share/ap"));
        Scratch {
            home: home.canonicalize().unwrap(),
        }
    }

    fn runs(&self) -> Runs {
        Runs::new(self.home.join(".local/share/ap"))
    }

    /// Runs `ap` with only `HOME` in its environment and no terminal.
    fn ap(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ap"))
            .args(args)
            .current_dir(&self.home)
            .env_clear()
            .env("HOME", &self.home)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
}

// spec(Grants): `ap diff` and `ap logs` do the same for names that an agent chose.
#[test]
fn a_name_the_agent_chose_prints_with_controls_and_overrides_escaped() {
    let scratch = Scratch::new("escaped");
    let runs = scratch.runs();
    let run = runs.create("notes", 1).unwrap();
    let mut log = runs.create_log(&run.name).unwrap();
    runs::write_log(&mut log, false, b"out\n").unwrap();
    // A name that clears your screen, one that a right-to-left override
    // shows as `example.com`, one that rings the bell, and one with a tab
    // and an isolate. A backslash shows doubled, so no escape is forged.
    for line in [
        "deny   clear\x1b[2J.example  the default",
        "allow  \u{202e}moc.elpmaxe:443  NETWORK +example.com",
        "deny   bell\x07.example  the default",
        "deny   a\tb\u{2066}c\\u{7}.example  the default",
    ] {
        runs::write_decision(&mut log, 0, line).unwrap();
    }

    let output = scratch.ap(&["logs", &run.name]);

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "out\n");
    let shown = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        shown,
        concat!(
            r"1970-01-01T00:00:00Z  deny   clear\u{1b}[2J.example  the default",
            "\n",
            r"1970-01-01T00:00:00Z  allow  \u{202e}moc.elpmaxe:443  NETWORK +example.com",
            "\n",
            r"1970-01-01T00:00:00Z  deny   bell\u{7}.example  the default",
            "\n",
            r"1970-01-01T00:00:00Z  deny   a\u{9}b\u{2066}c\\u{7}.example  the default",
            "\n",
        )
    );
}
