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

const SPEC: &str = "# A\n\nOne. Two.\n";
const CHECK_ONE: &str = "// spec(A): One.\n#[test]\nfn one() {}\n";

/// A workspace whose packages are each a name and the `src/lib.rs` it holds.
fn with_packages(name: &str, packages: &[(&str, &str)]) -> PathBuf {
    let root = workspace(name, Some(SPEC));
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"3\"\n",
    )
    .unwrap();
    for (package, lib) in packages {
        let directory = root.join("crates").join(package);
        fs::create_dir_all(directory.join("src")).unwrap();
        let manifest =
            format!("[package]\nname = \"{package}\"\nversion = \"0.0.0\"\nedition = \"2024\"\n");
        fs::write(directory.join("Cargo.toml"), manifest).unwrap();
        fs::write(directory.join("src/lib.rs"), lib).unwrap();
    }
    let output = Command::new("cargo")
        .arg("generate-lockfile")
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(output.status.success());
    root
}

fn xtask_in(root: &Path, command: &str) -> (Option<i32>, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg(command)
        .current_dir(root)
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .unwrap();
    let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
    (
        output.status.code(),
        text(&output.stdout),
        text(&output.stderr),
    )
}

#[test]
fn a_promise_only_a_t2_check_quotes_fails() {
    let root = with_packages("t2-only", &[("t2", CHECK_ONE)]);
    let (code, _, stderr) = xtask_in(&root, "score");
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("crates/t2/src/lib.rs:1"), "{stderr}");
    assert!(stderr.contains("another tier"), "{stderr}");
}

#[test]
fn a_t2_quote_passes_with_a_check_in_another_package() {
    let root = with_packages("t2-and-demo", &[("t2", CHECK_ONE), ("demo", CHECK_ONE)]);
    let (code, stdout, stderr) = xtask_in(&root, "score");
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(stdout, "spec 1 of 2\n");
}

#[test]
fn a_check_linked_to_a_fake_is_not_another_tier() {
    let root = with_packages(
        "t2-and-fake",
        &[("t2", CHECK_ONE), ("fake-demo", CHECK_ONE)],
    );
    let (code, _, stderr) = xtask_in(&root, "score");
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("another tier"), "{stderr}");
}

#[test]
fn an_ignored_check_is_not_another_tier() {
    let ignored = "// spec(A): One.\n#[test]\n#[ignore]\nfn one() {}\n";
    let root = with_packages("t2-and-ignored", &[("t2", CHECK_ONE), ("demo", ignored)]);
    let (code, _, stderr) = xtask_in(&root, "score");
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("another tier"), "{stderr}");
}

/// A check whose test leaves a marker in its package when it runs.
const MARKS: &str = "// spec(A): Gone.\n#[test]\nfn one() {\n    std::fs::write(std::path::Path::new(env!(\"CARGO_MANIFEST_DIR\")).join(\"marker\"), \"\").unwrap();\n}\n";

#[test]
fn spec_reports_a_stale_quote_without_running_a_test() {
    let root = with_packages("spec-stale", &[("demo", MARKS)]);
    let (code, stdout, stderr) = xtask_in(&root, "spec");
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("crates/demo/src/lib.rs:1"), "{stderr}");
    assert!(stderr.contains("Gone."), "{stderr}");
    assert_eq!(stdout, "");
    assert!(!root.join("crates/demo/marker").exists(), "{stderr}");
}

#[test]
fn spec_leaves_what_needs_a_test_result_to_the_tests() {
    let root = with_packages("spec-t2-only", &[("t2", CHECK_ONE)]);
    let (code, stdout, stderr) = xtask_in(&root, "spec");
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!((stdout.as_str(), stderr.as_str()), ("", ""));
}
