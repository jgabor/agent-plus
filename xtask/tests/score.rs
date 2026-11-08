//! Runs `cargo xtask score` against throwaway workspaces.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn workspace(name: &str, spec: Option<&str>) -> PathBuf {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("score")
        .join(name);
    if root.exists() {
        fs::remove_dir_all(&root).unwrap();
    }
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = []\nresolver = \"3\"\n",
    )
    .unwrap();
    if let Some(spec) = spec {
        fs::write(root.join("SPEC.md"), spec).unwrap();
    }
    root
}

fn score(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("score")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap()
}

#[test]
fn counts_the_units_of_the_spec() {
    let root = workspace("counts", Some("# A\n\nOne. Two.\n\n- Three.\n"));
    let output = score(&root, &[]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "spec 0 of 3\n");
}

#[test]
fn a_missing_spec_is_an_environment_error() {
    let root = workspace("missing", None);
    let output = score(&root, &[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("SPEC.md"));
}

#[test]
fn a_duplicate_heading_names_the_heading() {
    let root = workspace("duplicate", Some("# A\n\n## B\n\n## B\n"));
    let output = score(&root, &[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("`B`"));
}

#[test]
fn a_stray_argument_exits_2() {
    let root = workspace("stray", Some("# A\n"));
    assert_eq!(score(&root, &["extra"]).status.code(), Some(2));
}
