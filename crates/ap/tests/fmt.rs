//! `ap fmt`, from SPEC.md's `ap fmt` section: reading and writing the file,
//! the arguments, and the refusals. The formatting rules themselves are the
//! `agentfile` crate's, and its tests check them.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const MESSY: &str = "agent  claude   \n\n\n\nshare\t./out rw\n";
const TIDY: &str = "AGENT claude\n\nSHARE ./out rw\n";

/// A new, empty directory for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("fmt")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn ap(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ap"))
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The names in a directory, sorted.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

// spec(ap fmt): `ap fmt [dir]` rewrites the Agentfile in `dir` in place.
// spec(Format): The default name is `Agentfile`.
#[test]
fn fmt_rewrites_the_agentfile_in_dir() {
    let root = scratch("in-dir");
    let dir = root.join("project");
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("Agentfile"), MESSY).unwrap();
    let output = ap(&root, &["fmt", "project"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(output.stdout.is_empty());
    assert_eq!(fs::read_to_string(dir.join("Agentfile")).unwrap(), TIDY);
    assert_eq!(names(&dir), ["Agentfile"]);
}

// spec(ap fmt): `dir` defaults to the current directory.
#[test]
fn fmt_defaults_to_the_current_directory() {
    let dir = scratch("current");
    fs::write(dir.join("Agentfile"), MESSY).unwrap();
    let output = ap(&dir, &["fmt"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(fs::read_to_string(dir.join("Agentfile")).unwrap(), TIDY);
}

#[test]
fn fmt_replaces_the_file_atomically_and_keeps_its_mode() {
    let dir = scratch("atomic");
    let file = dir.join("Agentfile");
    fs::write(&file, MESSY).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
    let before = fs::metadata(&file).unwrap().ino();
    let output = ap(&dir, &["fmt"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let after = fs::metadata(&file).unwrap();
    // A rename puts a new file in place, where a write in place keeps the old
    // one and could leave half of it behind.
    assert_ne!(after.ino(), before);
    assert_eq!(after.permissions().mode() & 0o7777, 0o640);
    assert_eq!(fs::read_to_string(&file).unwrap(), TIDY);
    assert_eq!(names(&dir), ["Agentfile"]);
}

#[test]
fn fmt_leaves_a_formatted_file_alone() {
    let dir = scratch("formatted");
    let file = dir.join("Agentfile");
    fs::write(&file, TIDY).unwrap();
    let before = fs::metadata(&file).unwrap().ino();
    let output = ap(&dir, &["fmt"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(fs::metadata(&file).unwrap().ino(), before);
    assert_eq!(fs::read_to_string(&file).unwrap(), TIDY);
}

#[test]
fn fmt_writes_through_a_symlink() {
    let dir = scratch("symlink");
    fs::write(dir.join("real"), MESSY).unwrap();
    std::os::unix::fs::symlink("real", dir.join("Agentfile")).unwrap();
    let output = ap(&dir, &["fmt"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        fs::symlink_metadata(dir.join("Agentfile"))
            .unwrap()
            .is_symlink()
    );
    assert_eq!(fs::read_to_string(dir.join("real")).unwrap(), TIDY);
    assert_eq!(names(&dir), ["Agentfile", "real"]);
}

// spec(ap fmt): `ap fmt --check` writes nothing, and exits 1 if the file would change.
#[test]
fn check_writes_nothing_and_exits_1_if_the_file_would_change() {
    let dir = scratch("check");
    let file = dir.join("Agentfile");
    fs::write(&file, MESSY).unwrap();
    for args in [&["fmt", "--check"][..], &["fmt", "--check", "."][..]] {
        let output = ap(&dir, args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(stderr(&output).contains("Agentfile"), "{}", stderr(&output));
        assert_eq!(fs::read_to_string(&file).unwrap(), MESSY);
        assert_eq!(names(&dir), ["Agentfile"]);
    }
    fs::write(&file, TIDY).unwrap();
    let output = ap(&dir, &["fmt", "--check"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
}

// spec(ap fmt): A file with errors isn't changed: `ap fmt` prints the errors and exits 1.
#[test]
fn a_file_with_errors_is_not_changed() {
    let dir = scratch("errors");
    let file = dir.join("Agentfile");
    let source = "agent  claude\nNOPE x  \n\n\nmodel a\nMODEL b\n";
    fs::write(&file, source).unwrap();
    for args in [&["fmt"][..], &["fmt", "--check"][..]] {
        let output = ap(&dir, args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        let message = stderr(&output);
        assert!(message.contains("Agentfile:2: "), "{message}");
        assert!(message.contains("Agentfile:6: "), "{message}");
        assert_eq!(fs::read_to_string(&file).unwrap(), source);
        assert_eq!(names(&dir), ["Agentfile"]);
    }
}

// spec(Format): An Agentfile is a UTF-8 text file.
#[test]
fn a_file_that_is_not_utf8_is_an_error() {
    let dir = scratch("not-utf8");
    let file = dir.join("Agentfile");
    fs::write(&file, b"AGENT claude\nSYSTEM \xff\n").unwrap();
    let output = ap(&dir, &["fmt"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("UTF-8"), "{}", stderr(&output));
    assert_eq!(fs::read(&file).unwrap(), b"AGENT claude\nSYSTEM \xff\n");
}

#[test]
fn a_missing_agentfile_is_an_error() {
    let dir = scratch("missing");
    let output = ap(&dir, &["fmt"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("no Agentfile"),
        "{}",
        stderr(&output)
    );
    assert!(names(&dir).is_empty());
}

#[test]
fn a_dir_that_is_not_a_directory_is_an_error() {
    let dir = scratch("not-dir");
    fs::write(dir.join("file"), TIDY).unwrap();
    for target in ["file", "nowhere"] {
        let output = ap(&dir, &["fmt", target]);
        assert_eq!(output.status.code(), Some(1), "{target}");
        let message = stderr(&output);
        assert!(message.contains(target), "{message}");
        assert!(message.contains("directory"), "{message}");
    }
}

#[test]
fn bad_arguments_print_usage_and_exit_2() {
    let dir = scratch("usage");
    fs::write(dir.join("Agentfile"), MESSY).unwrap();
    for args in [
        &["fmt", "--nope"][..],
        &["fmt", "-c"][..],
        &["fmt", ".", "."][..],
        &["fmt", "--check", "--check"][..],
    ] {
        let output = ap(&dir, args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(stderr(&output).contains("ap fmt"), "{}", stderr(&output));
        assert_eq!(fs::read_to_string(dir.join("Agentfile")).unwrap(), MESSY);
    }
}

#[test]
fn the_usage_message_lists_fmt() {
    let dir = scratch("usage-lists");
    let output = ap(&dir, &[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("ap fmt [--check] [dir]"),
        "{}",
        stderr(&output)
    );
}
