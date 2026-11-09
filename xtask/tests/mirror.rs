//! Runs `mirror` against a fake `gh` that logs every call. Issue 7 is closed by
//! open PR 9. PR 8 closes another issue, and PR 6 closes an issue numbered 7 in
//! another repository. No test reaches GitHub.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

mod common;

const ISSUE: &str = "repos/{owner}/{repo}/issues";

fn closing(repository: &str, number: u64) -> String {
    format!("{{\"number\":{number},\"url\":\"https://github.com/{repository}/issues/{number}\"}}")
}

fn listed(number: u64, closes: &str) -> String {
    format!(
        "{{\"number\":{number},\"url\":\"https://github.com/o/r/pull/{number}\",\"closingIssuesReferences\":[{closes}]}}"
    )
}

fn labels_json(labels: &[&str]) -> String {
    let names: Vec<String> = labels
        .iter()
        .map(|label| format!("{{\"name\":\"{label}\"}}"))
        .collect();
    format!("[{}]", names.join(","))
}

struct Repo {
    root: PathBuf,
    bin: PathBuf,
}

impl Repo {
    fn new(name: &str) -> Repo {
        let parent = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("mirror")
            .join(name);
        if parent.exists() {
            fs::remove_dir_all(&parent).unwrap();
        }
        let repo = Repo {
            root: parent.join("repo"),
            bin: parent.join("bin"),
        };
        fs::create_dir_all(&repo.root).unwrap();
        fs::create_dir_all(&repo.bin).unwrap();
        fs::write(
            repo.root.join("Cargo.toml"),
            "[workspace]\nresolver = \"3\"\n",
        )
        .unwrap();
        repo.set_open_prs(&[
            listed(6, &closing("x/y", 7)),
            listed(8, &closing("o/r", 3)),
            listed(9, &closing("o/r", 7)),
        ]);
        repo.write_gh();
        repo
    }

    fn set_open_prs(&self, prs: &[String]) {
        fs::write(self.bin.join("prs.json"), format!("[{}]\n", prs.join(","))).unwrap();
    }

    fn set_issue(&self, labels: &[&str]) {
        fs::write(
            self.bin.join("item-7.json"),
            format!("{{\"number\":7,\"labels\":{}}}\n", labels_json(labels)),
        )
        .unwrap();
    }

    fn set_pr(&self, state: &str, labels: &[&str]) {
        fs::write(
            self.bin.join("item-9.json"),
            format!(
                "{{\"number\":9,\"labels\":{},\"pull_request\":{{\"url\":\"u\"}}}}\n",
                labels_json(labels)
            ),
        )
        .unwrap();
        fs::write(
            self.bin.join("pr.json"),
            format!(
                "{{\"number\":9,\"state\":\"{state}\",\"url\":\"https://github.com/o/r/pull/9\",\"isCrossRepository\":false,\"headRefName\":\"7-x\",\"baseRefName\":\"main\",\"body\":\"Closes #7.\",\"labels\":{},\"closingIssuesReferences\":[{}],\"reviews\":[]}}\n",
                labels_json(labels),
                closing("o/r", 7)
            ),
        )
        .unwrap();
    }

    /// Answers a delete with `delete-error`, if that file exists.
    fn write_gh(&self) {
        let bin = self.bin.display();
        let script = format!(
            r#"#!/bin/sh
echo "$*" >> '{bin}/calls'
case "$*" in
  "api -X DELETE "*)
    if [ -f '{bin}/delete-error' ]; then cat '{bin}/delete-error' >&2; exit 1; fi ;;
  "api -X POST "*) ;;
  "api {ISSUE}/"*) cat "{bin}/item-${{2##*/}}.json" ;;
  "pr view "*) cat '{bin}/pr.json' ;;
  "pr list "*) cat '{bin}/prs.json' ;;
  *) echo "unexpected gh call: $*" >&2; exit 3 ;;
esac
"#
        );
        let path = self.bin.join("gh");
        common::write_tool(&path, &script);
    }

    fn mirror(&self, args: &[&str]) -> Output {
        let path = format!("{}:{}", self.bin.display(), std::env::var("PATH").unwrap());
        Command::new(env!("CARGO_BIN_EXE_xtask"))
            .arg("mirror")
            .args(args)
            .current_dir(&self.root)
            .env("PATH", path)
            .output()
            .unwrap()
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.bin.join("calls")).unwrap_or_default()
    }

    fn writes(&self) -> Vec<String> {
        self.calls()
            .lines()
            .filter(|call| call.starts_with("api -X"))
            .map(str::to_string)
            .collect()
    }
}

#[track_caller]
fn assert_status(output: &Output, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn add(number: u64, label: &str) -> String {
    format!("api -X POST {ISSUE}/{number}/labels -f labels[]={label}")
}

fn remove(number: u64, label: &str) -> String {
    format!("api -X DELETE {ISSUE}/{number}/labels/{label}")
}

#[test]
fn a_label_added_to_an_issue_reaches_only_the_pr_that_closes_it() {
    for label in ["land-needed", "answer-needed"] {
        let repo = Repo::new(&format!("issue-add-{label}"));
        repo.set_issue(&[label]);
        assert_status(&repo.mirror(&["7", label, "labeled"]), 0);
        assert_eq!(repo.writes(), [add(7, label), add(9, label)]);
    }
}

#[test]
fn a_label_removed_from_an_issue_leaves_its_pr() {
    let repo = Repo::new("issue-remove");
    repo.set_issue(&[]);
    assert_status(&repo.mirror(&["7", "land-needed", "unlabeled"]), 0);
    assert_eq!(
        repo.writes(),
        [remove(7, "land-needed"), remove(9, "land-needed")]
    );
}

#[test]
fn a_label_added_to_a_pr_reaches_its_issue() {
    let repo = Repo::new("pr-add");
    repo.set_pr("OPEN", &["land-needed"]);
    assert_status(&repo.mirror(&["9", "land-needed", "labeled"]), 0);
    assert_eq!(
        repo.writes(),
        [add(9, "land-needed"), add(7, "land-needed")]
    );
}

#[test]
fn clearing_answer_needed_on_a_pr_clears_its_issue() {
    let repo = Repo::new("pr-remove");
    repo.set_pr("OPEN", &[]);
    assert_status(&repo.mirror(&["9", "answer-needed", "unlabeled"]), 0);
    assert_eq!(
        repo.writes(),
        [remove(9, "answer-needed"), remove(7, "answer-needed")]
    );
}

/// The issue was labeled, and the owner cleared the PR before that run
/// started. The issue's run still sees the label on the issue and puts it back
/// on the PR. The PR's run comes next, and has to clear both sides.
#[test]
fn a_label_cleared_while_an_older_run_waits_stays_cleared() {
    let repo = Repo::new("race");
    repo.set_issue(&["land-needed"]);
    repo.set_pr("OPEN", &[]);
    assert_status(&repo.mirror(&["7", "land-needed", "labeled"]), 0);
    repo.set_pr("OPEN", &["land-needed"]);
    fs::remove_file(repo.bin.join("calls")).unwrap();
    assert_status(&repo.mirror(&["9", "land-needed", "unlabeled"]), 0);
    assert_eq!(
        repo.writes(),
        [remove(9, "land-needed"), remove(7, "land-needed")]
    );
}

#[test]
fn an_issue_with_no_open_pr_keeps_the_label_alone() {
    let repo = Repo::new("no-pr");
    repo.set_issue(&["answer-needed"]);
    repo.set_open_prs(&[]);
    assert_status(&repo.mirror(&["7", "answer-needed", "labeled"]), 0);
    assert!(repo.writes().is_empty(), "{}", repo.calls());
}

#[test]
fn a_closed_pr_changes_nothing() {
    for state in ["CLOSED", "MERGED"] {
        let repo = Repo::new(&format!("closed-{state}"));
        repo.set_pr(state, &[]);
        assert_status(&repo.mirror(&["9", "land-needed", "unlabeled"]), 0);
        assert!(repo.writes().is_empty(), "{}", repo.calls());
    }
}

#[test]
fn no_other_label_is_copied() {
    for label in ["spec-change", "in-progress", "auto:merge", "land-needed2"] {
        let repo = Repo::new(&format!("other-{}", label.replace(':', "-")));
        assert_status(&repo.mirror(&["7", label, "labeled"]), 0);
        assert_eq!(repo.calls(), "");
    }
}

/// Another run or a person can get there first.
#[test]
fn removing_a_label_that_is_already_gone_passes() {
    let repo = Repo::new("already-gone");
    repo.set_issue(&[]);
    fs::write(
        repo.bin.join("delete-error"),
        "gh: Label does not exist (HTTP 404)\n",
    )
    .unwrap();
    assert_status(&repo.mirror(&["7", "land-needed", "unlabeled"]), 0);
}

#[test]
fn a_failed_write_is_an_error() {
    let repo = Repo::new("write-fails");
    repo.set_issue(&[]);
    fs::write(
        repo.bin.join("delete-error"),
        "gh: Resource not accessible (HTTP 403)\n",
    )
    .unwrap();
    assert_status(&repo.mirror(&["7", "land-needed", "unlabeled"]), 2);
}

#[test]
fn bad_arguments_are_errors_and_call_nothing() {
    let repo = Repo::new("bad-args");
    assert_status(&repo.mirror(&["seven", "land-needed", "labeled"]), 2);
    assert_status(&repo.mirror(&["7", "land-needed", "edited"]), 2);
    assert_status(&repo.mirror(&["7", "land-needed"]), 2);
    assert_status(&repo.mirror(&["7"]), 2);
    assert_eq!(repo.calls(), "");
}
