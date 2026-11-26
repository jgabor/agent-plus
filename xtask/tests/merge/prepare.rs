use super::counts::with_base;
use super::*;

/// Each status that was posted, in order: its commit, and then its state and
/// description as `failure Failed: format`.
pub(super) fn posts(repo: &Repo) -> Vec<(String, String)> {
    let prefix = "gh api -X POST repos/{owner}/{repo}/statuses/";
    repo.calls()
        .lines()
        .filter_map(|line| line.strip_prefix(prefix))
        .map(|post| {
            let (commit, fields) = post.split_once(" -f state=").expect(post);
            let (state, rest) = fields.split_once(" -f context=xtask/gate").expect(post);
            let said = rest.strip_prefix(" -f description=").unwrap_or(rest);
            let said = said.split(" -f target_url=").next().unwrap();
            (commit.to_string(), format!("{state} {said}"))
        })
        .collect()
}

/// What the newest status says.
pub(super) fn last_post(repo: &Repo) -> String {
    posts(repo).pop().expect("a status was posted").1
}

#[test]
fn the_gate_posts_no_status() {
    let repo = Repo::new("gate-status");
    let output = repo.xtask(&["gate"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(posts(&repo), []);

    repo.change_branch("crates/demo/src/lib.rs", "pub  fn f( ) {}\n");
    assert_eq!(repo.xtask(&["gate"]).status.code(), Some(1));
    assert_eq!(posts(&repo), []);
}

#[test]
fn prepare_records_pushes_posts_and_never_squashes() {
    let repo = Repo::new("prepare");
    repo.advance_main("other.txt", "y\n");
    let output = repo.prepare();
    assert_prepared(&repo, &output);
    assert_eq!(repo.commits_on_branch(), 2);
    assert!(stdout(&output).contains("ready"), "{}", stdout(&output));
    assert_eq!(repo.pr_comments(), "");
}

/// The phases go on the head that GitHub shows, and the score on the new one.
#[test]
fn a_proof_says_which_phase_it_is_in_and_then_the_score() {
    for command in ["merge", "prepare", "land"] {
        let repo = Repo::new(&format!("phases-{command}"));
        let repo = if command == "land" {
            repo.on_main()
        } else {
            repo
        };
        let before = repo.head();
        let output = repo.xtask(&[command, "9"]);
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        let after = repo.head();
        assert_ne!(before, after, "{command}");
        let said = |commit: &String, text: &str| (commit.clone(), text.to_string());
        assert_eq!(
            posts(&repo),
            [
                said(&before, "pending Proving: lint"),
                said(&before, "pending Proving: tests"),
                said(&before, "pending Proving: journey"),
                said(&after, "success spec 0 of 2, journey 1 of 1"),
            ],
            "{command}"
        );
    }
}

#[test]
fn a_proof_names_the_bench_only_when_it_runs() {
    let base = record(0, 1, "");
    let repo = Repo::with(
        "phases-bench",
        &[("harness/ledger/3.toml", base.as_str())],
        &[("crates/ap/src/main.rs", "fn main() {\n    println!();\n}\n")],
    );
    assert_prepared(&repo, &repo.prepare());
    let said: Vec<String> = posts(&repo).into_iter().map(|(_, said)| said).collect();
    assert_eq!(
        said[..4],
        [
            "pending Proving: lint",
            "pending Proving: tests",
            "pending Proving: journey",
            "pending Proving: bench"
        ]
    );
    assert_eq!(said.len(), 5, "{said:?}");

    // Each is posted just before the work it names. The journey and the bench
    // both build a release.
    let calls = repo.calls();
    let order = [
        calls.find("Proving: lint"),
        calls.find("cargo fmt --all"),
        calls.find("cargo clippy --workspace"),
        calls.find("Proving: tests"),
        calls.find("cargo test"),
        calls.find("Proving: journey"),
        calls.find("cargo build --release"),
        calls.find("Proving: bench"),
        calls.rfind("cargo build --release"),
        calls.find("state=success"),
    ];
    assert!(order.iter().all(Option::is_some), "{calls}");
    assert!(order.is_sorted(), "{order:?}: {calls}");
}

/// The proof is complete once it is green, whatever the squash does.
#[test]
fn a_squash_that_fails_leaves_the_proof_green() {
    let repo = Repo::new("squash-fails");
    fs::write(repo.bin.join("merge-fails"), "").unwrap();
    let output = repo.merge();
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(stderr(&output).contains("no merge"), "{}", stderr(&output));
    assert_eq!(
        posts(&repo).pop().unwrap(),
        (
            repo.head(),
            "success spec 0 of 2, journey 1 of 1".to_string()
        )
    );
}

#[test]
fn a_status_links_to_its_run_only_inside_one() {
    let run = [
        ("GITHUB_SERVER_URL", "https://github.com"),
        ("GITHUB_REPOSITORY", "o/r"),
        ("GITHUB_RUN_ID", "5"),
    ];
    let link = " -f target_url=https://github.com/o/r/actions/runs/5";
    for (name, known, linked) in [("linked", &run[..], 4), ("unlinked", &run[..2], 0)] {
        let repo = Repo::new(&format!("status-{name}"));
        let output = repo
            .command(env!("CARGO_BIN_EXE_xtask"))
            .args(["prepare", "9"])
            .envs(known.iter().copied())
            .output()
            .unwrap();
        assert_prepared(&repo, &output);
        let calls = repo.calls();
        let ending = calls.lines().filter(|line| line.ends_with(link)).count();
        assert_eq!(ending, linked, "{name}: {calls}");
        assert_eq!(calls.matches("target_url").count(), linked, "{name}");
    }
}

/// A `pending` only says what is happening, so losing one loses nothing else.
#[test]
fn a_pending_that_can_not_be_posted_changes_nothing() {
    let repo = Repo::new("prepare-no-pending");
    fs::write(repo.bin.join("pending-fails"), "").unwrap();
    let output = repo.prepare();
    assert_prepared(&repo, &output);
    assert!(
        stderr(&output).contains("no pending"),
        "{}",
        stderr(&output)
    );
    assert_eq!(last_post(&repo), "success spec 0 of 2, journey 1 of 1");
}

const LIVE: &str = "2999-01-01T00:00Z";

#[test]
fn a_thread_that_does_not_hold_the_issue_can_not_merge_or_prepare() {
    for args in [
        &["merge", "9"][..],
        &["merge", "9", "--thread", "other"],
        &["prepare", "9"],
        &["prepare", "9", "--thread", "other"],
        &["prepare", "9", "--keep-budgets", "--thread", "other"],
    ] {
        let repo = Repo::new("not-holder");
        repo.set_held_issue(&["auto:merge", "in-progress"], "holder", LIVE);
        let output = repo.xtask(args);
        assert_refused(&repo, &output, "held by thread `holder`");
        assert!(
            !repo.calls().contains("api -X"),
            "{args:?}: {}",
            repo.calls()
        );
    }
}

#[test]
fn the_thread_that_holds_the_issue_merges_and_prepares() {
    let repo = Repo::new("holder-merges");
    repo.set_held_issue(&["auto:merge", "in-progress"], "holder", LIVE);
    let output = repo.xtask(&["merge", "9", "--thread", "holder"]);
    assert_merged(&repo, &output);

    let repo = Repo::new("holder-prepares");
    repo.set_held_issue(&["auto:merge", "in-progress"], "holder", LIVE);
    let output = repo.xtask(&["prepare", "9", "--thread", "holder"]);
    assert_prepared(&repo, &output);
}

#[test]
fn with_no_live_claim_any_thread_prepares() {
    for (labels, lease) in [
        (&["auto:pr", "land-needed", "in-progress"][..], LIVE),
        (&["auto:pr", "in-progress"], "2000-01-01T00:00Z"),
        (&["auto:pr"], LIVE),
    ] {
        let repo = Repo::new("no-claim");
        repo.set_held_issue(labels, "holder", lease);
        let output = repo.prepare();
        assert_prepared(&repo, &output);
    }
}

#[test]
fn prepare_needs_no_auto_merge_and_takes_a_pr_that_waits() {
    let repo = Repo::new("prepare-waits");
    repo.set_issue(&[]);
    repo.change_branch(".github/workflows/x.yml", "name: x\n");
    assert_prepared(&repo, &repo.prepare());
}

#[test]
fn merge_has_no_owner_flag_and_prepare_takes_one_pr() {
    let repo = Repo::new("no-owner-flag");
    let output = repo.xtask(&["merge", "9", "--owner"]);
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(stderr(&output).contains("`merge` takes"));
    assert!(!repo.calls().contains("gh "), "{}", repo.calls());
    for args in [&["prepare"][..], &["prepare", "x"], &["prepare", "9", "x"]] {
        let output = repo.xtask(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(stderr(&output).contains("`prepare` takes"), "{args:?}");
    }
}

#[test]
fn a_prepare_can_run_again() {
    let repo = Repo::new("prepare-again");
    assert_prepared(&repo, &repo.prepare());
    repo.advance_main("other.txt", "y\n");
    assert_prepared(&repo, &repo.prepare());
    assert_eq!(repo.commits_on_branch(), 2);
}

#[test]
fn a_killed_prepare_is_not_green_and_resumes() {
    let repo = Repo::new("prepare-killed");
    let killed = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .args(["prepare", "9"])
        .env("KILL_ON", "state=success")
        .env("KILL_WHEN", "before")
        .output()
        .unwrap();
    assert_eq!(killed.status.code(), None);
    repo.forget_calls();
    assert_prepared(&repo, &repo.prepare());
    assert_eq!(repo.commits_on_branch(), 2);
}

#[test]
fn a_lowered_count_is_accepted_in_one_comment_and_merge_refuses_it() {
    let repo = with_base("accept-spec", 1, 1);
    assert_prepared(&repo, &repo.prepare());
    let comments = repo.pr_comments();
    assert!(comments.contains("spec fell from 1 to 0"), "{comments}");
    assert!(comments.contains(&repo.head()), "{comments}");
    assert_eq!(repo.calls().matches("gh pr comment").count(), 1);

    let repo = with_base("accept-journey", 0, 2);
    assert_prepared(&repo, &repo.prepare());
    let comments = repo.pr_comments();
    assert!(comments.contains("journey fell from 2 to 1"), "{comments}");
}

#[test]
fn a_prepared_branch_reads_its_own_record() {
    let repo = with_base("own-record", 0, 2);
    assert_prepared(&repo, &repo.prepare());
    repo.git(&["fetch", "-q", "origin", BRANCH]);
    repo.git(&["checkout", "-q", "--detach", "FETCH_HEAD"]);
    let output = repo.xtask(&["score"]);
    assert!(
        stdout(&output).contains("journey 1 of 1"),
        "{}",
        stdout(&output)
    );
}

/// The branch's head, which nothing replaced, is red and says why.
#[track_caller]
pub(super) fn assert_failure_posted(repo: &Repo, output: &Output, head: &str, why: &str) {
    assert_eq!(output.status.code(), Some(1), "{}", stdout(output));
    assert_eq!(
        posts(repo).pop().unwrap(),
        (head.to_string(), format!("failure {why}"))
    );
    assert_eq!(repo.head(), head);
    assert!(!repo.calls().contains("git push"), "{}", repo.calls());
    assert!(!repo.calls().contains("gh pr merge"), "{}", repo.calls());
}

#[test]
fn a_failed_proof_posts_failure_on_the_current_head() {
    let repo = Repo::new("prepare-fails");
    repo.advance_main("other.txt", "y\n");
    repo.change_branch("crates/demo/src/lib.rs", "pub  fn f( ) {}\n");
    let head = repo.head();
    assert_failure_posted(&repo, &repo.prepare(), &head, "Failed: format");
}

#[test]
fn a_branch_that_does_not_rebase_posts_failure() {
    let repo = Repo::new("prepare-conflict");
    repo.advance_main("docs.txt", "from main\n");
    let head = repo.head();
    assert_failure_posted(&repo, &repo.prepare(), &head, NO_REBASE);
}

#[test]
fn a_failure_that_can_not_be_posted_is_still_a_failure() {
    let repo = Repo::new("prepare-no-post");
    repo.change_branch("crates/demo/src/lib.rs", "pub  fn f( ) {}\n");
    fs::write(repo.bin.join("status-fails"), "").unwrap();
    let output = repo.prepare();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("no status"), "{}", stderr(&output));
}

/// An agent's `merge` leaves the reason on the PR too, with every check that
/// failed, in the order they ran.
#[test]
fn a_failed_merge_posts_failure() {
    let repo = Repo::new("merge-failure");
    let unused = "pub  fn f( ) {\n    let x = 1;\n}\n";
    repo.change_branch("crates/demo/src/lib.rs", unused);
    let head = repo.head();
    assert_failure_posted(&repo, &repo.merge(), &head, "Failed: format, lint");
}
