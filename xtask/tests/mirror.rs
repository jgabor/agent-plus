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
        repo.set_issue();
        repo.set_closers(&[(9, "o/r")]);
        repo.set_pr("OPEN");
        repo.write_gh();
        repo
    }

    fn set_open_prs(&self, prs: &[String]) {
        fs::write(self.bin.join("prs.json"), format!("[{}]\n", prs.join(","))).unwrap();
    }

    /// The PRs that GitHub links to issue 7 as closing it, open or not, as
    /// numbers and repositories.
    fn set_closers(&self, prs: &[(u64, &str)]) {
        let nodes: Vec<String> = prs
            .iter()
            .map(|(number, repository)| {
                format!(
                    "{{\"number\":{number},\"url\":\"https://github.com/{repository}/pull/{number}\"}}"
                )
            })
            .collect();
        fs::write(
            self.bin.join("closers.json"),
            format!(
                "{{\"data\":{{\"repository\":{{\"url\":\"https://github.com/o/r\",\"issue\":{{\"closedByPullRequestsReferences\":{{\"totalCount\":{},\"nodes\":[{}]}}}}}}}}}}\n",
                prs.len(),
                nodes.join(",")
            ),
        )
        .unwrap();
    }

    fn set_issue(&self) {
        fs::write(
            self.bin.join("item-7.json"),
            "{\"number\":7,\"labels\":[]}\n",
        )
        .unwrap();
    }

    fn set_pr(&self, state: &str) {
        fs::write(
            self.bin.join("item-9.json"),
            "{\"number\":9,\"labels\":[],\"pull_request\":{\"url\":\"u\"}}\n",
        )
        .unwrap();
        fs::write(
            self.bin.join("pr.json"),
            format!(
                "{{\"number\":9,\"state\":\"{state}\",\"url\":\"https://github.com/o/r/pull/9\",\"isCrossRepository\":false,\"headRefName\":\"7-x\",\"baseRefName\":\"main\",\"body\":\"Closes #7.\",\"labels\":[],\"closingIssuesReferences\":[{}],\"reviews\":[]}}\n",
                closing("o/r", 7)
            ),
        )
        .unwrap();
    }

    /// The lines `gh api` prints for an item's events after its `--jq` filter,
    /// such as `100 labeled`.
    fn set_changes(&self, number: u64, changes: &str) {
        fs::write(self.bin.join(format!("events-{number}")), changes).unwrap();
    }

    /// Answers a delete with `delete-error`, if that file exists. After one
    /// `pr view`, `pr-next.json` replaces `pr.json`, if it exists.
    fn write_gh(&self) {
        let bin = self.bin.display();
        let script = format!(
            r#"#!/bin/sh
echo "$*" >> '{bin}/calls'
case "$*" in
  "api -X DELETE "*)
    if [ -f '{bin}/delete-error' ]; then cat '{bin}/delete-error' >&2; exit 1; fi ;;
  "api -X POST "*) ;;
  "api {ISSUE}/"*"/events --paginate --jq "*)
    n=${{2%/events}}; cat "{bin}/events-${{n##*/}}" 2>/dev/null || true ;;
  "api {ISSUE}/"*) cat "{bin}/item-${{2##*/}}.json" ;;
  "api graphql "*) cat '{bin}/closers.json' ;;
  "pr view "*)
    cat '{bin}/pr.json'
    if [ -f '{bin}/pr-next.json' ]; then mv '{bin}/pr-next.json' '{bin}/pr.json'; fi ;;
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
        repo.set_changes(7, "100 labeled\n");
        assert_status(&repo.mirror(&["7", label]), 0);
        assert_eq!(repo.writes(), [add(7, label), add(9, label)]);
    }
}

#[test]
fn a_label_removed_from_an_issue_leaves_its_pr() {
    let repo = Repo::new("issue-remove");
    repo.set_changes(7, "100 labeled\n300 unlabeled\n");
    repo.set_changes(9, "200 labeled\n");
    assert_status(&repo.mirror(&["7", "land-needed"]), 0);
    assert_eq!(
        repo.writes(),
        [remove(7, "land-needed"), remove(9, "land-needed")]
    );
}

#[test]
fn a_label_added_to_a_pr_reaches_its_issue() {
    let repo = Repo::new("pr-add");
    repo.set_changes(9, "100 labeled\n");
    assert_status(&repo.mirror(&["9", "land-needed"]), 0);
    assert_eq!(
        repo.writes(),
        [add(9, "land-needed"), add(7, "land-needed")]
    );
}

#[test]
fn clearing_answer_needed_on_a_pr_clears_its_issue() {
    let repo = Repo::new("pr-remove");
    repo.set_changes(7, "100 labeled\n");
    repo.set_changes(9, "200 unlabeled\n");
    assert_status(&repo.mirror(&["9", "answer-needed"]), 0);
    assert_eq!(
        repo.writes(),
        [remove(9, "answer-needed"), remove(7, "answer-needed")]
    );
}

/// The issue was labeled, then the owner cleared the PR. The issue's run can
/// start before the PR's, after it, or again later, and each one clears both
/// sides.
#[test]
fn a_label_cleared_while_an_older_run_waits_stays_cleared() {
    let repo = Repo::new("race");
    repo.set_changes(7, "100 labeled\n");
    repo.set_changes(9, "200 unlabeled\n");
    for number in ["7", "9", "7"] {
        fs::write(repo.bin.join("calls"), "").unwrap();
        assert_status(&repo.mirror(&[number, "land-needed"]), 0);
        let (first, second) = if number == "7" { (7, 9) } else { (9, 7) };
        assert_eq!(
            repo.writes(),
            [remove(first, "land-needed"), remove(second, "land-needed")],
            "run for {number}"
        );
    }
}

/// The owner cleared the label on a PR that then closed, and a new PR
/// replaced it. A rerun of the issue's old run still sees that clear, though
/// it writes only to the open PR. A PR in another repository counts for
/// nothing here.
#[test]
fn a_label_cleared_on_a_replaced_pr_stays_cleared() {
    let repo = Repo::new("replaced");
    repo.set_changes(7, "100 labeled\n");
    repo.set_changes(5, "200 unlabeled\n");
    repo.set_changes(4, "300 labeled\n");
    repo.set_closers(&[(5, "o/r"), (4, "x/y"), (9, "o/r")]);
    assert_status(&repo.mirror(&["7", "land-needed"]), 0);
    assert_eq!(
        repo.writes(),
        [remove(7, "land-needed"), remove(9, "land-needed")]
    );
    assert!(
        !repo.calls().contains("issues/4/events"),
        "{}",
        repo.calls()
    );
}

/// GitHub lists at most 100 linked PRs in one page.
#[test]
fn too_many_linked_prs_are_an_error_and_write_nothing() {
    let repo = Repo::new("many-closers");
    repo.set_changes(7, "100 labeled\n");
    let prs: Vec<(u64, &str)> = (1000..1101).map(|n| (n, "o/r")).collect();
    repo.set_closers(&prs);
    assert_status(&repo.mirror(&["7", "land-needed"]), 2);
    assert!(repo.writes().is_empty(), "{}", repo.calls());
}

/// Only `mirror` itself has changed the label, and the bots' events are
/// filtered out.
#[test]
fn a_label_no_person_changed_is_left_alone() {
    let repo = Repo::new("no-changes");
    assert_status(&repo.mirror(&["7", "land-needed"]), 0);
    assert!(repo.writes().is_empty(), "{}", repo.calls());
}

#[test]
fn an_issue_with_no_open_pr_keeps_the_label_alone() {
    let repo = Repo::new("no-pr");
    repo.set_changes(7, "100 labeled\n");
    repo.set_open_prs(&[]);
    assert_status(&repo.mirror(&["7", "answer-needed"]), 0);
    assert!(repo.writes().is_empty(), "{}", repo.calls());
}

#[test]
fn a_pr_closed_without_merging_changes_nothing() {
    let repo = Repo::new("closed");
    repo.set_pr("CLOSED");
    repo.set_changes(9, "100 unlabeled\n");
    assert_status(&repo.mirror(&["9", "land-needed"]), 0);
    assert!(repo.writes().is_empty(), "{}", repo.calls());
}

/// Nothing waits on the owner once the PR has landed, whoever added the label
/// last.
#[test]
fn a_merged_pr_and_its_issue_lose_land_needed() {
    for changes in ["", "100 labeled\n", "100 unlabeled\n"] {
        let repo = Repo::new(&format!("merged-{}", changes.trim().replace(' ', "-")));
        repo.set_pr("MERGED");
        repo.set_changes(9, changes);
        assert_status(&repo.mirror(&["9", "land-needed"]), 0);
        assert_eq!(
            repo.writes(),
            [remove(9, "land-needed"), remove(7, "land-needed")],
            "changes {changes:?}"
        );
    }
}

/// A question can outlive the merge, so only the owner clears it.
#[test]
fn a_merged_pr_keeps_answer_needed_alone() {
    let repo = Repo::new("merged-answer");
    repo.set_pr("MERGED");
    repo.set_changes(9, "100 unlabeled\n");
    assert_status(&repo.mirror(&["9", "answer-needed"]), 0);
    assert!(repo.writes().is_empty(), "{}", repo.calls());
}

#[test]
fn no_other_label_is_copied() {
    for label in ["spec-change", "in-progress", "auto:merge", "land-needed2"] {
        let repo = Repo::new(&format!("other-{}", label.replace(':', "-")));
        assert_status(&repo.mirror(&["7", label]), 0);
        assert_eq!(repo.calls(), "");
    }
}

/// Another run or a person can get there first.
#[test]
fn removing_a_label_that_is_already_gone_passes() {
    let repo = Repo::new("already-gone");
    repo.set_changes(7, "100 unlabeled\n");
    fs::write(
        repo.bin.join("delete-error"),
        "gh: Label does not exist (HTTP 404)\n",
    )
    .unwrap();
    assert_status(&repo.mirror(&["7", "land-needed"]), 0);
}

#[test]
fn a_failed_write_is_an_error() {
    let repo = Repo::new("write-fails");
    repo.set_changes(7, "100 unlabeled\n");
    fs::write(
        repo.bin.join("delete-error"),
        "gh: Resource not accessible (HTTP 403)\n",
    )
    .unwrap();
    assert_status(&repo.mirror(&["7", "land-needed"]), 2);
}

#[test]
fn unreadable_events_are_an_error_and_write_nothing() {
    let repo = Repo::new("bad-events");
    repo.set_changes(7, "100 transferred\n");
    assert_status(&repo.mirror(&["7", "land-needed"]), 2);
    assert!(repo.writes().is_empty(), "{}", repo.calls());
}

#[test]
fn bad_arguments_are_errors_and_call_nothing() {
    let repo = Repo::new("bad-args");
    assert_status(&repo.mirror(&["seven", "land-needed"]), 2);
    assert_status(&repo.mirror(&["7"]), 2);
    assert_eq!(repo.calls(), "");
}

#[test]
fn a_merged_pr_that_closes_no_issue_still_loses_land_needed() {
    let repo = Repo::new("merged-alone");
    repo.set_pr("MERGED");
    let path = repo.bin.join("pr.json");
    let pr = fs::read_to_string(&path).unwrap();
    fs::write(&path, pr.replace(&closing("o/r", 7), "")).unwrap();
    assert_status(&repo.mirror(&["9", "land-needed"]), 0);
    assert_eq!(repo.writes(), [remove(9, "land-needed")]);
}

/// A run that saw the PR open can write after the merge's own run has
/// finished. It looks again, so the label doesn't come back.
#[test]
fn a_pr_that_merges_while_the_label_is_copied_loses_it() {
    let repo = Repo::new("merge-race-issue");
    repo.set_changes(7, "100 labeled\n");
    repo.set_pr("MERGED");
    assert_status(&repo.mirror(&["7", "land-needed"]), 0);
    assert_eq!(
        repo.writes(),
        [
            add(7, "land-needed"),
            add(9, "land-needed"),
            remove(7, "land-needed"),
            remove(9, "land-needed")
        ]
    );

    let repo = Repo::new("merge-race-pr");
    repo.set_changes(9, "100 labeled\n");
    repo.set_pr("MERGED");
    fs::rename(repo.bin.join("pr.json"), repo.bin.join("pr-next.json")).unwrap();
    repo.set_pr("OPEN");
    assert_status(&repo.mirror(&["9", "land-needed"]), 0);
    assert_eq!(
        repo.writes(),
        [
            add(9, "land-needed"),
            add(7, "land-needed"),
            remove(9, "land-needed"),
            remove(7, "land-needed")
        ]
    );
}
