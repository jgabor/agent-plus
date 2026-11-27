//! Runs the real `xtask` binary against throwaway workspaces.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

mod common;

const AT_ONCE_ENV: &str = "XTASK_PACKAGES_AT_ONCE";
const GOOD: &str = "pub fn answer() -> u32 {\n    42\n}\n";

/// A workspace with one crate, whose `lib.rs` holds `source`.
fn workspace(name: &str, source: &str) -> PathBuf {
    packages(name, &[("demo", source)])
}

/// A workspace of crates, each a name and the `lib.rs` it holds.
fn packages(name: &str, crates: &[(&str, &str)]) -> PathBuf {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    if root.exists() {
        fs::remove_dir_all(&root).unwrap();
    }
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("SPEC.md"), "# A\n").unwrap();
    common::crates(&root, crates);
    root
}

fn xtask(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(args)
        .current_dir(dir)
        .env("PATH", common::path(&[]))
        .env_remove("CARGO_TARGET_DIR")
        .env_remove(AT_ONCE_ENV)
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
    for line in [
        "pass format",
        "pass lint",
        "pass lint-macos",
        "pass tests",
        "gate: pass",
    ] {
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

/// Only a host that isn't macOS passes the first lint.
#[test]
#[cfg(target_os = "linux")]
fn a_lint_warning_that_only_macos_builds_fails() {
    let source = "#[cfg(target_os = \"macos\")]\npub fn answer() {\n    let unused = 42;\n}\n";
    let root = workspace("lint-macos", source);
    let output = xtask(&root, &["gate"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("pass lint\n"), "{out:?}");
    assert!(out.contains("fail lint-macos"), "{out:?}");
}

#[test]
fn a_failing_test_fails() {
    let source = "#[test]\nfn planted() {\n    panic!(\"planted\");\n}\n";
    let root = workspace("tests", source);
    let output = xtask(&root, &["gate"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stdout(&output).contains("fail tests"));
}

/// Starts a line on stderr, and ends it only after the other package wrote a
/// whole line of its own.
const STARTS: &str = r#"
#[test]
fn starts_a_line() {
    use std::io::Write;
    let mut stderr = std::io::stderr();
    stderr.write_all(b"first half, ").unwrap();
    meet("half", true);
    meet("whole", false);
    stderr.write_all(b"second half\n").unwrap();
}
"#;

const BETWEEN: &str = r#"
#[test]
fn writes_a_line_between() {
    use std::io::Write;
    meet("half", false);
    std::io::stderr().write_all(b"a whole line\n").unwrap();
    meet("whole", true);
}
"#;

#[test]
fn packages_run_side_by_side_and_keep_their_lines_whole() {
    let starts = format!("{}{STARTS}", common::MEET);
    let between = format!("{}{BETWEEN}", common::MEET);
    let root = packages("side-by-side", &[("a", &starts), ("b", &between)]);
    for fifo in ["half", "whole"] {
        common::fifo(&root.join(fifo));
    }
    let output = xtask(&root, &["gate"]);
    let err = stderr(&output);
    assert_eq!(output.status.code(), Some(0), "{err}");
    let lines: Vec<&str> = err.lines().collect();
    assert!(lines.contains(&"first half, second half"), "{err}");
    assert!(lines.contains(&"a whole line"), "{err}");
}

const FAILS: &str = "#[test]\nfn fails() {\n    panic!(\"planted\");\n}\n";

const FINISHES: &str = r#"#[test]
fn finishes() {
    let marks = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    std::fs::write(marks.join("finished"), "").unwrap();
}
"#;

/// One package at a time, so that the second waits for the first to fail.
#[test]
fn a_failing_package_fails_the_run_and_the_others_finish() {
    let root = packages("one-fails", &[("a", FAILS), ("b", FINISHES)]);
    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("gate")
        .current_dir(&root)
        .env("PATH", common::path(&[]))
        .env(AT_ONCE_ENV, "1")
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stdout(&output).contains("fail tests"));
    assert!(root.join("finished").exists(), "{}", stderr(&output));
}

#[test]
fn every_check_runs_after_a_failure() {
    let root = workspace("all", "pub fn  answer( ) -> u32 { 42 }\n");
    let out = stdout(&xtask(&root, &["gate"]));
    assert!(out.contains("fail format"), "{out:?}");
    assert!(out.contains("pass lint\n"), "{out:?}");
    assert!(out.contains("pass lint-macos"), "{out:?}");
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
        .env("PATH", common::path(&[]))
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

/// Allocates and touches 2 GiB, then stops, so that without a limit the test
/// passes instead of taking the machine with it.
const RUNAWAY: &str = "#[test]
fn runaway() {
    let mut blocks = Vec::new();
    for _ in 0..2048 {
        blocks.push(vec![1u8; 1 << 20]);
    }
    assert_eq!(blocks.len(), 2048);
}
";

#[test]
fn a_test_over_the_memory_limit_is_killed_and_the_gate_fails() {
    let root = workspace("runaway", RUNAWAY);
    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("gate")
        .current_dir(&root)
        .env("PATH", common::path(&[]))
        .env("XTASK_MEMORY_MAX", "512M")
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("SIGKILL"), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("fail tests"),
        "{}",
        stdout(&output)
    );
}

const KERNEL_DIGEST: &str = "a0c936696eb7d5ee3192bf53b9d281cecbb40ca9db520de72cb95817ad92ac72";

/// A repository whose pin names the digest of `kernel\n`, and whose test runs
/// `body` with `tree`, the path of the kernel that t1 fetches.
fn kernel_workspace(name: &str, body: &str) -> PathBuf {
    guest_workspace(name, "guest/kernel", KERNEL_DIGEST, "vmlinux", body)
}

/// A repository whose `<pins>/pin.toml` names `digest`, and whose test runs
/// `body` with `tree`, the path in `target/guest` that t1 fetches into.
fn guest_workspace(name: &str, pins: &str, digest: &str, tree: &str, body: &str) -> PathBuf {
    let source = format!(
        "#[test]\nfn guest() {{\n    let tree = std::path::Path::new(env!(\"CARGO_MANIFEST_DIR\")).join(\"../target/guest/{tree}\");\n    {body}\n}}\n"
    );
    let root = workspace(name, &source);
    fs::create_dir_all(root.join(pins)).unwrap();
    fs::write(
        root.join(pins).join("pin.toml"),
        format!("version = \"1\"\nurl = \"https://example.com/guest\"\nsha256 = \"{digest}\"\n"),
    )
    .unwrap();
    let output = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    root
}

fn kernel_store(root: &Path) -> PathBuf {
    root.join(".git/xtask/kernel/vmlinux")
}

fn store_kernel(root: &Path, content: &str) {
    let store = kernel_store(root);
    fs::create_dir_all(store.parent().unwrap()).unwrap();
    fs::write(store, content).unwrap();
}

#[test]
fn a_tree_without_the_kernel_gets_it_from_the_store() {
    let body = "assert_eq!(std::fs::read_to_string(&tree).unwrap(), \"kernel\\n\");";
    let root = kernel_workspace("kernel-out", body);
    store_kernel(&root, "kernel\n");
    let output = xtask(&root, &["gate"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
}

#[test]
fn the_gate_keeps_the_kernel_that_the_tests_fetched() {
    let body = "std::fs::create_dir_all(tree.parent().unwrap()).unwrap();\n    std::fs::write(&tree, \"kernel\\n\").unwrap();";
    let root = kernel_workspace("kernel-in", body);
    let output = xtask(&root, &["gate"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(fs::read_to_string(kernel_store(&root)).unwrap(), "kernel\n");
}

#[test]
fn only_the_pinned_kernel_moves_between_the_store_and_a_tree() {
    let body = "assert!(!tree.exists());\n    std::fs::create_dir_all(tree.parent().unwrap()).unwrap();\n    std::fs::write(&tree, \"other\\n\").unwrap();";
    let root = kernel_workspace("kernel-wrong", body);
    store_kernel(&root, "wrong\n");
    let output = xtask(&root, &["gate"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(fs::read_to_string(kernel_store(&root)).unwrap(), "wrong\n");
}

/// The digest of `os\n`, the uncompressed layer in an OS workspace.
const OS_DIGEST: &str = "6adc3cbcc7923c4ff9cbe6a1dd13e7f4cf99dd072363ce88feca9fdbf55c9f24";

/// A repository whose OS pin names the digest of `os\n`, and whose test runs
/// `body` with `tree`, the store in `target/guest` that t1 fetches the layer
/// into.
fn os_workspace(name: &str, body: &str) -> PathBuf {
    guest_workspace(
        name,
        "guest/os",
        OS_DIGEST,
        &format!("os-{OS_DIGEST}"),
        body,
    )
}

/// The layer's two files in `tree`, as a test body names them: its frame in
/// `zstd-layers`, and its uncompressed copy in `plain-layers`.
fn os_tree() -> [String; 2] {
    ["zstd-layers", "plain-layers"]
        .map(|directory| format!("tree.join(\"{directory}/{OS_DIGEST}\")"))
}

/// A test body that writes the layer into `tree` as t1 fetches it, with a
/// frame of `frame\n` and an uncompressed copy of `plain\n`.
fn write_os(plain: &str) -> String {
    let [zstd, uncompressed] = os_tree();
    format!(
        "for directory in [\"zstd-layers\", \"plain-layers\"] {{\n        std::fs::create_dir_all(tree.join(directory)).unwrap();\n    }}\n    std::fs::write({zstd}, \"frame\\n\").unwrap();\n    std::fs::write({uncompressed}, \"{plain}\\n\").unwrap();"
    )
}

/// The OS layer that the gate keeps in the shared git directory: its frame,
/// then its uncompressed copy.
fn os_store(root: &Path) -> [PathBuf; 2] {
    ["zstd-layer", "plain-layer"].map(|file| root.join(".git/xtask/os").join(file))
}

/// Keeps a layer in the store with a frame of `frame\n` and an uncompressed
/// copy of `plain`.
fn store_os(root: &Path, plain: &str) {
    let [zstd, plain_file] = os_store(root);
    fs::create_dir_all(zstd.parent().unwrap()).unwrap();
    fs::write(zstd, "frame\n").unwrap();
    fs::write(plain_file, plain).unwrap();
}

#[test]
fn a_tree_without_the_os_layer_gets_it_from_the_store() {
    let [zstd, plain] = os_tree();
    let body = format!(
        "assert_eq!(std::fs::read_to_string({zstd}).unwrap(), \"frame\\n\");\n    assert_eq!(std::fs::read_to_string({plain}).unwrap(), \"os\\n\");"
    );
    let root = os_workspace("os-out", &body);
    store_os(&root, "os\n");
    let output = xtask(&root, &["gate"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
}

#[test]
fn the_gate_keeps_the_os_layer_that_the_tests_fetched() {
    let root = os_workspace("os-in", &write_os("os"));
    let output = xtask(&root, &["gate"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let [zstd, plain] = os_store(&root);
    assert_eq!(fs::read_to_string(zstd).unwrap(), "frame\n");
    assert_eq!(fs::read_to_string(plain).unwrap(), "os\n");
}

#[test]
fn only_the_pinned_os_layer_moves_between_the_store_and_a_tree() {
    let body = format!("assert!(!tree.exists());\n    {}", write_os("other"));
    let root = os_workspace("os-wrong", &body);
    store_os(&root, "wrong\n");
    let output = xtask(&root, &["gate"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let [_, plain] = os_store(&root);
    assert_eq!(fs::read_to_string(plain).unwrap(), "wrong\n");
}
