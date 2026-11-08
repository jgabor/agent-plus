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

fn with_waivers(name: &str, spec: &str, lib: &str, waivers: &str) -> PathBuf {
    workspace(name, spec, lib, &[("harness/waived.txt", waivers)])
}

#[test]
fn a_waived_unit_is_not_a_promise() {
    let root = with_waivers("waived", SPEC, "", "A\tTwo.\tDescribes the layout.\n");
    let output = xtask(&root, &["score"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "spec 0 of 2\n");
}

#[test]
fn waiver_comments_and_blank_lines_are_ignored() {
    let root = with_waivers(
        "waiver-comments",
        SPEC,
        "",
        "# why these are waived\n\nA\tTwo.\treason\n",
    );
    let output = xtask(&root, &["score"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "spec 0 of 2\n");
}

#[test]
fn a_repeated_waiver_counts_once() {
    let root = with_waivers(
        "waiver-repeated",
        SPEC,
        "",
        "A\tTwo.\tone reason\nA\tTwo.\tanother reason\n",
    );
    assert_eq!(stdout(&xtask(&root, &["score"])), "spec 0 of 2\n");
}

#[test]
fn a_stale_waiver_fails_and_names_its_line() {
    let root = with_waivers("waiver-stale", SPEC, "", "# note\nA\tGone.\treason\n");
    let output = xtask(&root, &["score"]);
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(
        stderr(&output).contains("harness/waived.txt:2"),
        "{}",
        stderr(&output)
    );
    assert_eq!(stdout(&output), "spec 0 of 3\n");
}

#[test]
fn a_waiver_under_the_wrong_heading_is_stale() {
    let root = with_waivers("waiver-heading", SPEC, "", "B\tOne.\treason\n");
    assert_eq!(xtask(&root, &["score"]).status.code(), Some(1));
}

#[test]
fn a_waiver_of_a_marked_unit_is_stale() {
    let spec = "# A\n\n*Proposed:* One.\n\nTwo.\n";
    let root = with_waivers("waiver-marked", spec, "", "A\tOne.\treason\n");
    assert_eq!(xtask(&root, &["score"]).status.code(), Some(1));
}

#[test]
fn a_stale_waiver_fails_the_gate() {
    let root = with_waivers("waiver-gate", SPEC, "", "A\tGone.\treason\n");
    let output = xtask(&root, &["gate"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stdout(&output).contains("fail spec"), "{}", stdout(&output));
}

#[test]
fn a_waiver_without_a_reason_is_a_problem() {
    for (index, line) in ["A\tTwo.\n", "A\tTwo.\t  \n", "A Two. reason\n"]
        .iter()
        .enumerate()
    {
        let root = with_waivers(&format!("waiver-malformed-{index}"), SPEC, "", line);
        let output = xtask(&root, &["score"]);
        assert_eq!(output.status.code(), Some(2), "{line:?}");
        assert!(stderr(&output).contains("harness/waived.txt:1"));
    }
}

#[test]
fn a_check_quoting_a_waived_unit_fails() {
    let root = with_waivers("waiver-quoted", SPEC, CHECK_ONE, "A\tOne.\treason\n");
    let output = xtask(&root, &["score"]);
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(stderr(&output).contains("waived"), "{}", stderr(&output));
    assert_eq!(stdout(&output), "spec 0 of 2\n");
}

#[test]
fn this_repository_merges_waivers_by_union() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let output = Command::new("git")
        .args(["check-attr", "merge", "--", "harness/waived.txt"])
        .current_dir(root)
        .output()
        .unwrap();
    assert_eq!(stdout(&output), "harness/waived.txt: merge: union\n");
}

/// `crates/demo` holds `CHECK_ONE`, with these lines appended to its manifest,
/// beside `crates/fake-vm`, an empty package named like a fake.
fn score_with_demo_manifest(name: &str, extra: &str, files: &[(&str, &str)]) -> Output {
    let demo = format!("{}{extra}", package_manifest("demo"));
    let fake = package_manifest("fake-vm");
    let mut all = vec![
        ("crates/demo/Cargo.toml", demo.as_str()),
        ("crates/fake-vm/Cargo.toml", fake.as_str()),
        ("crates/fake-vm/src/lib.rs", ""),
    ];
    all.extend_from_slice(files);
    let root = workspace(name, SPEC, CHECK_ONE, &all);
    xtask(&root, &["score"])
}

#[track_caller]
fn assert_spec(output: &Output, expected: &str) {
    assert_eq!(output.status.code(), Some(0), "{}", stderr(output));
    assert_eq!(stdout(output), format!("{expected}\n"));
}

#[test]
fn a_check_in_a_package_with_a_fake_dependency_earns_nothing() {
    let extra = "\n[dependencies]\nfake-vm = { path = \"../fake-vm\" }\n";
    assert_spec(
        &score_with_demo_manifest("fake-dep", extra, &[]),
        "spec 0 of 3",
    );
}

#[test]
fn a_fake_dev_dependency_earns_nothing() {
    let extra = "\n[dev-dependencies]\nfake-vm = { path = \"../fake-vm\" }\n";
    assert_spec(
        &score_with_demo_manifest("fake-dev", extra, &[]),
        "spec 0 of 3",
    );
}

#[test]
fn a_renamed_fake_dependency_earns_nothing() {
    let extra = "\n[dependencies]\nvm = { package = \"fake-vm\", path = \"../fake-vm\" }\n";
    assert_spec(
        &score_with_demo_manifest("fake-renamed", extra, &[]),
        "spec 0 of 3",
    );
}

#[test]
fn a_fake_reached_through_another_package_earns_nothing() {
    let extra = "\n[dependencies]\nmiddle = { path = \"../middle\" }\n";
    let middle = format!(
        "{}\n[dependencies]\nfake-vm = {{ path = \"../fake-vm\" }}\n",
        package_manifest("middle")
    );
    let files = [
        ("crates/middle/Cargo.toml", middle.as_str()),
        ("crates/middle/src/lib.rs", ""),
    ];
    assert_spec(
        &score_with_demo_manifest("fake-transitive", extra, &files),
        "spec 0 of 3",
    );
}

#[test]
fn a_dependency_with_a_fake_only_in_its_own_tests_still_earns() {
    let extra = "\n[dependencies]\nmiddle = { path = \"../middle\" }\n";
    let middle = format!(
        "{}\n[dev-dependencies]\nfake-vm = {{ path = \"../fake-vm\" }}\n",
        package_manifest("middle")
    );
    let files = [
        ("crates/middle/Cargo.toml", middle.as_str()),
        ("crates/middle/src/lib.rs", ""),
    ];
    assert_spec(
        &score_with_demo_manifest("fake-dev-of-dep", extra, &files),
        "spec 1 of 3",
    );
}

#[test]
fn a_check_inside_a_fake_earns_nothing() {
    let fake = package_manifest("fake-vm");
    let root = workspace(
        "fake-itself",
        SPEC,
        "",
        &[
            ("crates/fake-vm/Cargo.toml", fake.as_str()),
            ("crates/fake-vm/src/lib.rs", CHECK_ONE),
        ],
    );
    assert_spec(&xtask(&root, &["score"]), "spec 0 of 3");
}

#[test]
fn a_stale_quote_beside_a_fake_still_fails() {
    let extra = "\n[dependencies]\nfake-vm = { path = \"../fake-vm\" }\n";
    let demo = format!("{}{extra}", package_manifest("demo"));
    let fake = package_manifest("fake-vm");
    let root = workspace(
        "fake-stale",
        SPEC,
        "// spec(A): Gone.\n#[test]\nfn one() {}\n",
        &[
            ("crates/demo/Cargo.toml", demo.as_str()),
            ("crates/fake-vm/Cargo.toml", fake.as_str()),
            ("crates/fake-vm/src/lib.rs", ""),
        ],
    );
    assert_eq!(xtask(&root, &["score"]).status.code(), Some(1));
}

#[test]
fn a_dependency_not_named_like_a_fake_earns() {
    let extra = "\n[dependencies]\nfaker = { path = \"../faker\" }\n";
    let faker = package_manifest("faker");
    let files = [
        ("crates/faker/Cargo.toml", faker.as_str()),
        ("crates/faker/src/lib.rs", ""),
    ];
    assert_spec(
        &score_with_demo_manifest("not-fake", extra, &files),
        "spec 1 of 3",
    );
}
