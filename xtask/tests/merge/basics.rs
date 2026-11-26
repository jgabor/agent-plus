use super::*;

#[test]
fn a_merge_records_pushes_verifies_and_squashes() {
    let repo = Repo::new("happy");
    let output = repo.merge();
    assert_merged(&repo, &output);
    // It took the slot at once.
    assert!(!stderr(&output).contains(SLOT_WAIT), "{}", stderr(&output));

    let head = repo.head();
    let gated = repo.origin(&["rev-parse", &format!("{head}~1")]);
    assert_eq!(repo.commits_on_branch(), 2);
    assert_eq!(
        repo.origin(&["diff-tree", "--no-commit-id", "--name-only", "-r", &head]),
        "harness/ledger/7.toml"
    );
    let record = repo.origin(&["show", &format!("{head}:harness/ledger/7.toml")]);
    assert!(record.contains("issue = 7\npr = 9\n"), "{record}");
    assert!(
        record.contains(&format!("commit = \"{gated}\"")),
        "{record}"
    );
    assert!(
        record.contains("journey = { passing = 1, steps = 1 }"),
        "{record}"
    );

    let calls = repo.calls();
    assert!(
        calls.contains(&format!(
            "gh pr merge 9 -R o/r --squash --delete-branch --match-head-commit {head}"
        )),
        "{calls}"
    );
    assert!(
        calls.contains(&format!(
            "gh api -X POST repos/{{owner}}/{{repo}}/statuses/{head} -f state=success -f context=xtask/gate"
        )),
        "{calls}"
    );
}

#[test]
fn a_merge_leaves_the_callers_checkout_alone() {
    let repo = Repo::new("alone");
    let before = repo.git(&["rev-parse", "HEAD"]);
    assert_merged(&repo, &repo.merge());
    assert_eq!(repo.git(&["rev-parse", "HEAD"]), before);
    assert_eq!(repo.git(&["status", "--porcelain"]), "");
    assert!(repo.merge_trees().is_empty(), "{:?}", repo.merge_trees());
    assert_eq!(repo.git(&["worktree", "list"]).lines().count(), 1);
}

#[test]
fn a_merge_works_from_a_checkout_of_main() {
    let repo = Repo::new("from-main");
    repo.git(&["checkout", "-q", "main"]);
    assert_merged(&repo, &repo.merge());
}

#[test]
fn a_merge_rebases_onto_the_newest_main() {
    let repo = Repo::new("rebase");
    repo.advance_main("other.txt", "y\n");
    assert_merged(&repo, &repo.merge());
    let main = repo.origin(&["rev-parse", "refs/heads/main"]);
    assert_eq!(repo.origin(&["merge-base", &main, &repo.head()]), main);
    assert_eq!(repo.commits_on_branch(), 2);
}

#[test]
fn a_conflict_stops_the_merge_and_leaves_the_branch() {
    let repo = Repo::new("conflict");
    repo.advance_main("docs.txt", "from main\n");
    let before = repo.head();
    let output = repo.merge();
    assert_refused(&repo, &output, "rebase");
    assert_eq!(repo.head(), before);
    assert!(repo.merge_trees().is_empty(), "{:?}", repo.merge_trees());
    assert_eq!(repo.git(&["worktree", "list"]).lines().count(), 1);
}

#[test]
fn a_record_left_at_the_tip_is_replaced() {
    let repo = Repo::new("left-record");
    let old = record(0, 1, "");
    repo.change_branch("harness/ledger/7.toml", &old);
    assert_merged(&repo, &repo.merge());
    assert_eq!(repo.commits_on_branch(), 2);
    let record = repo.origin(&["show", &format!("{}:harness/ledger/7.toml", repo.head())]);
    assert!(record.contains("pr = 9\n"), "{record}");
}

#[test]
fn a_pr_without_auto_merge_needs_the_owner() {
    let repo = Repo::new("auto-pr");
    repo.set_issue(&["auto:pr"]);
    assert_refused(&repo, &repo.merge(), "prepare");
    assert_prepared(&repo, &repo.prepare());
}

#[test]
fn a_pr_must_close_exactly_one_issue_here() {
    let repo = Repo::new("no-issue");
    for (closes, elsewhere) in [
        (&[][..], &[][..]),
        (&[7, 8][..], &[][..]),
        (&[][..], &[7][..]),
    ] {
        repo.set_pr(&Pr {
            closes,
            elsewhere,
            ..Pr::default()
        });
        assert_refused(&repo, &repo.merge(), "Closes #");
    }
}

#[test]
fn a_standing_change_request_is_refused() {
    let repo = Repo::new("changes");
    for reviews in [
        &[("a", "CHANGES_REQUESTED")][..],
        &[("a", "CHANGES_REQUESTED"), ("a", "COMMENTED")][..],
        &[("a", "APPROVED"), ("a", "CHANGES_REQUESTED")][..],
        &[("a", "CHANGES_REQUESTED"), ("b", "APPROVED")][..],
    ] {
        repo.set_pr(&Pr {
            reviews,
            ..Pr::default()
        });
        assert_refused(&repo, &repo.prepare(), "changes");
    }
}

#[test]
fn a_change_request_that_was_answered_does_not_block() {
    for (index, reviews) in [
        &[("a", "CHANGES_REQUESTED"), ("a", "APPROVED")][..],
        &[("a", "DISMISSED")][..],
    ]
    .into_iter()
    .enumerate()
    {
        let repo = Repo::new(&format!("answered-{index}"));
        repo.set_pr(&Pr {
            reviews,
            ..Pr::default()
        });
        assert_merged(&repo, &repo.merge());
    }
}

#[test]
fn a_pr_from_a_fork_is_refused() {
    let repo = Repo::new("fork");
    repo.set_pr(&Pr {
        fork: true,
        ..Pr::default()
    });
    assert_refused(&repo, &repo.prepare(), "fork");
}

#[test]
fn a_pr_against_another_branch_is_refused() {
    let repo = Repo::new("other-base");
    repo.set_pr(&Pr {
        base: "release",
        ..Pr::default()
    });
    assert_refused(&repo, &repo.merge(), "main");
}

#[test]
fn an_already_merged_pr_succeeds_without_merging_again() {
    let repo = Repo::new("merged");
    repo.set_pr(&Pr {
        state: "MERGED",
        ..Pr::default()
    });
    let output = repo.merge();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!repo.calls().contains("gh pr merge"));
}

#[test]
fn a_closed_pr_is_a_problem() {
    let repo = Repo::new("closed");
    repo.set_pr(&Pr {
        state: "CLOSED",
        ..Pr::default()
    });
    assert_eq!(repo.merge().status.code(), Some(2));
}

#[test]
fn bad_usage_is_a_problem() {
    let repo = Repo::new("usage");
    for args in [
        &["merge"][..],
        &["merge", "x"][..],
        &["merge", "1", "extra"][..],
    ] {
        let output = repo.xtask(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(stderr(&output).contains("`merge` takes"), "{args:?}");
    }
}

#[test]
fn a_failing_gate_stops_the_merge() {
    let repo = Repo::new("gate-fails");
    repo.change_branch("crates/demo/src/lib.rs", "pub  fn f( ) {}\n");
    let output = repo.merge();
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(
        stdout(&output).contains("fail format"),
        "{}",
        stdout(&output)
    );
    assert!(!repo.calls().contains("git push"));
}

/// Binds a socket in its crate, which works only where the path is short.
const BINDS_A_SOCKET: &str = "#[test]\nfn binds() {\n    let path = std::path::Path::new(env!(\"CARGO_MANIFEST_DIR\")).join(\"s.sock\");\n    std::os::unix::net::UnixListener::bind(&path).unwrap();\n    std::fs::remove_file(&path).unwrap();\n}\n";

/// A worker's worktree is long, so a check that needs a short path must fail
/// in the merge too, even when the temporary directory is short.
#[test]
fn a_check_that_needs_a_short_path_fails_the_proof() {
    let mut repo = Repo::new("short-path");
    let short = env::temp_dir().join(format!("xm{}", std::process::id()));
    if short.exists() {
        fs::remove_dir_all(&short).unwrap();
    }
    fs::create_dir_all(&short).unwrap();
    repo.temp = short.clone();
    repo.change_branch("crates/demo/src/lib.rs", BINDS_A_SOCKET);
    let output = repo.prepare();
    fs::remove_dir_all(&short).unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(
        stdout(&output).contains("fail tests"),
        "{}",
        stdout(&output)
    );
}
