use super::counts::with_base;
use super::prepare::{last_post, posts};
use super::slot::{assert_merged_after, merge_ten_during};
use super::timeout::{assert_stray_stopped, stray};
use super::*;

/// One row of `gh pr list`.
/// A row of `gh pr list` for a PR whose auto-merge is armed.
fn listed(number: u64, branch: &str, head: &str, closes: u64, fork: bool) -> String {
    format!(
        "{{\"number\":{number},\"title\":\"t\",\"labels\":[],\"url\":\"https://github.com/o/r/pull/{number}\",\"isCrossRepository\":{fork},\"headRefName\":{},\"headRefOid\":{},\"closingIssuesReferences\":[{{\"number\":{closes},\"url\":\"https://github.com/o/r/issues/{closes}\"}}],\"autoMergeRequest\":{ARMED}}}",
        quoted(branch),
        quoted(head)
    )
}

const ARMED: &str = "{\"mergeMethod\":\"SQUASH\"}";

/// The same row, for a PR the owner hasn't approved by arming auto-merge.
fn unarmed(row: &str) -> String {
    row.replace(ARMED, "null")
}

pub(super) fn merged(number: u64, head: &str) -> String {
    format!("gh pr merge {number} -R o/r --squash --delete-branch --match-head-commit {head}")
}

/// The call that closes an issue as completed.
fn closed(number: u64) -> String {
    format!(
        "gh api -X PATCH repos/{{owner}}/{{repo}}/issues/{number} -f state=closed -f state_reason=completed\n"
    )
}

fn unlabeled(number: u64) -> String {
    format!("gh api -X DELETE repos/{{owner}}/{{repo}}/issues/{number}/labels/land-needed")
}

const KEPT: &str = "version_ms = 12.5\n";
const GREEN: &str = "spec 0 of 2, journey 1 of 1";

fn status(head: &str, state: &str) -> String {
    format!(
        "gh api -X POST repos/{{owner}}/{{repo}}/statuses/{head} -f state={state} -f context=xtask/gate"
    )
}

impl Repo {
    /// The fake `gh pr view` reads each PR's head from its branch, and whether
    /// its auto-merge is armed from its row.
    fn set_open_prs(&self, rows: &[String]) {
        for row in rows {
            let field = |name: &str, end: char| {
                let start = row.find(name).unwrap() + name.len();
                row[start..].split(end).next().unwrap().to_string()
            };
            let number = field("\"number\":", ',');
            let branch = field("\"headRefName\":\"", '"');
            fs::write(self.bin.join(format!("branch-{number}")), branch).unwrap();
            if row.contains(ARMED) {
                fs::write(self.bin.join(format!("armed-{number}")), "").unwrap();
            }
        }
        fs::write(self.bin.join("prs.json"), format!("[{}]\n", rows.join(","))).unwrap();
    }

    /// A green `xtask/gate` on this commit, as a proof leaves it.
    fn set_green(&self, head: &str) {
        let status = format!(
            "{{\"statuses\":[{{\"context\":\"xtask/gate\",\"state\":\"success\",\"description\":\"{GREEN}\"}}]}}\n"
        );
        fs::write(self.bin.join(format!("status-{head}")), status).unwrap();
    }

    /// Which open issues have `land-needed`. Issue 2 has another label.
    fn set_land_needed(&self, issues: &[u64]) {
        let mut rows = vec![
            "{\"number\":2,\"title\":\"t\",\"labels\":[{\"name\":\"auto:pr\"}],\"isPinned\":false}"
                .to_string(),
        ];
        rows.extend(issues.iter().map(|number| {
            format!("{{\"number\":{number},\"title\":\"t\",\"labels\":[{{\"name\":\"land-needed\"}}],\"isPinned\":false}}")
        }));
        fs::write(
            self.bin.join("issues.json"),
            format!("[{}]\n", rows.join(",")),
        )
        .unwrap();
    }

    /// A branch for another PR, cut from `main`. `pr` is what `gh pr view` says.
    pub(super) fn other_pr(&self, number: u64, branch: &str, pr: &Pr) {
        self.git(&["checkout", "-q", "main"]);
        self.git(&["checkout", "-q", "-b", branch]);
        self.write(&format!("{branch}.txt"), "x\n");
        self.commit();
        self.git(&["push", "-q", "origin", branch]);
        self.git(&["checkout", "-q", BRANCH]);
        let json = pr_json(pr, pr.state).replace(BRANCH, branch);
        fs::write(self.bin.join(format!("pr-{number}.json")), json).unwrap();
        fs::write(self.bin.join(format!("branch-{number}")), branch).unwrap();
    }

    /// Adds the record that an earlier `prepare` leaves at the tip of a branch.
    fn prepared(&self, branch: &str, issue: u64, reason: &str) {
        self.git(&["checkout", "-q", branch]);
        let record =
            record(0, 1, KEPT).replace("reason = \"\"", &format!("reason = {}", quoted(reason)));
        self.write(&format!("harness/ledger/{issue}.toml"), &record);
        self.commit();
        self.git(&["push", "-q", "origin", branch]);
        self.git(&["checkout", "-q", BRANCH]);
    }

    pub(super) fn head_of(&self, branch: &str) -> String {
        self.origin(&["rev-parse", &format!("refs/heads/{branch}")])
    }

    /// From a checkout of `main`, as the workflow runs it.
    fn refresh(&self) -> Output {
        self.git(&["checkout", "-q", "main"]);
        self.xtask(&["refresh"])
    }
}

#[test]
fn a_refresh_proves_and_merges_each_armed_land_needed_pr_oldest_first() {
    let repo = Repo::new("refresh");
    repo.other_pr(
        5,
        "8-y",
        &Pr {
            closes: &[8],
            ..Pr::default()
        },
    );
    repo.other_pr(
        6,
        "2-z",
        &Pr {
            closes: &[2],
            ..Pr::default()
        },
    );
    repo.prepared(BRANCH, 7, "");
    repo.prepared("8-y", 8, "");
    repo.advance_main("later.txt", "x\n");
    // Cut from the new `main`, so it isn't behind, and never proven.
    repo.other_pr(
        4,
        "6-up",
        &Pr {
            closes: &[6],
            ..Pr::default()
        },
    );
    repo.prepared("6-up", 6, "");
    let before: Vec<String> = [BRANCH, "8-y", "2-z", "6-up"]
        .iter()
        .map(|branch| repo.head_of(branch))
        .collect();
    repo.set_open_prs(&[
        listed(9, BRANCH, &before[0], 7, false),
        unarmed(&listed(4, "6-up", &before[3], 6, false)),
        listed(6, "2-z", &before[2], 2, false),
        listed(5, "8-y", &before[1], 8, false),
    ]);
    repo.set_land_needed(&[6, 7, 8]);
    repo.forget_calls();

    let output = repo.refresh();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    let calls = repo.calls();
    let at = |text: &str| {
        calls
            .find(text)
            .unwrap_or_else(|| panic!("no `{text}` in:\n{calls}"))
    };
    let green_five = at(&status(&repo.head_of("8-y"), "success"));
    let green_nine = at(&status(&repo.head_of(BRANCH), "success"));
    assert!(at(&status(&before[1], "pending")) < at(&status(&before[0], "pending")));
    at(&format!(
        "{} -f description=Queued, next\n",
        status(&before[1], "pending")
    ));
    at(&format!(
        "{} -f description=Queued, 1 PR ahead\n",
        status(&before[0], "pending")
    ));
    assert!(at(&status(&before[0], "pending")) < green_five, "{calls}");
    let merged_five = at(&merged(5, &repo.head_of("8-y")));
    assert!(
        green_five < merged_five && merged_five < green_nine,
        "{calls}"
    );
    assert!(
        green_nine < at(&merged(9, &repo.head_of(BRANCH))),
        "{calls}"
    );
    // Its `mirror-labels` run doesn't start for a merge by the workflow.
    for number in [5, 8, 9, 7] {
        assert!(merged_five < at(&unlabeled(number)), "{calls}");
    }
    // Nor does GitHub close the issue of a PR that the workflow merged. No
    // other issue is closed, or changed in any other way.
    for issue in [8, 7] {
        assert!(at(&unlabeled(issue)) < at(&closed(issue)), "{calls}");
    }
    let to_issues: String = calls
        .lines()
        .filter(|call| {
            call.starts_with("gh api -X ") && call.contains("/issues/") && !call.contains("/labels")
        })
        .map(|call| format!("{call}\n"))
        .collect();
    assert_eq!(to_issues, closed(8) + &closed(7), "{calls}");
    for (number, branch, head) in [(6, "2-z", &before[2]), (4, "6-up", &before[3])] {
        assert_eq!(&repo.head_of(branch), head);
        assert!(!calls.contains(&format!("statuses/{head}")), "{calls}");
        assert!(!calls.contains(&format!("gh pr merge {number}")), "{calls}");
        assert!(!calls.contains(&unlabeled(number)), "{calls}");
    }
    assert!(!calls.contains("pr comment"), "{calls}");
    let out = stdout(&output);
    let line = |text: &str| {
        out.find(&format!("{text}\n"))
            .unwrap_or_else(|| panic!("no `{text}` in:\n{out}"))
    };
    assert!(line("refresh: proving PR 5 (1 of 2)") < line("refresh: proving PR 9 (2 of 2)"));
    // Outside Actions, a fold marker is noise.
    assert!(!out.contains("::"), "{out}");
    // Every PR was proven, so `main` needn't be.
    assert!(!out.contains("proving `main`"), "{out}");
}

/// In Actions, each proof folds under its PR, and the run's page sums them up.
#[test]
fn a_refresh_in_actions_folds_each_proof_and_sums_them_up() {
    let repo = Repo::new("refresh-actions");
    repo.other_pr(
        5,
        "8-y",
        &Pr {
            closes: &[8],
            reviews: &[("a", "CHANGES_REQUESTED")],
            ..Pr::default()
        },
    );
    repo.prepared(BRANCH, 7, "");
    repo.prepared("8-y", 8, "");
    repo.advance_main("later.txt", "x\n");
    repo.other_pr(
        4,
        "6-up",
        &Pr {
            closes: &[6],
            ..Pr::default()
        },
    );
    let (nine, five, four) = (
        repo.head_of(BRANCH),
        repo.head_of("8-y"),
        repo.head_of("6-up"),
    );
    repo.set_open_prs(&[
        listed(5, "8-y", &five, 8, false),
        listed(9, BRANCH, &nine, 7, false).replace("\"title\":\"t\"", "\"title\":\"Nine\""),
        listed(4, "6-up", &four, 6, false),
    ]);
    repo.set_land_needed(&[6, 7, 8]);
    repo.set_green(&four);
    let summary = repo.temp.join("summary.md");
    fs::write(&summary, "earlier\n").unwrap();
    repo.git(&["checkout", "-q", "main"]);
    let output = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .arg("refresh")
        .env("GITHUB_ACTIONS", "true")
        .env("GITHUB_STEP_SUMMARY", &summary)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    let line = |text: &str| {
        out.find(&format!("{text}\n"))
            .unwrap_or_else(|| panic!("no `{text}` in:\n{out}"))
    };
    let proving = line("refresh: proving PR 9 (2 of 2)");
    let opened = line("::group::PR 9 (2 of 2): Nine");
    let proved = out.find("prepare: PR 9 is ready to merge").expect(&out);
    assert!(line("::group::PR 5 (1 of 2): t") < proving, "{out}");
    assert!(proving < opened && opened < proved, "{out}");
    assert!(proved < out.rfind("::endgroup::\n").unwrap(), "{out}");
    // PR 5 failed, so `main` was proven in a fold of its own.
    assert!(
        line("::group::PR 5 (1 of 2): t") < line("::group::`main`"),
        "{out}"
    );
    assert!(line("::group::`main`") < proving, "{out}");
    assert_eq!(out.matches("::group::").count(), 3, "{out}");
    assert_eq!(out.matches("::endgroup::").count(), 3, "{out}");

    let summary = fs::read_to_string(&summary).unwrap();
    let rows: Vec<&str> = summary.lines().collect();
    assert_eq!(
        rows[..4],
        [
            "earlier",
            "",
            "| PR | Verdict | Status | Took |",
            "|---|---|---|---|"
        ]
    );
    assert_eq!(rows[4], format!("| #4 | merged | {GREEN} | |"), "{summary}");
    assert!(
        rows[5].starts_with("| #5 | not proven | Not proven | "),
        "{summary}"
    );
    assert!(rows[6].starts_with("| #9 | merged | spec "), "{summary}");
    let calls = repo.calls();
    // A green head on the newest `main` needs no proof.
    assert!(calls.contains(&merged(4, &four)), "{calls}");
    assert!(!calls.contains(&format!("statuses/{four}")), "{calls}");
    for row in &rows[5..] {
        assert!(row.ends_with("s |"), "{summary}");
    }
    assert_eq!(rows.len(), 7, "{summary}");
}

#[test]
fn a_failed_refresh_turns_that_pr_red_and_goes_on_to_the_next() {
    let repo = Repo::new("refresh-fails");
    repo.other_pr(
        5,
        "8-y",
        &Pr {
            closes: &[8],
            reviews: &[("a", "CHANGES_REQUESTED")],
            ..Pr::default()
        },
    );
    repo.prepared(BRANCH, 7, "");
    repo.prepared("8-y", 8, "");
    repo.advance_main("later.txt", "x\n");
    let (nine, five) = (repo.head_of(BRANCH), repo.head_of("8-y"));
    repo.set_open_prs(&[
        listed(5, "8-y", &five, 8, false),
        listed(9, BRANCH, &nine, 7, false),
    ]);
    repo.set_land_needed(&[7, 8]);

    let output = repo.refresh();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    let calls = repo.calls();
    // Its `prepare` refused it before the fetch, and so said nothing.
    let unproven = format!("{} -f description=Not proven\n", status(&five, "failure"));
    assert!(calls.contains(&unproven), "{calls}");
    assert_eq!(repo.head_of("8-y"), five);
    assert!(!calls.contains("gh pr merge 5"), "{calls}");
    // The dispatcher restarts its issue, and its auto-merge stays armed.
    assert!(calls.contains("gh pr comment 5 --body-file -"), "{calls}");
    assert!(
        repo.pr_comments().contains("Not proven"),
        "{}",
        repo.pr_comments()
    );
    assert!(calls.contains(&unlabeled(8)), "{calls}");
    assert!(calls.contains(&unlabeled(5)), "{calls}");
    assert!(!calls.contains("--disable-auto"), "{calls}");
    assert!(
        calls.contains(&status(&repo.head_of(BRANCH), "success")),
        "{calls}"
    );
    assert!(calls.contains(&merged(9, &repo.head_of(BRANCH))), "{calls}");
    // `main` was proven, and was sound, so the PR was handed back.
    let out = stdout(&output);
    assert_eq!(out.matches("refresh: proving `main`").count(), 1, "{out}");
}

/// A machine that can't prove `main` either says nothing about the PR, so the
/// refresh hands no PR back, and the next run proves them again.
#[test]
fn a_refresh_that_can_not_prove_main_hands_no_pr_back() {
    let repo = Repo::new("refresh-main-fails");
    repo.other_pr(
        5,
        "8-y",
        &Pr {
            closes: &[8],
            reviews: &[("a", "CHANGES_REQUESTED")],
            ..Pr::default()
        },
    );
    repo.prepared(BRANCH, 7, "");
    repo.prepared("8-y", 8, "");
    repo.set_pr(&Pr {
        reviews: &[("a", "CHANGES_REQUESTED")],
        ..Pr::default()
    });
    repo.advance_main("crates/demo/src/lib.rs", "pub fn f(  ) {}\n");
    let (nine, five) = (repo.head_of(BRANCH), repo.head_of("8-y"));
    repo.set_open_prs(&[
        listed(5, "8-y", &five, 8, false),
        listed(9, BRANCH, &nine, 7, false),
    ]);
    repo.set_land_needed(&[7, 8]);
    let summary = repo.temp.join("summary.md");
    repo.git(&["checkout", "-q", "main"]);
    let output = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .arg("refresh")
        .env("GITHUB_ACTIONS", "true")
        .env("GITHUB_STEP_SUMMARY", &summary)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    // Once per run, however many PRs fail.
    assert_eq!(out.matches("refresh: proving `main`").count(), 1, "{out}");
    let said = stderr(&output);
    let why = "refresh: `main` can't be proven on this machine: Failed: format";
    assert!(said.contains(why), "{said}");
    let calls = repo.calls();
    for number in [5, 8, 9, 7] {
        assert!(!calls.contains(&unlabeled(number)), "{calls}");
    }
    assert!(!calls.contains("pr comment"), "{calls}");
    assert!(!calls.contains("--disable-auto"), "{calls}");
    let summary = fs::read_to_string(&summary).unwrap();
    assert!(
        summary.contains("`main` can't be proven on this machine: Failed: format"),
        "{summary}"
    );
}

/// A gate of `main` that hangs would hold the refresh, and every refresh after
/// it, for good. It stops at the merge timeout, with what it started, and that is
/// a `main` the machine can't prove.
#[test]
fn a_refresh_whose_gate_of_main_hangs_stops_it_at_the_timeout() {
    let repo = Repo::new("refresh-main-hangs");
    repo.prepared(BRANCH, 7, "");
    repo.set_pr(&Pr {
        reviews: &[("a", "CHANGES_REQUESTED")],
        ..Pr::default()
    });
    repo.advance_main("later.txt", "x\n");
    let head = repo.head_of(BRANCH);
    repo.set_open_prs(&[listed(9, BRANCH, &head, 7, false)]);
    repo.set_land_needed(&[7]);
    repo.git(&["checkout", "-q", "main"]);
    // `prepare` refuses the PR before the fetch, so only the gate of `main`
    // asks for the formatter, which is the first thing it does. That call
    // starts the stray, and then hangs for far longer than the timeout.
    let started = Instant::now();
    let output = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .arg("refresh")
        .env("RUN_ON", "cargo fmt --version")
        .env("RUN_COMMAND", stray(&repo))
        .env("HANG_ON", "cargo fmt --version")
        .env("HANG_FOR", "60")
        .env("XTASK_MERGE_TIMEOUT_SECONDS", "10")
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(started.elapsed() < Duration::from_secs(30), "{output:?}");
    let said = stderr(&output);
    let why =
        "refresh: `main` can't be proven on this machine: its gate ran past the 10 second timeout";
    assert!(said.contains(why), "{said}");
    // The timeout stopped the hang, and not a gate that was only slow.
    assert!(repo.bin.join("hung").exists(), "{said}");
    assert_stray_stopped(&repo);
    let calls = repo.calls();
    for number in [9, 7] {
        assert!(!calls.contains(&unlabeled(number)), "{calls}");
    }
    assert!(!calls.contains("pr comment"), "{calls}");
}

/// A token that can't write issues can still unlabel the PR, and the comment
/// says only what was done, once for each failed head.
#[test]
fn a_refresh_that_can_not_unlabel_the_issue_unlabels_the_pr_and_comments_once() {
    let repo = Repo::new("refresh-refused");
    repo.prepared(BRANCH, 7, "");
    repo.advance_main("later.txt", "x\n");
    repo.set_open_prs(&[listed(9, BRANCH, &repo.head(), 7, false)]);
    repo.set_land_needed(&[7]);
    repo.set_pr(&Pr {
        reviews: &[("a", "CHANGES_REQUESTED")],
        ..Pr::default()
    });
    fs::write(repo.bin.join("refuse-label-7"), "").unwrap();

    for run in 1..=2 {
        let output = repo.refresh();
        assert_eq!(
            output.status.code(),
            Some(1),
            "run {run}: {}{}",
            stdout(&output),
            stderr(&output)
        );
        let said = stderr(&output);
        assert!(
            said.contains(&format!("`{}` failed", unlabeled(7))),
            "{said}"
        );
        assert!(said.contains("HTTP 403"), "{said}");
    }
    let calls = repo.calls();
    assert_eq!(calls.matches("gh pr comment 9").count(), 1, "{calls}");
    let line = |call: &str| {
        calls
            .find(call)
            .unwrap_or_else(|| panic!("{call}: {calls}"))
    };
    assert!(line(&unlabeled(9)) < line("gh pr comment 9"), "{calls}");
    assert!(line(&unlabeled(7)) < line("gh pr comment 9"), "{calls}");
    let comments = repo.pr_comments();
    assert!(comments.contains("Not proven"), "{comments}");
    assert!(!comments.contains("no longer has"), "{comments}");
}

/// A comment that can't be posted hides none of the labels' failures.
#[test]
fn a_refresh_names_each_failure_of_a_hand_back() {
    let repo = Repo::new("refresh-refused-all");
    repo.prepared(BRANCH, 7, "");
    repo.advance_main("later.txt", "x\n");
    repo.set_open_prs(&[listed(9, BRANCH, &repo.head(), 7, false)]);
    repo.set_land_needed(&[7]);
    repo.set_pr(&Pr {
        reviews: &[("a", "CHANGES_REQUESTED")],
        ..Pr::default()
    });
    for name in ["refuse-label-7", "refuse-label-9", "comment-fails"] {
        fs::write(repo.bin.join(name), "").unwrap();
    }

    let output = repo.refresh();
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    let said = stderr(&output);
    for call in [unlabeled(9), unlabeled(7)] {
        assert!(said.contains(&format!("`{call}` failed")), "{said}");
    }
    assert!(said.contains("no comment (HTTP 403)"), "{said}");
}

/// A fork's branch isn't in this repository, and `prepare` refuses it.
#[test]
fn a_refresh_runs_nothing_from_a_fork() {
    let repo = Repo::new("refresh-fork");
    repo.set_pr(&Pr {
        fork: true,
        ..Pr::default()
    });
    let head = "a".repeat(40);
    repo.set_open_prs(&[listed(9, "patch", &head, 7, true)]);
    repo.set_land_needed(&[7]);
    repo.forget_calls();

    let output = repo.refresh();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    let calls = repo.calls();
    assert!(calls.contains(&status(&head, "pending")), "{calls}");
    let unproven = format!("{} -f description=Not proven\n", status(&head, "failure"));
    assert!(calls.contains(&unproven), "{calls}");
    // Only `main` is checked out, to tell the machine's faults from the PR's.
    let added: Vec<&str> = calls
        .lines()
        .filter(|l| l.contains("worktree add"))
        .collect();
    assert_eq!(added.len(), 1, "{calls}");
    assert!(added[0].contains("xtask-main-"), "{calls}");
}

#[test]
fn a_refresh_keeps_the_branchs_budgets_and_runs_no_bench() {
    let base = record(0, 1, "version_ms = 99\n");
    let repo = Repo::with(
        "refresh-budgets",
        &[("harness/ledger/3.toml", base.as_str())],
        &[("crates/ap/src/main.rs", SLOW)],
    );
    repo.prepared(BRANCH, 7, "worth it");
    repo.advance_main("later.txt", "x\n");
    repo.set_open_prs(&[listed(9, BRANCH, &repo.head(), 7, false)]);
    repo.set_land_needed(&[7]);

    let output = repo.refresh();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(!stdout(&output).contains("bench:"), "{}", stdout(&output));
    let record = repo.origin(&["show", &format!("{}:harness/ledger/7.toml", repo.head())]);
    assert!(record.contains(KEPT.trim_end()), "{record}");
    assert!(record.contains("reason = \"worth it\"\n"), "{record}");
    let main = repo.origin(&["rev-parse", "refs/heads/main"]);
    assert!(record.contains(&format!("base = \"{main}\"\n")), "{record}");
}

/// A kernel that the fixture's tests fetch where a record reads it.
fn fetching(kernel: &str) -> String {
    format!(
        "pub fn f() {{}}\n\n#[test]\nfn fetch() {{\n    let guest = concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/../../target/guest\");\n    std::fs::create_dir_all(guest).unwrap();\n    std::fs::write(format!(\"{{guest}}/vmlinux\"), {kernel:?}).unwrap();\n}}\n"
    )
}

/// A pinned size is the same on every machine, so it follows the rebased tree.
#[test]
fn a_kept_record_has_the_sizes_that_the_rebased_tree_pins() {
    let (old, new) = ("an old kernel\n", "a new kernel, larger\n");
    let pin = |sha256: &str| format!("version = \"1\"\nsha256 = \"{sha256}\"\n");
    let budgets = format!("{BUDGETS}\n[[budget]]\nkey = \"kernel_bytes\"\nname = \"Kernel\"\n");
    let base = record(0, 1, "");
    let (old_pin, old_fetch) = (
        pin("cb0e1c77d92bf650037e5a22dd2e52e8776a5aa30aab83ff2caa6774fc6ba764"),
        fetching(old),
    );
    let repo = Repo::with(
        "prepare-kept-pinned",
        &[
            ("harness/ledger/3.toml", base.as_str()),
            ("harness/budgets.toml", budgets.as_str()),
            ("guest/kernel/pin.toml", old_pin.as_str()),
            ("crates/demo/src/lib.rs", old_fetch.as_str()),
        ],
        &[("docs.txt", "x\n")],
    );
    let kept = format!("kernel_bytes = {}\n{KEPT}", old.len());
    repo.change_branch("harness/ledger/7.toml", &record(0, 1, &kept));
    repo.advance_main(
        "guest/kernel/pin.toml",
        &pin("083c89b4717f7e0be368ddf18af5cc26fb24ccd6c4e7f070226d37225b35b1b2"),
    );
    repo.advance_main("crates/demo/src/lib.rs", &fetching(new));

    let output = repo.xtask(&["prepare", "9", "--keep-budgets"]);
    assert_prepared(&repo, &output);
    let record = repo.origin(&["show", &format!("{}:harness/ledger/7.toml", repo.head())]);
    let budgets = format!(
        "\n[budgets]\nkernel_bytes = {}\n{KEPT}\n[seconds]\n",
        new.len()
    );
    assert!(record.contains(&budgets), "{record}");
}

/// Commits after the record mean that it doesn't describe this branch.
#[test]
fn a_refresh_with_no_record_at_the_tip_fails_and_pushes_nothing() {
    let repo = Repo::new("refresh-no-record");
    repo.prepared(BRANCH, 7, "");
    repo.change_branch("more.txt", "x\n");
    repo.advance_main("later.txt", "x\n");
    let head = repo.head();
    repo.set_open_prs(&[listed(9, BRANCH, &head, 7, false)]);
    repo.set_land_needed(&[7]);

    let output = repo.refresh();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("no ledger record"),
        "{}",
        stderr(&output)
    );
    assert_eq!(repo.head(), head);
    // `prepare` refused the PR in words, so `main` isn't proven for it.
    let out = stdout(&output);
    assert!(!out.contains("proving `main`"), "{out}");
    // `prepare` said why, and `refresh` leaves that standing.
    let failures: Vec<(String, String)> = posts(&repo)
        .into_iter()
        .filter(|(_, said)| said.starts_with("failure"))
        .collect();
    let why = "the tip of PR 9 has no ledger record, so there are no budgets to keep. Run `cargo xtask prepare 9`";
    assert_eq!(failures, [(head, format!("failure {why}"))]);
    assert!(repo.pr_comments().contains(why), "{}", repo.pr_comments());
    let calls = repo.calls();
    assert!(calls.contains(&unlabeled(7)), "{calls}");
    assert!(calls.contains(&unlabeled(9)), "{calls}");
    assert!(!calls.contains("gh pr merge"), "{calls}");
}

/// A `main` that can't be proven excuses only the PRs that a check or an
/// error failed. One that `prepare` refused in words is handed back.
#[test]
fn a_refresh_that_can_not_prove_main_still_hands_back_a_pr_refused_in_words() {
    let repo = Repo::new("refresh-main-fails-in-words");
    repo.other_pr(
        5,
        "8-y",
        &Pr {
            closes: &[8],
            reviews: &[("a", "CHANGES_REQUESTED")],
            ..Pr::default()
        },
    );
    repo.prepared(BRANCH, 7, "");
    repo.prepared("8-y", 8, "");
    repo.change_branch("more.txt", "x\n");
    repo.advance_main("crates/demo/src/lib.rs", "pub fn f(  ) {}\n");
    let (nine, five) = (repo.head_of(BRANCH), repo.head_of("8-y"));
    repo.set_open_prs(&[
        listed(5, "8-y", &five, 8, false),
        listed(9, BRANCH, &nine, 7, false),
    ]);
    repo.set_land_needed(&[7, 8]);

    let output = repo.refresh();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert_eq!(out.matches("refresh: proving `main`").count(), 1, "{out}");
    let said = stderr(&output);
    let why = "refresh: `main` can't be proven on this machine: Failed: format";
    assert!(said.contains(why), "{said}");
    let calls = repo.calls();
    for number in [5, 8] {
        assert!(!calls.contains(&unlabeled(number)), "{calls}");
    }
    for number in [9, 7] {
        assert!(calls.contains(&unlabeled(number)), "{calls}");
    }
    let comments = repo.pr_comments();
    assert!(comments.contains("no ledger record"), "{comments}");
    assert!(!comments.contains("Not proven"), "{comments}");
}

/// A `prepare` that was stopped after it posted the green did prove the PR.
#[test]
fn a_refresh_leaves_a_green_that_its_prepare_posted() {
    let repo = Repo::new("refresh-green");
    repo.prepared(BRANCH, 7, "");
    repo.advance_main("later.txt", "x\n");
    repo.set_open_prs(&[listed(9, BRANCH, &repo.head(), 7, false)]);
    repo.set_land_needed(&[7]);
    repo.git(&["checkout", "-q", "main"]);
    let summary = repo.temp.join("summary.md");

    let output = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .arg("refresh")
        .env("KILL_ON", "state=success")
        .env("KILL_WHEN", "after")
        .env("GITHUB_ACTIONS", "true")
        .env("GITHUB_STEP_SUMMARY", &summary)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(repo.bin.join("killed").exists());
    assert_eq!(
        posts(&repo).pop().unwrap(),
        (
            repo.head(),
            "success spec 0 of 2, journey 1 of 1".to_string()
        )
    );
    let summary = fs::read_to_string(&summary).unwrap();
    let row = "| #9 | merged | spec 0 of 2, journey 1 of 1 | ";
    assert!(summary.contains(row), "{summary}");
    assert!(
        repo.calls().contains(&merged(9, &repo.head())),
        "{}",
        repo.calls()
    );
}

/// The queue position only says what is happening, as any `pending` does.
#[test]
fn a_refresh_proves_a_pr_whose_queue_position_can_not_be_posted() {
    let repo = Repo::new("refresh-no-pending");
    repo.prepared(BRANCH, 7, "");
    repo.advance_main("later.txt", "x\n");
    repo.set_open_prs(&[listed(9, BRANCH, &repo.head(), 7, false)]);
    repo.set_land_needed(&[7]);
    fs::write(repo.bin.join("pending-fails"), "").unwrap();

    let output = repo.refresh();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    assert_eq!(last_post(&repo), "success spec 0 of 2, journey 1 of 1");
}

/// Nothing proves a waiting PR before the owner approves it by arming
/// auto-merge.
#[test]
fn a_refresh_leaves_a_pr_alone_until_its_auto_merge_is_armed() {
    let repo = Repo::new("refresh-unarmed");
    repo.prepared(BRANCH, 7, "");
    repo.advance_main("later.txt", "x\n");
    let head = repo.head();
    repo.set_open_prs(&[unarmed(&listed(9, BRANCH, &head, 7, false))]);
    repo.set_land_needed(&[7]);

    let output = repo.refresh();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    assert_eq!(repo.head(), head);
    assert_eq!(posts(&repo), []);
    let calls = repo.calls();
    assert!(!calls.contains("gh pr merge"), "{calls}");
    assert!(!calls.contains("/labels/"), "{calls}");
}

/// An armed PR on the newest `main` that was never proven, or whose proof
/// failed, is proven again.
#[test]
fn a_refresh_proves_an_armed_pr_that_is_up_to_date_but_not_green() {
    let repo = Repo::new("refresh-not-green");
    repo.prepared(BRANCH, 7, "");
    let head = repo.head();
    repo.set_open_prs(&[listed(9, BRANCH, &head, 7, false)]);
    repo.set_land_needed(&[7]);

    let output = repo.refresh();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    assert_eq!(last_post(&repo), format!("success {GREEN}"));
    assert!(
        repo.calls().contains(&merged(9, &repo.head())),
        "{}",
        repo.calls()
    );
}

impl Repo {
    /// A refresh of PR 9, behind `main` and armed, with its issue waiting.
    fn armed_behind(name: &str) -> Repo {
        let repo = Repo::new(name);
        repo.prepared(BRANCH, 7, "");
        repo.advance_main("later.txt", "x\n");
        repo.set_open_prs(&[listed(9, BRANCH, &repo.head(), 7, false)]);
        repo.set_land_needed(&[7]);
        repo.git(&["checkout", "-q", "main"]);
        repo
    }

    /// Holds a lock in the repository's lock directory, as another run would.
    fn hold(&self, name: &str) -> fs::File {
        let directory = self.root.join(".git/xtask");
        fs::create_dir_all(&directory).unwrap();
        let held = fs::File::create(directory.join(name)).unwrap();
        held.lock().unwrap();
        held
    }

    /// A refresh that waits for the merge slot, which another run holds while
    /// `meanwhile` runs.
    fn refresh_waiting(&self, meanwhile: impl FnOnce()) -> Output {
        let held = self.hold("merge.lock");
        let said = self.temp.join("refresh.err");
        let child = self
            .command(env!("CARGO_BIN_EXE_xtask"))
            .arg("refresh")
            .stdout(Stdio::piped())
            .stderr(fs::File::create(&said).unwrap())
            .spawn()
            .unwrap();
        let waits = "refresh: PR 9 waits for the merge slot, which another run holds\n";
        let deadline = Instant::now() + Duration::from_secs(60);
        while !fs::read_to_string(&said).unwrap().contains(waits) {
            assert!(Instant::now() < deadline, "{}", self.calls());
            thread::sleep(Duration::from_millis(10));
        }
        meanwhile();
        held.unlock().unwrap();
        let mut output = child.wait_with_output().unwrap();
        output.stderr = fs::read(&said).unwrap();
        output
    }
}

/// What a queued PR shows once the refresh waits for the merge slot.
fn waited_on(head: &str) -> [(String, String); 2] {
    ["pending Queued, next", "pending Waiting: the merge slot"]
        .map(|text| (head.to_string(), text.to_string()))
}

/// The refresh takes the merge slot before it proves a PR, as a worker's merge
/// does, so one lock orders both.
#[test]
fn a_refresh_waits_for_the_merge_slot() {
    let repo = Repo::armed_behind("refresh-slot");
    let head = repo.head();
    let output = repo.refresh_waiting(|| {
        // It says so before it waits, so give it time to go on if it didn't.
        thread::sleep(Duration::from_millis(500));
        let calls = repo.calls();
        assert!(!calls.contains("gh pr view"), "{calls}");
        assert!(!calls.contains("git worktree add"), "{calls}");
        assert_eq!(posts(&repo), waited_on(&head));
    });
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    let calls = repo.calls();
    assert!(calls.contains(&merged(9, &repo.head())), "{calls}");
}

/// A PR that changed while the refresh waited for the slot is left as it is,
/// and the run still passes.
fn assert_left(repo: &Repo, output: &Output, head: &str, why: &str) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}{}",
        stdout(output),
        stderr(output)
    );
    let line = format!("refresh: PR 9 is left as it is, because {why}\n");
    assert!(stdout(output).contains(&line), "{}", stdout(output));
    let calls = repo.calls();
    // Only the read before the wait fetched the branch.
    assert_eq!(calls.matches("refs/heads/7-x:").count(), 1, "{calls}");
    for call in [
        "git worktree add",
        "gh pr merge",
        "/labels/",
        "gh pr comment",
    ] {
        assert!(!calls.contains(call), "{calls}");
    }
    assert_eq!(posts(repo), waited_on(head));
}

/// A worker's `prepare` can merge the PR while the refresh waits, and GitHub
/// deletes its branch.
#[test]
fn a_refresh_leaves_a_pr_that_merged_while_it_waited() {
    let repo = Repo::armed_behind("refresh-merged-meanwhile");
    let head = repo.head();
    let output = repo.refresh_waiting(|| {
        fs::write(repo.bin.join("merged-9"), "").unwrap();
        repo.origin(&["update-ref", "-d", "refs/heads/7-x"]);
    });
    assert_left(&repo, &output, &head, "it is MERGED");
}

/// The owner can take the PR's issue back while the refresh waits.
#[test]
fn a_refresh_leaves_a_pr_whose_issue_lost_land_needed_while_it_waited() {
    let repo = Repo::armed_behind("refresh-unlabeled-meanwhile");
    let head = repo.head();
    let output = repo.refresh_waiting(|| repo.set_land_needed(&[]));
    assert_left(
        &repo,
        &output,
        &head,
        "its issue no longer has `land-needed`",
    );
}

/// A worker's merge that comes during the refresh's proof waits until the
/// refresh has squashed the PR it proved, so no merge moves `main` between the
/// proof and its squash.
#[test]
fn a_refresh_keeps_the_merge_slot_from_its_proof_to_its_squash() {
    let repo = Repo::armed_behind("refresh-turn");
    repo.other_pr(10, "8-y", &Pr::default());
    repo.git(&["checkout", "-q", "main"]);
    let (refreshed, merged_ten, heard) = merge_ten_during(&repo, &["refresh"]);
    assert_eq!(
        refreshed.status.code(),
        Some(0),
        "{}{}",
        stdout(&refreshed),
        stderr(&refreshed)
    );
    assert_eq!(merged_ten.status.code(), Some(0), "{heard:?}");
    assert_merged_after(&repo.calls(), 9);
}

/// A timer can start a refresh while the last one still proves, and so can the
/// owner. The second one leaves everything to the first.
#[test]
fn a_refresh_that_starts_while_another_runs_proves_nothing() {
    let repo = Repo::armed_behind("refresh-twice");
    let _held = repo.hold("refresh.lock");
    repo.forget_calls();
    let output = repo.xtask(&["refresh"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let out = stdout(&output);
    assert_eq!(
        out, "refresh: another refresh runs, so this one proves nothing\n",
        "{out}"
    );
    let calls = repo.calls();
    assert!(!calls.contains("gh "), "{calls}");
    assert!(!calls.contains("fetch"), "{calls}");
}

/// A timer runs the refresh often, so a run with nothing to prove costs only
/// what it reads from GitHub.
#[test]
fn a_refresh_with_no_pr_to_prove_proves_nothing() {
    let repo = Repo::new("refresh-none");
    repo.set_open_prs(&[]);
    repo.set_land_needed(&[]);
    repo.git(&["checkout", "-q", "main"]);
    repo.forget_calls();
    let output = repo.xtask(&["refresh"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let calls = repo.calls();
    for call in ["cargo build", "cargo test", "fetch", "worktree", "statuses"] {
        assert!(!calls.contains(call), "{calls}");
    }
}

/// The owner can take the approval back during the proof.
#[test]
fn a_refresh_merges_no_pr_whose_auto_merge_was_turned_off() {
    let repo = Repo::new("refresh-disarmed");
    repo.prepared(BRANCH, 7, "");
    repo.advance_main("later.txt", "x\n");
    repo.set_open_prs(&[listed(9, BRANCH, &repo.head(), 7, false)]);
    repo.set_land_needed(&[7]);
    repo.git(&["checkout", "-q", "main"]);
    let disarm = format!(": > '{}'", repo.bin.join("disarmed-9").display());

    let output = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .arg("refresh")
        .env("RUN_ON", "state=success")
        .env("RUN_COMMAND", &disarm)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    assert_eq!(last_post(&repo), format!("success {GREEN}"));
    let calls = repo.calls();
    assert!(!calls.contains("gh pr merge"), "{calls}");
    assert!(!calls.contains("/labels/"), "{calls}");
}

impl Repo {
    /// The comment that a proof of an earlier head left, which the owner
    /// approved by arming auto-merge.
    pub(super) fn approved(&self, reasons: &[&str]) {
        let lines: Vec<String> = reasons.iter().map(|reason| format!("- {reason}")).collect();
        let body = format!(
            "Merging this PR at `0123456789abcdef0123456789abcdef01234567` accepts:\n\n{}\n\n",
            lines.join("\n")
        );
        fs::write(self.bin.join("pr-comments"), body).unwrap();
    }

    /// A refresh in Actions, and the summary it wrote.
    pub(super) fn refresh_summed_up(&self) -> (Output, String) {
        let summary = self.temp.join("summary.md");
        self.git(&["checkout", "-q", "main"]);
        let output = self
            .command(env!("CARGO_BIN_EXE_xtask"))
            .arg("refresh")
            .env("GITHUB_ACTIONS", "true")
            .env("GITHUB_STEP_SUMMARY", &summary)
            .output()
            .unwrap();
        (output, fs::read_to_string(&summary).unwrap_or_default())
    }
}

/// A PR whose journey count falls from 2 to 1, armed and behind `main`.
pub(super) fn falling(name: &str) -> Repo {
    let repo = with_base(name, 0, 2);
    repo.prepared(BRANCH, 7, "");
    repo.advance_main("later.txt", "x\n");
    repo.set_open_prs(&[listed(9, BRANCH, &repo.head(), 7, false)]);
    repo.set_land_needed(&[7]);
    repo.forget_calls();
    repo
}

/// The owner's approval covers only what the comment listed when they armed
/// the PR, so a refresh that finds more turns auto-merge off before the green
/// that GitHub's auto-merge would act on.
#[test]
fn a_refresh_leaves_open_a_pr_whose_proof_accepts_something_new() {
    let repo = falling("refresh-new-reason");
    repo.approved(&["it changes `.github/workflows/x.yml`"]);

    let (output, summary) = repo.refresh_summed_up();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    let head = repo.head();
    let calls = repo.calls();
    let off = calls
        .find("gh pr merge 9 --disable-auto")
        .unwrap_or_else(|| panic!("auto-merge stayed on:\n{calls}"));
    let green = calls.find(&status(&head, "success")).expect(&calls);
    assert!(off < green, "{calls}");
    assert!(!calls.contains(&merged(9, &head)), "{calls}");
    assert!(!calls.contains("/labels/"), "{calls}");
    assert!(!calls.contains("/issues/7 "), "{calls}");
    let comments = repo.pr_comments();
    let listed = format!("Merging this PR at `{head}` accepts:\n\n- journey fell from 2 to 1\n");
    assert!(comments.contains(&listed), "{comments}");
    let why = "PR 9 stays open, because its proof accepts what the owner didn't approve: journey fell from 2 to 1. Arming its auto-merge again approves it";
    let out = stdout(&output);
    assert!(out.contains(&format!("refresh: {why}\n")), "{out}");
    assert!(
        summary.contains("| #9 | proven | spec 0 of 2, journey 1 of 1 | "),
        "{summary}"
    );
    assert!(summary.contains(&format!("\n{why}\n")), "{summary}");
}

/// A fall no bigger than the one approved is the same reason, whatever head
/// or `main` it was measured on.
#[test]
fn a_refresh_merges_a_pr_whose_proof_accepts_what_was_approved() {
    let repo = falling("refresh-same-reason");
    repo.approved(&[
        "journey fell from 3 to 2",
        "it changes `.github/workflows/x.yml`",
    ]);

    let (output, summary) = repo.refresh_summed_up();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    let calls = repo.calls();
    assert!(!calls.contains("--disable-auto"), "{calls}");
    assert!(calls.contains(&merged(9, &repo.head())), "{calls}");
    assert!(calls.contains(&closed(7)), "{calls}");
    assert!(summary.contains("| #9 | merged | "), "{summary}");
    assert!(!summary.contains("stays open"), "{summary}");
}

const SOMETHING_WENT_WRONG: &str =
    "echo 'GraphQL: Something went wrong while executing your query' >&2; exit 1";

impl Repo {
    /// A refresh that runs `command` in place of the first call that
    /// matches `on`.
    fn refresh_running(&self, on: &str, command: &str) -> Output {
        self.git(&["checkout", "-q", "main"]);
        self.command(env!("CARGO_BIN_EXE_xtask"))
            .arg("refresh")
            .env("RUN_ON", on)
            .env("RUN_COMMAND", command)
            .output()
            .unwrap()
    }

    /// A refresh where the first call that matches `on` fails, as GitHub's
    /// GraphQL did for PR 855.
    fn refresh_failing_once(&self, on: &str) -> Output {
        self.refresh_running(on, SOMETHING_WENT_WRONG)
    }

    /// A refresh where every call that matches `on` fails.
    fn refresh_failing(&self, on: &str) -> Output {
        let again = format!(
            "rm '{}'; {SOMETHING_WENT_WRONG}",
            self.bin.join("ran").display()
        );
        self.refresh_running(on, &again)
    }
}

/// Asserts that a refresh failed the PR, and kept it for the next run.
fn assert_kept(output: &Output, calls: &str) {
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}{}",
        stdout(output),
        stderr(output)
    );
    assert!(!calls.contains("/labels/"), "{calls}");
    assert!(!calls.contains("gh pr merge"), "{calls}");
    let out = stdout(output);
    assert!(!out.contains("refresh: proving `main`"), "{out}");
    let kept = "refresh: PR 9 keeps `land-needed`, because its proof passed, its head moved, or GitHub didn't say, so the next run proves it again";
    assert!(out.contains(kept), "{out}");
}

/// A GitHub call that fails after the proof passed gets a second try, so the
/// PR merges in the same run, with one comment.
#[test]
fn a_refresh_merges_a_pr_whose_comment_fails_once_after_its_proof() {
    let repo = falling("refresh-comment-once");
    repo.approved(&["journey fell from 2 to 1"]);

    let output = repo.refresh_failing_once("gh pr comment 9");
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(repo.bin.join("ran").exists());
    let head = repo.head();
    let calls = repo.calls();
    assert!(calls.contains(&merged(9, &head)), "{calls}");
    assert!(calls.contains(&closed(7)), "{calls}");
    let comments = repo.pr_comments();
    let listed = format!("Merging this PR at `{head}` accepts:");
    assert_eq!(comments.matches(&listed).count(), 1, "{comments}");
    assert!(!comments.contains("takes it up"), "{comments}");
}

#[test]
fn a_refresh_merges_a_pr_whose_green_fails_once_after_its_proof() {
    let repo = Repo::new("refresh-green-once");
    repo.prepared(BRANCH, 7, "");
    repo.advance_main("later.txt", "x\n");
    repo.set_open_prs(&[listed(9, BRANCH, &repo.head(), 7, false)]);
    repo.set_land_needed(&[7]);

    let output = repo.refresh_failing_once("state=success");
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(repo.bin.join("ran").exists());
    assert_eq!(last_post(&repo), format!("success {GREEN}"));
    let calls = repo.calls();
    assert!(calls.contains(&merged(9, &repo.head())), "{calls}");
    assert!(!calls.contains("gh pr comment"), "{calls}");
}

/// `prepare` pushes only a head it proved, so whatever stops it after the
/// push is never the change's. The PR keeps `land-needed`, and the next run
/// finishes it.
#[test]
fn a_refresh_keeps_a_pr_that_github_fails_after_its_proof() {
    let repo = falling("refresh-comment-fails");
    repo.approved(&["journey fell from 2 to 1"]);
    fs::write(repo.bin.join("comment-fails"), "").unwrap();

    let output = repo.refresh();
    let calls = repo.calls();
    assert_kept(&output, &calls);
    // The comment and its second try. The refresh posts none of its own.
    assert_eq!(calls.matches("gh pr comment 9").count(), 2, "{calls}");
    assert_eq!(
        posts(&repo).pop().unwrap(),
        (
            repo.head(),
            "failure Proven, then stopped by an error".to_string()
        )
    );

    fs::remove_file(repo.bin.join("comment-fails")).unwrap();
    let output = repo.refresh();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    let calls = repo.calls();
    assert!(calls.contains(&merged(9, &repo.head())), "{calls}");
    assert!(calls.contains(&closed(7)), "{calls}");
}

/// A call can fail after the proof and before the push, and then the
/// status says so.
#[test]
fn a_refresh_keeps_a_pr_that_github_fails_before_its_push() {
    let repo = Repo::new("refresh-read-fails");
    repo.prepared(BRANCH, 7, "");
    repo.advance_main("later.txt", "x\n");
    let head = repo.head();
    repo.set_open_prs(&[listed(9, BRANCH, &head, 7, false)]);
    repo.set_land_needed(&[7]);

    let output = repo.refresh_failing("ls-remote origin refs/heads/main");
    let calls = repo.calls();
    assert_kept(&output, &calls);
    assert_eq!(calls.matches("ls-remote origin refs/heads/main").count(), 2);
    assert_eq!(repo.head(), head);
    assert_eq!(
        posts(&repo).pop().unwrap(),
        (head, "failure Proven, then stopped by an error".to_string())
    );
    assert!(!calls.contains("gh pr comment"), "{calls}");
}

/// A status that `prepare` posted can fail to read back, and then the
/// refresh posts nothing over it.
#[test]
fn a_refresh_keeps_a_pr_whose_status_it_can_not_read_after_the_proof() {
    let repo = Repo::new("refresh-status-unread");
    repo.prepared(BRANCH, 7, "");
    repo.advance_main("later.txt", "x\n");
    let head = repo.head();
    repo.set_open_prs(&[listed(9, BRANCH, &head, 7, false)]);
    repo.set_land_needed(&[7]);
    let unreadable = format!(
        ": > '{}'; rm '{}'; {SOMETHING_WENT_WRONG}",
        repo.bin.join("status-read-fails").display(),
        repo.bin.join("ran").display()
    );

    let output = repo.refresh_running("ls-remote origin refs/heads/main", &unreadable);
    let calls = repo.calls();
    assert_kept(&output, &calls);
    assert!(stderr(&output).contains("no status to read"));
    assert_eq!(repo.head(), head);
    assert_eq!(
        posts(&repo).pop().unwrap(),
        (head, "failure Proven, then stopped by an error".to_string())
    );
}

/// GitHub can fail the refresh's read of where the proof left the PR too.
#[test]
fn a_refresh_keeps_a_pr_whose_head_it_can_not_read_after_the_proof() {
    let repo = falling("refresh-head-unread");
    repo.approved(&["journey fell from 2 to 1"]);
    fs::write(repo.bin.join("comment-fails"), "").unwrap();

    let output = repo.refresh_failing_once("ls-remote origin refs/heads/7-x");
    assert!(repo.bin.join("ran").exists());
    let calls = repo.calls();
    assert_kept(&output, &calls);
    assert_eq!(calls.matches("gh pr comment 9").count(), 2, "{calls}");
}

/// A proof that accepts something new turns auto-merge off, and that call
/// gets a second try too.
#[test]
fn a_refresh_turns_auto_merge_off_when_the_first_try_fails() {
    let repo = falling("refresh-disarm-once");
    repo.approved(&["it changes `.github/workflows/x.yml`"]);

    let output = repo.refresh_failing_once("--disable-auto");
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    let head = repo.head();
    let calls = repo.calls();
    assert_eq!(
        calls.matches("gh pr merge 9 --disable-auto").count(),
        2,
        "{calls}"
    );
    assert!(repo.bin.join("disarmed-9").exists());
    let off = calls.rfind("gh pr merge 9 --disable-auto").unwrap();
    assert!(
        off < calls.find(&status(&head, "success")).expect(&calls),
        "{calls}"
    );
    assert!(!calls.contains(&merged(9, &head)), "{calls}");
    assert!(!calls.contains("/labels/"), "{calls}");
}

/// A call that turned auto-merge off can fail all the same, and the second
/// try finds it off.
#[test]
fn a_refresh_reads_auto_merge_again_before_it_turns_it_off_again() {
    let repo = falling("refresh-disarmed-anyway");
    repo.approved(&["it changes `.github/workflows/x.yml`"]);
    let disarmed = format!(
        ": > '{}'; {SOMETHING_WENT_WRONG}",
        repo.bin.join("disarmed-9").display()
    );

    let output = repo.refresh_running("--disable-auto", &disarmed);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    let head = repo.head();
    let calls = repo.calls();
    assert_eq!(
        calls.matches("gh pr merge 9 --disable-auto").count(),
        1,
        "{calls}"
    );
    assert_eq!(last_post(&repo), format!("success {GREEN}"));
    assert!(!calls.contains(&merged(9, &head)), "{calls}");
    assert!(!calls.contains("/labels/"), "{calls}");
}

/// A comment call can fail after GitHub took the comment.
#[test]
fn a_comment_that_was_posted_is_not_posted_again() {
    let repo = falling("refresh-comment-posted");
    repo.approved(&["journey fell from 2 to 1"]);
    let comments = repo.bin.join("pr-comments").display().to_string();
    let posted = format!("cat >> '{comments}'; echo >> '{comments}'; {SOMETHING_WENT_WRONG}");

    let output = repo.refresh_running("gh pr comment 9", &posted);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    let head = repo.head();
    let calls = repo.calls();
    assert_eq!(calls.matches("gh pr comment 9").count(), 1, "{calls}");
    assert!(calls.contains(&merged(9, &head)), "{calls}");
    let listed = format!("Merging this PR at `{head}` accepts:");
    let comments = repo.pr_comments();
    assert_eq!(comments.matches(&listed).count(), 1, "{comments}");
}

/// A head that someone pushed during the proof wasn't proven, so it isn't
/// the change's failure either.
#[test]
fn a_refresh_keeps_a_pr_whose_head_someone_moved_during_its_proof() {
    let repo = Repo::new("refresh-pushed-during");
    repo.prepared(BRANCH, 7, "");
    repo.advance_main("later.txt", "x\n");
    repo.set_open_prs(&[listed(9, BRANCH, &repo.head(), 7, false)]);
    repo.set_land_needed(&[7]);
    // Its commit before the record, as a person would push it.
    let pushed = repo.origin(&["rev-parse", "refs/heads/7-x~1"]);
    let push = repo.update_ref("refs/heads/7-x", &pushed);

    let output = repo.refresh_running("cargo fmt", &push);
    assert!(repo.bin.join("ran").exists());
    let calls = repo.calls();
    assert_kept(&output, &calls);
    assert_eq!(repo.head(), pushed);
    assert!(!calls.contains("gh pr comment"), "{calls}");
}

/// A check that fails on a `main` that passes is the change's, so a worker
/// takes it up again.
#[test]
fn a_refresh_hands_back_a_pr_whose_check_fails() {
    let repo = Repo::new("refresh-check-fails");
    repo.change_branch("crates/demo/src/lib.rs", "pub  fn f( ) {}\n");
    repo.prepared(BRANCH, 7, "");
    repo.advance_main("later.txt", "x\n");
    repo.set_open_prs(&[listed(9, BRANCH, &repo.head(), 7, false)]);
    repo.set_land_needed(&[7]);

    let output = repo.refresh();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    assert_eq!(last_post(&repo), "failure Failed: format");
    let calls = repo.calls();
    assert!(calls.contains(&unlabeled(7)), "{calls}");
    assert!(calls.contains(&unlabeled(9)), "{calls}");
    assert!(!calls.contains("gh pr merge"), "{calls}");
    let out = stdout(&output);
    assert_eq!(out.matches("refresh: proving `main`").count(), 1, "{out}");
    let comments = repo.pr_comments();
    assert!(comments.contains("Failed: format"), "{comments}");
    assert!(
        comments.contains("so a worker takes it up again"),
        "{comments}"
    );
}

/// A head that a person pushed after the proof isn't the one it proved.
#[test]
fn a_refresh_merges_no_pr_whose_head_moved() {
    let repo = Repo::new("refresh-head-moved");
    repo.prepared(BRANCH, 7, "");
    repo.advance_main("later.txt", "x\n");
    repo.set_open_prs(&[listed(9, BRANCH, &repo.head(), 7, false)]);
    repo.set_land_needed(&[7]);
    repo.git(&["checkout", "-q", "main"]);

    let output = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .arg("refresh")
        // A PR behind `main` has its status read first after the proof,
        // which is after the refresh read its head.
        .env("RUN_ON", "/commits/")
        .env(
            "RUN_COMMAND",
            repo.update_ref("refs/heads/7-x", "refs/heads/main"),
        )
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(
        out.contains("refresh: PR 9 stays open, because its head moved from "),
        "{out}"
    );
    let calls = repo.calls();
    assert!(!calls.contains("gh pr merge"), "{calls}");
    assert!(!calls.contains("/labels/"), "{calls}");
}

/// `prepare` refuses a PR with a standing change request, so a green from
/// before the review doesn't merge it either.
#[test]
fn a_refresh_merges_no_green_pr_that_a_review_asked_to_change() {
    let repo = Repo::new("refresh-changes-requested");
    repo.prepared(BRANCH, 7, "");
    repo.set_pr(&Pr {
        reviews: &[("a", "CHANGES_REQUESTED")],
        ..Pr::default()
    });
    repo.set_green(&repo.head());
    repo.set_open_prs(&[listed(9, BRANCH, &repo.head(), 7, false)]);
    repo.set_land_needed(&[7]);

    let output = repo.refresh();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        stdout(&output).contains("requested changes"),
        "{}",
        stdout(&output)
    );
    assert!(!repo.calls().contains("gh pr merge"), "{}", repo.calls());
}

/// The label goes from the issue that this run listed the PR with, and that
/// is the issue to close. A PR that has come to close another one waits for a
/// run that lists it so.
#[test]
fn a_refresh_merges_no_green_pr_that_came_to_close_another_issue() {
    let repo = Repo::new("refresh-issue-changed");
    repo.prepared(BRANCH, 7, "");
    repo.set_pr(&Pr {
        closes: &[8],
        ..Pr::default()
    });
    repo.set_green(&repo.head());
    repo.set_open_prs(&[listed(9, BRANCH, &repo.head(), 7, false)]);
    repo.set_land_needed(&[7]);

    let output = repo.refresh();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        stdout(&output).contains("the issue it closes changed"),
        "{}",
        stdout(&output)
    );
    let calls = repo.calls();
    assert!(!calls.contains("gh pr merge"), "{calls}");
    assert!(!calls.contains("-X DELETE"), "{calls}");
    assert!(!calls.contains("-X PATCH"), "{calls}");
}

/// `gh pr merge` can fail after its squash, as when it can't delete the
/// branch. The PR merged all the same, so its issue is closed.
#[test]
fn a_refresh_closes_the_issue_when_gh_fails_after_its_squash() {
    let repo = Repo::new("refresh-late-failure");
    repo.prepared(BRANCH, 7, "");
    repo.set_green(&repo.head());
    repo.set_open_prs(&[listed(9, BRANCH, &repo.head(), 7, false)]);
    repo.set_land_needed(&[7]);
    fs::write(repo.bin.join("merge-fails-late"), "").unwrap();

    let output = repo.refresh();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    let calls = repo.calls();
    let (unlabel, close) = (calls.find(&unlabeled(7)), calls.find(&closed(7)));
    assert!(unlabel.is_some() && unlabel < close, "{calls}");
}

/// The next run lists only open PRs, so a step after the squash that fails
/// can't be retried. The PR merged all the same, and the run names what is
/// left to do.
#[test]
fn a_refresh_that_merges_says_so_when_a_later_step_fails() {
    let repo = Repo::new("refresh-left-undone");
    repo.prepared(BRANCH, 7, "");
    repo.set_green(&repo.head());
    repo.set_open_prs(&[listed(9, BRANCH, &repo.head(), 7, false)]);
    repo.set_land_needed(&[7]);
    for name in ["refuse-label-7", "refuse-close-7"] {
        fs::write(repo.bin.join(name), "").unwrap();
    }

    let (output, summary) = repo.refresh_summed_up();
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(summary.contains("| #9 | merged | "), "{summary}");
    let said = stderr(&output);
    assert!(!said.contains("wasn't merged"), "{said}");
    for left in [
        "refresh: PR 9 merged, but issue 7 keeps `land-needed`: ",
        "refresh: PR 9 merged, but issue 7 is still open: ",
    ] {
        assert!(said.contains(left), "{said}");
    }
    // Each step gets a try, so one that fails stops none of the others.
    let calls = repo.calls();
    assert!(calls.contains(&unlabeled(9)), "{calls}");
    assert!(calls.contains(&closed(7)), "{calls}");
}

/// Each merge moves `main`, so a PR that was green on the old one is proven
/// again before it merges.
#[test]
fn a_refresh_proves_a_pr_that_an_earlier_merge_left_behind() {
    let repo = Repo::new("refresh-left-behind");
    repo.other_pr(
        4,
        "8-y",
        &Pr {
            closes: &[8],
            ..Pr::default()
        },
    );
    repo.prepared(BRANCH, 7, "");
    let (nine, four) = (repo.head(), repo.head_of("8-y"));
    repo.set_green(&nine);
    repo.set_green(&four);
    repo.set_open_prs(&[
        listed(4, "8-y", &four, 8, false),
        listed(9, BRANCH, &nine, 7, false),
    ]);
    repo.set_land_needed(&[7, 8]);
    repo.git(&["checkout", "-q", "main"]);

    let output = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .arg("refresh")
        .env("RUN_ON", "gh pr merge 4")
        .env(
            "RUN_COMMAND",
            repo.update_ref("refs/heads/main", "refs/heads/8-y"),
        )
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    let calls = repo.calls();
    assert!(calls.contains(&merged(4, &four)), "{calls}");
    assert!(
        stdout(&output).contains("refresh: proving PR 9 (1 of 1)\n"),
        "{}",
        stdout(&output)
    );
    assert_ne!(repo.head(), nine);
    assert_eq!(repo.origin(&["rev-parse", "refs/heads/7-x~2"]), four);
    assert!(calls.contains(&merged(9, &repo.head())), "{calls}");
}

/// A proof that fails only because `main` moved says nothing about the
/// change, so the PR keeps waiting for the next run.
#[test]
fn a_refresh_keeps_a_pr_waiting_when_main_moved_during_its_proof() {
    let repo = Repo::new("refresh-main-moved");
    repo.prepared(BRANCH, 7, "");
    repo.advance_main("later.txt", "x\n");
    repo.set_open_prs(&[listed(9, BRANCH, &repo.head(), 7, false)]);
    repo.set_land_needed(&[7]);
    repo.git(&["checkout", "-q", "main"]);

    let output = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .arg("refresh")
        .env("RUN_ON", "cargo build --release")
        .env(
            "RUN_COMMAND",
            repo.update_ref("refs/heads/main", "refs/heads/7-x"),
        )
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    assert_eq!(
        last_post(&repo),
        "failure `main` moved during the merge, so this head wasn't tested on it. Run it again"
    );
    let calls = repo.calls();
    assert!(!calls.contains("gh pr merge"), "{calls}");
    assert!(!calls.contains("/labels/"), "{calls}");
    assert_eq!(repo.pr_comments(), "");
}

/// Every `prepare` builds the xtask of the newest `main`, whatever `refresh`
/// was built from.
#[test]
fn a_refresh_built_from_an_older_main_proves_with_the_newest() {
    let repo = Repo::new("refresh-older-checks");
    repo.prepared(BRANCH, 7, "");
    repo.advance_main("later.txt", "x\n");
    repo.git(&["checkout", "-q", "main"]);
    repo.git(&["reset", "-q", "--hard", "HEAD~1"]);
    repo.set_open_prs(&[listed(9, BRANCH, &repo.head(), 7, false)]);
    repo.set_land_needed(&[7]);

    let output = repo.xtask(&["refresh"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    assert_eq!(last_post(&repo), "success spec 0 of 2, journey 1 of 1");
}

/// It also waits for the owner, which a refresh proves like any other.
#[test]
fn a_refresh_proves_a_change_to_xtask_with_its_own_xtask_too() {
    failing();
    let base = record(0, 1, "");
    let repo = Repo::with(
        "refresh-own-runner",
        &[("harness/ledger/3.toml", base.as_str())],
        &[
            ("xtask/src/journey.rs", "// variant: failing\n"),
            ("xtask/src/accepts.rs", "// what waits\n"),
        ],
    );
    repo.prepared(BRANCH, 7, "");
    repo.advance_main("later.txt", "x\n");
    repo.set_open_prs(&[listed(9, BRANCH, &repo.head(), 7, false)]);
    repo.set_land_needed(&[7]);

    let output = repo.refresh();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    assert_eq!(last_post(&repo), "success spec 0 of 2, journey 0 of 1");
    let comments = repo.pr_comments();
    assert!(comments.contains("journey fell from 1 to 0"), "{comments}");
    assert!(
        comments.contains("it changes `xtask/src/accepts.rs`, which decides"),
        "{comments}"
    );
}
