//! Runs `journey` against small workspaces with their own `ap` and steps.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const AP: &str = "fn main() {\n    println!(\"ap test\");\n}\n";
const PASS: &str = "#!/bin/sh\nexit 0\n";
const FAIL: &str = "#!/bin/sh\nexit 1\n";

fn manifest() -> String {
    "[package]\nname = \"ap\"\nversion = \"0.0.0\"\nedition = \"2024\"\n".to_string()
}

fn workspace(name: &str, journey: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("journey")
        .join(name);
    if root.exists() {
        fs::remove_dir_all(&root).unwrap();
    }
    let manifest = manifest();
    let base = [
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"3\"\n",
        ),
        ("crates/ap/Cargo.toml", manifest.as_str()),
        ("crates/ap/src/main.rs", AP),
        ("harness/journey.toml", journey),
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

fn journey(root: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("journey")
        .current_dir(root)
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("XTASK_ACTIVITY_HELD")
        .env("JOURNEY_LEAK", "from the caller")
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn step(id: u32, needs: &str, script: &str) -> String {
    format!(
        "[[step]]\nid = {id}\nname = \"step {id}\"\nneeds = [{needs}]\nscript = \"{script}\"\n\n"
    )
}

#[track_caller]
fn assert_journey(root: &Path, expected: &str) {
    let output = journey(root);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), expected, "{}", stderr(&output));
}

#[test]
fn a_passing_step_counts() {
    let root = workspace(
        "passes",
        &step(1, "", "harness/journey/one.sh"),
        &[("harness/journey/one.sh", PASS)],
    );
    assert_journey(&root, "pass step 1\njourney 1 of 1\n");
}

#[test]
fn a_failing_step_does_not_count() {
    let root = workspace(
        "fails",
        &step(1, "", "harness/journey/one.sh"),
        &[("harness/journey/one.sh", FAIL)],
    );
    assert_journey(&root, "fail step 1\njourney 0 of 1\n");
}

#[test]
fn a_step_with_no_script_yet_fails() {
    let root = workspace("no-script", &step(1, "", "harness/journey/one.sh"), &[]);
    let output = journey(&root);
    assert_eq!(stdout(&output), "fail step 1\njourney 0 of 1\n");
    assert!(stderr(&output).contains("one.sh"), "{}", stderr(&output));
}

#[test]
fn a_step_is_skipped_unless_every_need_passes() {
    let toml = format!(
        "{}{}{}",
        step(1, "", "harness/journey/pass.sh"),
        step(2, "", "harness/journey/fail.sh"),
        step(3, "1, 2", "harness/journey/pass.sh"),
    );
    let root = workspace(
        "needs",
        &toml,
        &[
            ("harness/journey/pass.sh", PASS),
            ("harness/journey/fail.sh", FAIL),
        ],
    );
    assert_journey(
        &root,
        "pass step 1\nfail step 2\nskip step 3\njourney 1 of 3\n",
    );
}

#[test]
fn a_spend_step_and_what_needs_it_do_not_run() {
    let toml = format!(
        "[[step]]\nid = 1\nname = \"s\"\nneeds = []\nscript = \"harness/journey/touch.sh\"\nspend = true\n\n{}",
        step(2, "1", "harness/journey/touch.sh"),
    );
    let root = workspace(
        "spend",
        &toml,
        &[(
            "harness/journey/touch.sh",
            "#!/bin/sh\ntouch \"$HOME/../../ran\"\n",
        )],
    );
    assert_journey(&root, "skip step 1\nskip step 2\njourney 0 of 2\n");
    assert!(!root.join("target/journey/ran").exists());
}

#[test]
fn a_step_naming_a_floor_fails_while_its_check_does_not_exist() {
    let toml = "[[step]]\nid = 1\nname = \"s\"\nneeds = []\nscript = \"harness/journey/one.sh\"\nfloor = \"floor_guest_host\"\n";
    let root = workspace("floor-missing", toml, &[("harness/journey/one.sh", PASS)]);
    let output = journey(&root);
    assert_eq!(stdout(&output), "fail step 1\njourney 0 of 1\n");
    assert!(
        stderr(&output).contains("floor_guest_host"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn a_step_naming_a_floor_passes_once_its_check_exists() {
    let toml = "[[step]]\nid = 1\nname = \"s\"\nneeds = []\nscript = \"harness/journey/one.sh\"\nfloor = \"floor_guest_host\"\n";
    let t1 = "[package]\nname = \"t1\"\nversion = \"0.0.0\"\nedition = \"2024\"\n";
    let root = workspace(
        "floor-present",
        toml,
        &[
            ("harness/journey/one.sh", PASS),
            ("crates/t1/Cargo.toml", t1),
            (
                "crates/t1/src/lib.rs",
                "#[test]\nfn floor_guest_host() {}\n",
            ),
        ],
    );
    assert_journey(&root, "pass step 1\njourney 1 of 1\n");
}

#[test]
fn a_step_runs_the_installed_release_binary() {
    let script = "#!/bin/sh\n[ \"$(ap)\" = \"ap test\" ] || exit 1\ncase \"$(command -v ap)\" in */target/journey/bin/ap) ;; *) exit 1 ;; esac\n";
    let root = workspace(
        "binary",
        &step(1, "", "harness/journey/one.sh"),
        &[("harness/journey/one.sh", script)],
    );
    assert_journey(&root, "pass step 1\njourney 1 of 1\n");
}

#[test]
fn a_step_gets_a_clean_environment() {
    let script = "#!/bin/sh\n[ -z \"${JOURNEY_LEAK:-}\" ] || exit 1\ncase \"$HOME\" in */target/journey/*) ;; *) exit 1 ;; esac\n[ -d \"$HOME\" ] || exit 1\n[ -z \"$(ls -A \"$PWD\")\" ] || exit 1\n";
    let root = workspace(
        "clean",
        &step(1, "", "harness/journey/one.sh"),
        &[("harness/journey/one.sh", script)],
    );
    assert_journey(&root, "pass step 1\njourney 1 of 1\n");
}

#[test]
fn each_run_starts_clean() {
    let script =
        "#!/bin/sh\n[ ! -e \"$HOME/left-behind\" ] || exit 1\ntouch \"$HOME/left-behind\"\n";
    let root = workspace(
        "repeat",
        &step(1, "", "harness/journey/one.sh"),
        &[("harness/journey/one.sh", script)],
    );
    assert_journey(&root, "pass step 1\njourney 1 of 1\n");
    assert_journey(&root, "pass step 1\njourney 1 of 1\n");
}

#[test]
fn steps_run_in_id_order() {
    let toml = format!(
        "{}{}",
        step(2, "1", "harness/journey/pass.sh"),
        step(1, "", "harness/journey/pass.sh"),
    );
    let root = workspace("order", &toml, &[("harness/journey/pass.sh", PASS)]);
    assert_journey(&root, "pass step 1\npass step 2\njourney 2 of 2\n");
}

#[test]
fn a_build_failure_is_a_problem() {
    let root = workspace(
        "no-build",
        &step(1, "", "harness/journey/one.sh"),
        &[
            ("harness/journey/one.sh", PASS),
            ("crates/ap/src/main.rs", "fn main() { nope }\n"),
        ],
    );
    assert_eq!(journey(&root).status.code(), Some(2));
}

#[test]
fn a_missing_journey_file_is_a_problem() {
    let root = workspace("no-file", "", &[]);
    fs::remove_file(root.join("harness/journey.toml")).unwrap();
    let output = journey(&root);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("journey.toml"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn a_need_on_a_step_that_does_not_exist_is_a_problem() {
    let root = workspace("bad-need", &step(1, "9", "harness/journey/one.sh"), &[]);
    let output = journey(&root);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains('9'), "{}", stderr(&output));
}

#[test]
fn a_repeated_step_id_is_a_problem() {
    let toml = format!(
        "{}{}",
        step(1, "", "harness/journey/one.sh"),
        step(1, "", "harness/journey/one.sh"),
    );
    let root = workspace("repeated", &toml, &[]);
    assert_eq!(journey(&root).status.code(), Some(2));
}

#[test]
fn a_journey_needs_no_arguments() {
    let root = workspace("extra", &step(1, "", "harness/journey/one.sh"), &[]);
    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["journey", "--nope"])
        .current_dir(&root)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn a_step_without_a_name_is_a_problem() {
    let toml = "[[step]]\nid = 1\nneeds = []\nscript = \"harness/journey/one.sh\"\n";
    let root = workspace("no-name", toml, &[("harness/journey/one.sh", PASS)]);
    let output = journey(&root);
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(stderr(&output).contains("`name`"), "{}", stderr(&output));
}

#[test]
fn a_mistyped_spend_is_a_problem() {
    let toml = "[[step]]\nid = 1\nname = \"s\"\nneeds = []\nscript = \"harness/journey/one.sh\"\nspend = \"true\"\n";
    let root = workspace("spend-type", toml, &[("harness/journey/one.sh", PASS)]);
    let output = journey(&root);
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(stderr(&output).contains("spend"), "{}", stderr(&output));
}

#[test]
fn a_mistyped_floor_is_a_problem() {
    let toml = "[[step]]\nid = 1\nname = \"s\"\nneeds = []\nscript = \"harness/journey/one.sh\"\nfloor = [\"floor_guest_host\"]\n";
    let root = workspace("floor-type", toml, &[("harness/journey/one.sh", PASS)]);
    let output = journey(&root);
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(stderr(&output).contains("floor"), "{}", stderr(&output));
}

#[test]
fn a_comment_is_not_a_floor_check() {
    let toml = "[[step]]\nid = 1\nname = \"s\"\nneeds = []\nscript = \"harness/journey/one.sh\"\nfloor = \"floor_guest_host\"\n";
    let t1 = "[package]\nname = \"t1\"\nversion = \"0.0.0\"\nedition = \"2024\"\n";
    let root = workspace(
        "floor-comment",
        toml,
        &[
            ("harness/journey/one.sh", PASS),
            ("crates/t1/Cargo.toml", t1),
            ("crates/t1/src/lib.rs", "// fn floor_guest_host() {}\n"),
        ],
    );
    assert_journey(&root, "fail step 1\njourney 0 of 1\n");
}
