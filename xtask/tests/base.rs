//! Runs `gate` in real git repositories, to compare a branch with its base.
//! A fake `gh` on `PATH` stands in for GitHub, so no test reaches it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

mod common;

const SPEC: &str = "# A\n\nOne. Two.\n\n## B\n\nThree.\n";
const CHECK_ONE: &str = "// spec(A): One.\n#[test]\nfn one() {}\n";
const NO_CHECK: &str = "pub fn f() {}\n";

fn package_manifest(name: &str) -> String {
    format!("[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2024\"\n")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Runs a program with no access to the machine's git configuration, which
/// could sign commits or pick another default branch.
fn isolated(program: impl AsRef<std::ffi::OsStr>, root: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("PATH", common::path(&[]))
        .env_remove("CARGO_TARGET_DIR");
    command
}

struct Repo {
    root: PathBuf,
    bin: PathBuf,
}

impl Repo {
    /// A workspace with `crates/demo`, not yet under git.
    fn files(name: &str, lib: &str, extra: &[(&str, &str)]) -> Repo {
        let parent = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("base")
            .join(name);
        if parent.exists() {
            fs::remove_dir_all(&parent).unwrap();
        }
        let repo = Repo {
            root: parent.join("repo"),
            bin: parent.join("bin"),
        };
        fs::create_dir_all(&repo.bin).unwrap();
        repo.set_gh("echo \"unexpected gh call: $*\" >&2\nexit 3\n");
        let manifest = package_manifest("demo");
        let base = [
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"3\"\n",
            ),
            (".gitignore", "/target\n"),
            ("SPEC.md", SPEC),
            ("crates/demo/Cargo.toml", manifest.as_str()),
            ("crates/demo/src/lib.rs", lib),
        ];
        for (path, content) in base.iter().chain(extra) {
            repo.write(path, content);
        }
        repo.lockfile();
        repo
    }

    /// The same workspace committed on `main`, with `origin/main` at that
    /// commit, and branch `work` checked out from it.
    fn new(name: &str, lib: &str, extra: &[(&str, &str)]) -> Repo {
        let repo = Repo::files(name, lib, extra);
        repo.git(&["init", "-q", "-b", "main"]);
        repo.commit();
        repo.publish();
        repo.git(&["checkout", "-q", "-b", "work"]);
        repo
    }

    fn write(&self, path: &str, content: &str) {
        let path = self.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn lockfile(&self) {
        let output = isolated("cargo", &self.root)
            .arg("generate-lockfile")
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
    }

    #[track_caller]
    fn git(&self, args: &[&str]) {
        let output = isolated("git", &self.root).args(args).output().unwrap();
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
    }

    fn commit(&self) {
        self.git(&["add", "-A"]);
        self.git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            "change",
        ]);
    }

    fn publish(&self) {
        self.git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
    }

    fn set_gh(&self, body: &str) {
        let path = self.bin.join("gh");
        common::write_tool(&path, &format!("#!/bin/sh\n{body}"));
    }

    fn gate_command(&self) -> Command {
        let mut command = isolated(env!("CARGO_BIN_EXE_xtask"), &self.root);
        command.arg("gate").env("PATH", common::path(&[&self.bin]));
        command
    }

    fn gate(&self) -> Output {
        self.gate_command().output().unwrap()
    }
}

#[track_caller]
fn assert_passes(output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(output),
        stderr(output)
    );
}

#[track_caller]
fn assert_fails(output: &Output, step: &str, message: &str) {
    assert_eq!(output.status.code(), Some(1), "{}", stderr(output));
    let out = stdout(output);
    assert!(out.contains(&format!("fail {step}\n")), "{out}");
    assert!(out.contains("gate: fail"), "{out}");
    assert!(stderr(output).contains(message), "{}", stderr(output));
}

#[test]
fn a_kept_check_passes() {
    let repo = Repo::new("kept", CHECK_ONE, &[]);
    let output = repo.gate();
    assert_passes(&output);
    assert!(stdout(&output).contains("pass checks-kept\n"));
}

#[test]
fn a_deleted_check_fails() {
    let repo = Repo::new("deleted", CHECK_ONE, &[]);
    repo.write("crates/demo/src/lib.rs", NO_CHECK);
    repo.commit();
    assert_fails(&repo.gate(), "checks-kept", "One.");
}

#[test]
fn an_uncommitted_deletion_fails() {
    let repo = Repo::new("uncommitted", CHECK_ONE, &[]);
    repo.write("crates/demo/src/lib.rs", NO_CHECK);
    assert_fails(&repo.gate(), "checks-kept", "One.");
}

#[test]
fn deleting_one_of_two_checks_on_a_sentence_fails() {
    let two =
        "// spec(A): One.\n#[test]\nfn one() {}\n\n// spec(A): One.\n#[test]\nfn again() {}\n";
    let repo = Repo::new("one-of-two", two, &[]);
    repo.write("crates/demo/src/lib.rs", CHECK_ONE);
    repo.commit();
    assert_fails(&repo.gate(), "checks-kept", "One.");
}

#[test]
fn repeating_a_quote_does_not_stand_in_for_a_check() {
    let two =
        "// spec(A): One.\n#[test]\nfn one() {}\n\n// spec(A): One.\n#[test]\nfn again() {}\n";
    let repeated = "// spec(A): One.\n// spec(A): One.\n#[test]\nfn one() {}\n";
    let repo = Repo::new("repeated", two, &[]);
    repo.write("crates/demo/src/lib.rs", repeated);
    repo.commit();
    assert_fails(&repo.gate(), "checks-kept", "One.");
}

#[test]
fn deleting_a_crate_with_a_check_fails() {
    let manifest = package_manifest("other");
    let repo = Repo::new(
        "crate",
        CHECK_ONE,
        &[
            ("crates/other/Cargo.toml", manifest.as_str()),
            ("crates/other/src/lib.rs", NO_CHECK),
        ],
    );
    fs::remove_dir_all(repo.root.join("crates/demo")).unwrap();
    repo.lockfile();
    repo.commit();
    assert_fails(&repo.gate(), "checks-kept", "One.");
}

#[test]
fn a_renamed_check_passes() {
    let repo = Repo::new("renamed", CHECK_ONE, &[]);
    repo.write(
        "crates/demo/src/lib.rs",
        "// spec(A): One.\n#[test]\nfn first() {}\n",
    );
    repo.commit();
    assert_passes(&repo.gate());
}

#[test]
fn a_check_moved_to_another_file_passes() {
    let repo = Repo::new("moved", CHECK_ONE, &[]);
    repo.write("crates/demo/src/lib.rs", NO_CHECK);
    repo.write("crates/demo/tests/one.rs", CHECK_ONE);
    repo.commit();
    assert_passes(&repo.gate());
}

#[test]
fn removing_the_sentence_too_passes() {
    let repo = Repo::new("sentence", CHECK_ONE, &[]);
    repo.write("crates/demo/src/lib.rs", NO_CHECK);
    repo.write("SPEC.md", "# A\n\nTwo.\n\n## B\n\nThree.\n");
    repo.commit();
    assert_passes(&repo.gate());
}

#[test]
fn a_check_main_gained_later_is_not_missing_from_the_branch() {
    let repo = Repo::new("later", NO_CHECK, &[]);
    repo.git(&["checkout", "-q", "main"]);
    repo.write("crates/demo/src/lib.rs", CHECK_ONE);
    repo.commit();
    repo.publish();
    repo.git(&["checkout", "-q", "work"]);
    assert_passes(&repo.gate());
}

#[test]
fn outside_a_repository_nothing_is_compared() {
    let repo = Repo::files("no-git", CHECK_ONE, &[]);
    let output = repo.gate();
    assert_passes(&output);
    assert!(
        stdout(&output).contains("skip base\n"),
        "{}",
        stdout(&output)
    );
    assert!(stderr(&output).contains("origin/main"));
}

#[test]
fn without_origin_main_nothing_is_compared() {
    let repo = Repo::files("no-origin", CHECK_ONE, &[]);
    repo.git(&["init", "-q", "-b", "main"]);
    repo.commit();
    let output = repo.gate();
    assert_passes(&output);
    assert!(
        stdout(&output).contains("skip base\n"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn a_git_that_cannot_run_is_a_problem() {
    let repo = Repo::new("broken-git", CHECK_ONE, &[]);
    repo.write("crates/demo/src/lib.rs", NO_CHECK);
    repo.commit();
    let output = repo
        .gate_command()
        .env("GIT_CONFIG_COUNT", "invalid")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(
        !stdout(&output).contains("skip base"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn an_unrelated_history_is_a_problem() {
    let repo = Repo::new("unrelated", CHECK_ONE, &[]);
    repo.git(&["checkout", "-q", "--orphan", "fresh"]);
    repo.write("README.md", "So this root commit differs from the first.\n");
    repo.commit();
    let output = repo.gate();
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
}

/// What waits for the owner is for `merge` to decide from the diff, so the
/// gate reads no label, and never asks GitHub.
#[test]
fn any_path_passes_without_asking_github() {
    let paths: Vec<(usize, &str)> = [
        "SPEC.md",
        "AGENTS.md",
        "HARNESS.md",
        "harness/waived.txt",
        "harness/journey.toml",
        "harness/budgets.toml",
        "harness/tasks/nightly.md",
        "xtask/src/gate.rs",
        ".github/workflows/x.yml",
    ]
    .into_iter()
    .enumerate()
    .collect();
    common::each(&paths, |&(index, path)| {
        let repo = Repo::new(&format!("any-path-{index}"), NO_CHECK, &[]);
        let content = if path == "SPEC.md" {
            format!("{SPEC}\nFour.\n")
        } else {
            "# comment\n".to_string()
        };
        repo.write(path, &content);
        repo.commit();
        let output = repo.gate();
        assert_passes(&output);
        assert!(!stdout(&output).contains("labels"), "{}", stdout(&output));
    });
}

fn record_with_proven(proven: u32) -> String {
    format!(
        "issue = 1\npr = 2\ncommit = \"c\"\ndate = \"2026-01-01\"\nhost = \"h\"\nspec = {{ proven = {proven}, promises = 3 }}\njourney = {{ passing = 0, steps = 1 }}\nreason = \"\"\n\n[budgets]\n"
    )
}

#[test]
fn a_count_below_the_base_record_fails() {
    let record = record_with_proven(2);
    let repo = Repo::new(
        "hills-lower",
        CHECK_ONE,
        &[("harness/ledger/1.toml", record.as_str())],
    );
    assert_fails(&repo.gate(), "hills", "2");
}

#[test]
fn a_count_level_with_the_base_record_passes() {
    let record = record_with_proven(1);
    let repo = Repo::new(
        "hills-level",
        CHECK_ONE,
        &[("harness/ledger/1.toml", record.as_str())],
    );
    let output = repo.gate();
    assert_passes(&output);
    assert!(stdout(&output).contains("pass hills\n"));
}

#[test]
fn with_no_record_the_base_scores_zero() {
    let repo = Repo::new("hills-none", NO_CHECK, &[]);
    let output = repo.gate();
    assert_passes(&output);
    assert!(stdout(&output).contains("pass hills\n"));
}

#[test]
fn the_newest_record_in_the_base_history_counts() {
    let older = record_with_proven(5);
    let newer = record_with_proven(1);
    let repo = Repo::new(
        "hills-newest",
        CHECK_ONE,
        &[("harness/ledger/1.toml", older.as_str())],
    );
    repo.git(&["checkout", "-q", "main"]);
    repo.write("harness/ledger/2.toml", &newer);
    repo.commit();
    repo.publish();
    repo.git(&["checkout", "-q", "work"]);
    repo.git(&["reset", "-q", "--hard", "main"]);
    assert_passes(&repo.gate());
}

#[test]
fn a_record_main_gained_later_is_not_the_base() {
    let later = record_with_proven(5);
    let repo = Repo::new("hills-later", CHECK_ONE, &[]);
    repo.git(&["checkout", "-q", "main"]);
    repo.write("harness/ledger/2.toml", &later);
    repo.commit();
    repo.publish();
    repo.git(&["checkout", "-q", "work"]);
    assert_passes(&repo.gate());
}

#[test]
fn a_deleted_record_no_longer_counts() {
    let kept = record_with_proven(1);
    let deleted = record_with_proven(5);
    let repo = Repo::new(
        "hills-deleted",
        CHECK_ONE,
        &[("harness/ledger/1.toml", kept.as_str())],
    );
    repo.git(&["checkout", "-q", "main"]);
    repo.write("harness/ledger/2.toml", &deleted);
    repo.commit();
    repo.git(&["rm", "-q", "harness/ledger/2.toml"]);
    repo.commit();
    repo.publish();
    repo.git(&["checkout", "-q", "work"]);
    repo.git(&["reset", "-q", "--hard", "main"]);
    assert_passes(&repo.gate());
}

#[test]
fn an_unreadable_base_record_is_a_problem() {
    let repo = Repo::new(
        "hills-bad",
        CHECK_ONE,
        &[("harness/ledger/1.toml", "issue = \"nope\"\n")],
    );
    let output = repo.gate();
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(
        stderr(&output).contains("harness/ledger/1.toml"),
        "{}",
        stderr(&output)
    );
}

const T2_MANIFEST: &str = "[package]\nname = \"t2\"\nversion = \"0.0.0\"\nedition = \"2024\"\n";
const T2_FAILS: &str = "#[test]\nfn planted_in_t2() {\n    panic!(\"planted\");\n}\n";
const AP_MANIFEST: &str = "[package]\nname = \"ap\"\nversion = \"0.0.0\"\nedition = \"2024\"\n";
const UPSTREAM_MANIFEST: &str =
    "[package]\nname = \"fake-upstream\"\nversion = \"0.0.0\"\nedition = \"2024\"\n";

/// A workspace whose `t2` package has a test that fails, so that a gate that
/// runs it fails `t2`.
fn with_failing_t2(name: &str) -> Repo {
    Repo::new(
        name,
        CHECK_ONE,
        &[
            ("crates/t2/Cargo.toml", T2_MANIFEST),
            ("crates/t2/src/lib.rs", T2_FAILS),
            ("crates/ap/Cargo.toml", AP_MANIFEST),
            ("crates/ap/src/lib.rs", NO_CHECK),
            ("crates/fake-upstream/Cargo.toml", UPSTREAM_MANIFEST),
            ("crates/fake-upstream/src/lib.rs", NO_CHECK),
        ],
    )
}

#[test]
fn a_change_outside_the_adapter_paths_leaves_t2_out() {
    let repo = with_failing_t2("t2-left-out");
    repo.write("crates/demo/src/lib.rs", &format!("{CHECK_ONE}{NO_CHECK}"));
    repo.write("crates/ap/src/lib.rs", "pub fn g() {}\n");
    repo.commit();
    let output = repo.gate();
    assert_passes(&output);
    assert!(stdout(&output).contains("skip t2\n"), "{}", stdout(&output));
    assert!(
        stderr(&output).contains("no change touches an adapter path"),
        "{}",
        stderr(&output)
    );
    assert!(
        !stderr(&output).contains("planted_in_t2"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn a_change_to_an_adapter_path_runs_t2() {
    let paths: Vec<(usize, &str)> = [
        "crates/ap/src/adapter/mod.rs",
        "crates/ap/src/lock.rs",
        "crates/t2/tests/probe.rs",
        "crates/fake-upstream/src/main.rs",
        "guest/os/pin.toml",
        "guest/kernel/config",
    ]
    .into_iter()
    .enumerate()
    .collect();
    common::each(&paths, |&(index, path)| {
        let repo = with_failing_t2(&format!("t2-runs-{index}"));
        repo.write(path, "// changed\n");
        repo.commit();
        assert_fails(&repo.gate(), "t2", "planted_in_t2");
    });
}

#[test]
fn an_untracked_file_on_an_adapter_path_runs_t2() {
    let repo = with_failing_t2("t2-untracked");
    repo.write("crates/t2/tests/new.rs", "#[test]\nfn new() {}\n");
    assert_fails(&repo.gate(), "t2", "planted_in_t2");
}

#[test]
fn an_uncommitted_edit_on_an_adapter_path_runs_t2() {
    let repo = with_failing_t2("t2-uncommitted");
    repo.write("crates/t2/src/lib.rs", &format!("{T2_FAILS}// edited\n"));
    assert_fails(&repo.gate(), "t2", "planted_in_t2");
}

/// A tree with no change from its base is `main` as it is, as the nightly job,
/// the runner gate, and the refresh's proof of `main` test it.
#[test]
fn a_tree_like_its_base_runs_t2() {
    let repo = with_failing_t2("t2-unchanged");
    assert_fails(&repo.gate(), "t2", "planted_in_t2");
}

/// An earlier attempt at a record may leave one in the ledger, as the nightly
/// job allows.
#[test]
fn a_tree_like_its_base_but_in_the_ledger_runs_t2() {
    let repo = with_failing_t2("t2-ledger");
    repo.write("harness/ledger/9.toml", &record_with_proven(0));
    assert_fails(&repo.gate(), "t2", "planted_in_t2");
}

#[test]
fn a_file_beside_an_adapter_file_leaves_t2_out() {
    let repo = with_failing_t2("t2-beside");
    repo.write("crates/ap/src/lock.rs.orig", "// beside\n");
    let output = repo.gate();
    assert_passes(&output);
    assert!(stdout(&output).contains("skip t2\n"), "{}", stdout(&output));
}

#[test]
fn without_origin_main_t2_runs() {
    let repo = Repo::files(
        "t2-no-origin",
        CHECK_ONE,
        &[
            ("crates/t2/Cargo.toml", T2_MANIFEST),
            ("crates/t2/src/lib.rs", T2_FAILS),
        ],
    );
    repo.git(&["init", "-q", "-b", "main"]);
    repo.commit();
    assert_fails(&repo.gate(), "t2", "planted_in_t2");
}

#[test]
fn without_a_base_t2_runs() {
    let repo = Repo::files(
        "t2-no-base",
        CHECK_ONE,
        &[
            ("crates/t2/Cargo.toml", T2_MANIFEST),
            ("crates/t2/src/lib.rs", T2_FAILS),
        ],
    );
    assert_fails(&repo.gate(), "t2", "planted_in_t2");
}

/// Sentence `Three.` has a check only in `t2`.
const T2_CHECKS_THREE: &str = "// spec(B): Three.\n#[test]\nfn three() {}\n";

#[test]
fn a_t2_check_counts_toward_the_spec_score_when_t2_runs() {
    let record = record_with_proven(2);
    let repo = Repo::new(
        "t2-counts",
        CHECK_ONE,
        &[
            ("crates/t2/Cargo.toml", T2_MANIFEST),
            ("crates/t2/src/lib.rs", T2_CHECKS_THREE),
            ("harness/ledger/1.toml", record.as_str()),
        ],
    );
    repo.write("crates/t2/tests/probe.rs", "// changed\n");
    repo.commit();
    let output = repo.gate();
    assert_passes(&output);
    for line in ["pass t2\n", "pass hills\n"] {
        assert!(stdout(&output).contains(line), "{}", stdout(&output));
    }
}

#[test]
fn a_t2_check_left_out_does_not_count() {
    let record = record_with_proven(2);
    let repo = Repo::new(
        "t2-left-out-count",
        CHECK_ONE,
        &[
            ("crates/t2/Cargo.toml", T2_MANIFEST),
            ("crates/t2/src/lib.rs", T2_CHECKS_THREE),
            ("harness/ledger/1.toml", record.as_str()),
        ],
    );
    repo.write("crates/demo/src/lib.rs", &format!("{CHECK_ONE}{NO_CHECK}"));
    repo.commit();
    let output = repo.gate();
    assert!(stdout(&output).contains("skip t2\n"), "{}", stdout(&output));
    assert_fails(&output, "hills", "from 2 to 1");
}
