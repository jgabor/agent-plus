//! Runs the real `xtask` binary against throwaway workspaces.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const GOOD: &str = "pub fn answer() -> u32 {\n    42\n}\n";

/// A workspace with one crate, whose `lib.rs` holds `source`.
fn workspace(name: &str, source: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    if root.exists() {
        fs::remove_dir_all(&root).unwrap();
    }
    fs::create_dir_all(root.join("demo/src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"demo\"]\nresolver = \"3\"\n",
    )
    .unwrap();
    fs::write(
        root.join("demo/Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(root.join("demo/src/lib.rs"), source).unwrap();
    let output = Command::new("cargo")
        .arg("generate-lockfile")
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(output.status.success());
    root
}

fn xtask(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(args)
        .current_dir(dir)
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn clean_workspace_passes() {
    let root = workspace("clean", GOOD);
    let output = xtask(&root, &["gate"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let out = stdout(&output);
    for line in ["pass format", "pass lint", "pass tests", "gate: pass"] {
        assert!(out.contains(line), "missing {line:?} in {out:?}");
    }
}

#[test]
fn bad_formatting_fails() {
    let root = workspace("format", "pub fn  answer( ) -> u32 { 42 }\n");
    let output = xtask(&root, &["gate"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("fail format"), "{out:?}");
    assert!(out.contains("gate: fail"), "{out:?}");
}

#[test]
fn a_lint_warning_fails() {
    let root = workspace("lint", "pub fn answer() {\n    let unused = 42;\n}\n");
    let output = xtask(&root, &["gate"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stdout(&output).contains("fail lint"));
}

#[test]
fn a_failing_test_fails() {
    let source = "#[test]\nfn planted() {\n    panic!(\"planted\");\n}\n";
    let root = workspace("tests", source);
    let output = xtask(&root, &["gate"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stdout(&output).contains("fail tests"));
}

#[test]
fn every_check_runs_after_a_failure() {
    let root = workspace("all", "pub fn  answer( ) -> u32 { 42 }\n");
    let out = stdout(&xtask(&root, &["gate"]));
    assert!(out.contains("fail format"), "{out:?}");
    assert!(out.contains("pass lint"), "{out:?}");
    assert!(out.contains("pass tests"), "{out:?}");
}

#[test]
fn check_output_stays_off_stdout() {
    let root = workspace("streams", GOOD);
    let out = stdout(&xtask(&root, &["gate"]));
    assert!(!out.contains("test result"), "{out:?}");
    assert!(!out.contains("Compiling"), "{out:?}");
}

#[test]
fn unknown_command_exits_2_and_names_the_choices() {
    let root = workspace("usage", GOOD);
    let output = xtask(&root, &["gaet"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("gate"), "{}", stderr(&output));
    assert!(stdout(&output).is_empty());
}

#[test]
fn no_command_exits_2() {
    let root = workspace("nothing", GOOD);
    assert_eq!(xtask(&root, &[]).status.code(), Some(2));
}

#[test]
fn stray_argument_exits_2() {
    let root = workspace("stray", GOOD);
    assert_eq!(xtask(&root, &["gate", "extra"]).status.code(), Some(2));
}

#[test]
fn help_exits_0_on_stdout() {
    let root = workspace("help", GOOD);
    let output = xtask(&root, &["--help"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(stdout(&output).contains("gate"));
}

#[test]
fn missing_formatter_is_an_environment_error() {
    let root = workspace("norustfmt", GOOD);
    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("gate")
        .current_dir(&root)
        .env("RUSTFMT", "/nonexistent/rustfmt")
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(stderr(&output).contains("rustfmt"), "{}", stderr(&output));
}

#[test]
fn missing_cargo_is_an_environment_error() {
    let root = workspace("nocargo", GOOD);
    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("gate")
        .current_dir(&root)
        .env("PATH", "")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("cargo"), "{}", stderr(&output));
}
