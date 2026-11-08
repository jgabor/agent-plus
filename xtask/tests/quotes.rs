//! Runs `score` and `gate` against workspaces whose tests quote a spec.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const SPEC: &str = "# A\n\nOne. Two.\n\n## B\n\nThree.\n";
const CHECK_ONE: &str = "// spec(A): One.\n#[test]\nfn one() {}\n";

fn package_manifest(name: &str) -> String {
    format!("[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2024\"\n")
}

/// A workspace with `crates/demo`, whose `src/lib.rs` is `lib`.
fn workspace(name: &str, spec: &str, lib: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("quotes")
        .join(name);
    if root.exists() {
        fs::remove_dir_all(&root).unwrap();
    }
    let manifest = package_manifest("demo");
    let base = [
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"3\"\n",
        ),
        ("SPEC.md", spec),
        ("crates/demo/Cargo.toml", manifest.as_str()),
        ("crates/demo/src/lib.rs", lib),
    ];
    for (path, content) in base.iter().chain(files) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    let output = Command::new("cargo")
        .arg("generate-lockfile")
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(output.status.success());
    root
}

fn xtask(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(args)
        .current_dir(root)
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

#[track_caller]
fn assert_score(lib: &str, name: &str, expected: &str) {
    let root = workspace(name, SPEC, lib, &[]);
    let output = xtask(&root, &["score"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), format!("{expected}\n"));
}

#[test]
fn a_passing_check_proves_its_promise() {
    assert_score(CHECK_ONE, "proves", "spec 1 of 3");
}

#[test]
fn a_failing_check_proves_nothing() {
    let lib = "// spec(A): One.\n#[test]\nfn one() {\n    panic!();\n}\n";
    assert_score(lib, "failing", "spec 0 of 3");
}

#[test]
fn an_ignored_check_proves_nothing() {
    let lib = "// spec(A): One.\n#[test]\n#[ignore]\nfn one() {}\n";
    assert_score(lib, "ignored", "spec 0 of 3");
}

#[test]
fn a_test_without_a_quote_proves_nothing() {
    assert_score("#[test]\nfn one() {}\n", "unquoted", "spec 0 of 3");
}

#[test]
fn one_check_can_quote_several_promises() {
    let lib = "// spec(A): One.\n// spec(A): Two.\n// spec(B): Three.\n#[test]\nfn all() {}\n";
    assert_score(lib, "several", "spec 3 of 3");
}

#[test]
fn two_checks_quoting_one_promise_count_once() {
    let lib = "// spec(A): One.\n#[test]\nfn a() {}\n\n// spec(A): One.\n#[test]\nfn b() {}\n";
    assert_score(lib, "twice", "spec 1 of 3");
}

#[test]
fn one_passing_and_one_failing_check_still_prove_it() {
    let lib = "// spec(A): One.\n#[test]\nfn a() {}\n\n// spec(A): One.\n#[test]\nfn b() {\n    panic!();\n}\n";
    assert_score(lib, "mixed", "spec 1 of 3");
}

#[test]
fn a_check_inside_a_test_module_counts() {
    let lib = "#[cfg(test)]\nmod tests {\n    // spec(A): One.\n    #[test]\n    fn one() {}\n}\n";
    assert_score(lib, "module", "spec 1 of 3");
}

#[test]
fn a_check_in_an_integration_test_counts() {
    let root = workspace(
        "integration",
        SPEC,
        "",
        &[("crates/demo/tests/flow.rs", CHECK_ONE)],
    );
    assert_eq!(stdout(&xtask(&root, &["score"])), "spec 1 of 3\n");
}

#[test]
fn a_check_sharing_a_name_with_another_test_fails() {
    let lib = "// spec(A): One.\n#[test]\nfn one() {}\n\n#[cfg(test)]\nmod other {\n    #[test]\n    fn one() {}\n}\n";
    let root = workspace("collision", SPEC, lib, &[]);
    let output = xtask(&root, &["score"]);
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(stderr(&output).contains("one"), "{}", stderr(&output));
}

#[test]
fn a_disabled_check_cannot_borrow_another_result() {
    let lib = "// spec(A): One.\n#[test]\n#[cfg(windows)]\nfn same() {}\n\n#[cfg(test)]\nmod other {\n    #[test]\n    fn same() {}\n}\n";
    let root = workspace("borrowed", SPEC, lib, &[]);
    let output = xtask(&root, &["score"]);
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
}

#[test]
fn a_multiline_attribute_is_allowed() {
    let lib = "// spec(A): One.\n#[test]\n#[cfg(any(\n    unix,\n    windows\n))]\nfn one() {}\n";
    assert_score(lib, "multiline", "spec 1 of 3");
}

#[test]
fn a_stale_quote_fails_and_names_the_file() {
    let lib = "// spec(A): One, edited.\n#[test]\nfn one() {}\n";
    let root = workspace("stale", SPEC, lib, &[]);
    let output = xtask(&root, &["score"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("lib.rs:1"), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("One, edited."),
        "{}",
        stderr(&output)
    );
}

#[test]
fn a_quote_under_the_wrong_heading_fails() {
    let lib = "// spec(B): One.\n#[test]\nfn one() {}\n";
    let root = workspace("wrong-heading", SPEC, lib, &[]);
    assert_eq!(xtask(&root, &["score"]).status.code(), Some(1));
}

#[test]
fn a_quote_not_above_a_test_fails() {
    let root = workspace(
        "not-a-test",
        SPEC,
        "// spec(A): One.\npub fn one() {}\n",
        &[],
    );
    let output = xtask(&root, &["score"]);
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(stderr(&output).contains("lib.rs:1"), "{}", stderr(&output));
}

#[test]
fn a_blank_line_breaks_the_link_to_the_test() {
    let lib = "// spec(A): One.\n\n#[test]\nfn one() {}\n";
    let root = workspace("blank", SPEC, lib, &[]);
    assert_eq!(xtask(&root, &["score"]).status.code(), Some(1));
}

#[test]
fn two_checks_with_one_name_in_a_package_fail() {
    let lib = "// spec(A): One.\n#[test]\nfn same() {}\n\n#[cfg(test)]\nmod other {\n    // spec(A): Two.\n    #[test]\n    fn same() {}\n}\n";
    let root = workspace("same-name", SPEC, lib, &[]);
    let output = xtask(&root, &["score"]);
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(stderr(&output).contains("same"), "{}", stderr(&output));
}

#[test]
fn one_name_in_two_packages_is_fine() {
    let lib = "// spec(A): One.\n#[test]\nfn same() {}\n";
    let other = package_manifest("other");
    let other_lib = "// spec(A): Two.\n#[test]\nfn same() {}\n";
    let root = workspace(
        "two-packages",
        SPEC,
        lib,
        &[
            ("crates/other/Cargo.toml", &other),
            ("crates/other/src/lib.rs", other_lib),
        ],
    );
    let output = xtask(&root, &["score"]);
    assert_eq!(stdout(&output), "spec 2 of 3\n", "{}", stderr(&output));
}

#[test]
fn json_output_carries_the_same_counts() {
    let root = workspace("json", SPEC, CHECK_ONE, &[]);
    let output = xtask(&root, &["score", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "{\"spec\":{\"proven\":1,\"promises\":3}}\n"
    );
}

#[test]
fn test_output_stays_off_stdout() {
    let root = workspace("streams", SPEC, CHECK_ONE, &[]);
    let out = stdout(&xtask(&root, &["score"]));
    assert!(!out.contains("test result"), "{out:?}");
}

#[test]
fn the_gate_passes_with_a_true_quote() {
    let root = workspace("gate-true", SPEC, CHECK_ONE, &[]);
    let output = xtask(&root, &["gate"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).contains("pass spec"), "{}", stdout(&output));
}

#[test]
fn editing_a_quoted_sentence_fails_the_gate() {
    let root = workspace("gate-edited", SPEC, CHECK_ONE, &[]);
    fs::write(root.join("SPEC.md"), SPEC.replace("One.", "One, changed.")).unwrap();
    let output = xtask(&root, &["gate"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stdout(&output).contains("fail spec"), "{}", stdout(&output));
    assert!(
        stdout(&output).contains("gate: fail"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn a_failing_test_still_fails_the_gate() {
    let lib = "#[test]\nfn planted() {\n    panic!();\n}\n";
    let root = workspace("gate-failing", SPEC, lib, &[]);
    let output = xtask(&root, &["gate"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("fail tests"),
        "{}",
        stdout(&output)
    );
}
