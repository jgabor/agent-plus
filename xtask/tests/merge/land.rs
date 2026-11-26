use super::counts::with_base;
use super::floor::revert_of_a_check;
use super::mains_xtask::builds;
use super::prepare::assert_failure_posted;
use super::*;

#[test]
fn land_squashes_what_only_the_owner_may_merge() {
    let repo = Repo::new("land-owner").on_main();
    repo.set_issue(&[]);
    let output = repo.land();
    assert_merged(&repo, &output);

    let head = repo.head();
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
    assert_eq!(repo.commits_merged(), 2);
}

#[test]
fn land_waits_for_the_merge_slot() {
    let repo = Repo::new("land-slot").on_main();
    let directory = repo.root.join(".git/xtask");
    fs::create_dir_all(&directory).unwrap();
    let held = fs::File::create(directory.join("merge.lock")).unwrap();
    held.lock().unwrap();
    let child = repo
        .command(env!("CARGO_BIN_EXE_xtask"))
        .args(["land", "9"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(500));
    assert!(!repo.calls().contains("git fetch"), "{}", repo.calls());
    held.unlock().unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(repo.bin.join("merged").exists());
}

#[test]
fn land_keeps_the_slot_until_its_squash_finishes() {
    let repo = Repo::new("land-outlives").on_main();
    let killed = repo.land_running("gh pr merge", "kill -9 $PPID; sleep 2");
    assert_eq!(killed.status.code(), None);
    let output = repo.land();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("already merged"),
        "{}",
        stdout(&output)
    );
    assert_eq!(
        fs::read_to_string(repo.bin.join("merges")).unwrap(),
        "merged\n"
    );
}

/// The PR is ready for a merge button, and `land` said what that accepts.
#[track_caller]
fn assert_stopped_to_accept(repo: &Repo, output: &Output, accepted: &str) {
    assert_eq!(output.status.code(), Some(1), "{}", stdout(output));
    assert!(stderr(output).contains(accepted), "{}", stderr(output));
    assert_left_prepared(repo);
    let comments = repo.pr_comments();
    assert!(comments.contains(accepted), "{comments}");
}

#[test]
fn land_stops_before_accepting_a_lowered_count() {
    let repo = with_base("land-lower", 1, 1).on_main();
    let output = repo.land();
    assert_stopped_to_accept(&repo, &output, "spec fell from 1 to 0");
}

#[test]
fn land_stops_before_accepting_a_revert() {
    let (repo, _) = revert_of_a_check("land-revert", None);
    let repo = repo.on_main();
    let output = repo.land();
    assert_stopped_to_accept(&repo, &output, "exact revert");
}

#[test]
fn land_merges_nothing_when_the_gate_fails() {
    let repo = Repo::new("land-gate");
    repo.change_branch("crates/demo/src/lib.rs", "pub  fn f( ) {}\n");
    let repo = repo.on_main();
    let head = repo.head();
    assert_failure_posted(&repo, &repo.land(), &head, "Failed: format");
}

#[test]
fn land_merges_nothing_when_the_branch_does_not_rebase() {
    let repo = Repo::new("land-conflict");
    repo.advance_main("docs.txt", "from main\n");
    let repo = repo.on_main();
    let head = repo.head();
    assert_failure_posted(&repo, &repo.land(), &head, NO_REBASE);
}

#[test]
fn land_merges_nothing_when_main_moves_during_the_proof() {
    let repo = Repo::new("land-moved").on_main();
    let elsewhere = repo.head();
    let moved = repo.update_ref("refs/heads/main", &elsewhere);
    let output = repo.land_running("cargo build --release", &moved);
    assert_refused(&repo, &output, "moved");
}

#[test]
fn land_succeeds_on_an_already_merged_pr_without_merging_again() {
    let repo = Repo::new("land-merged").on_main();
    repo.set_pr(&Pr {
        state: "MERGED",
        ..Pr::default()
    });
    let output = repo.land();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!repo.calls().contains("gh pr merge"));
}

#[test]
fn land_runs_from_anything_at_the_newest_main() {
    let repo = Repo::new("land-detached").on_main();
    repo.git(&["checkout", "-q", "--detach"]);
    repo.forget_calls();
    assert_merged(&repo, &repo.land());
}

#[test]
fn land_allows_untracked_files() {
    let repo = Repo::new("land-untracked").on_main();
    repo.write("notes.txt", "x\n");
    assert_merged(&repo, &repo.land());
}

#[test]
fn land_takes_one_pr() {
    let repo = Repo::new("land-usage").on_main();
    for args in [&["land"][..], &["land", "x"], &["land", "9", "x"]] {
        let output = repo.xtask(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(stderr(&output).contains("`land` takes"), "{args:?}");
    }
    assert!(!repo.calls().contains("gh "), "{}", repo.calls());
}

const BUDGETS_NOT_YET: &str =
    "[[budget]]\nkey = \"version_ms\"\nname = \"v\"\nband_pct = 20.0\nband_abs = 25.0\n";

/// The PR's own xtask measures what `main`'s may not, so `land` proves the
/// PR with both, from any checkout.
#[test]
fn land_proves_a_pr_that_may_add_a_measurer_with_its_own_xtask() {
    let base = record(0, 1, "");
    let repo = Repo::with(
        "land-measurer",
        &[
            ("harness/ledger/3.toml", base.as_str()),
            ("harness/budgets.toml", BUDGETS_NOT_YET),
        ],
        &[("xtask/src/bench.rs", "// the idle measurer\n")],
    )
    .on_main();
    assert_merged(&repo, &repo.land());
    assert_eq!(builds(&repo), 2, "{}", repo.calls());
}

#[test]
fn land_takes_a_change_to_a_measurer_when_every_budget_is_measured() {
    let base = record(0, 1, "");
    let repo = Repo::with(
        "land-measurer-known",
        &[("harness/ledger/3.toml", base.as_str())],
        &[("xtask/src/bench.rs", "// the boot measurer\n")],
    )
    .on_main();
    assert_merged(&repo, &repo.land());
}
