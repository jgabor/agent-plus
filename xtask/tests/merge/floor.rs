use super::prepare::last_post;
use super::*;

fn based(proven: u32, base: &str) -> String {
    record(proven, 1, "").replace("date =", &format!("base = \"{base}\"\ndate ="))
}

fn main_commit(repo: &Repo) -> String {
    repo.origin(&["rev-parse", "refs/heads/main"])
}

#[test]
fn a_record_names_the_main_it_was_measured_on() {
    let repo = Repo::new("record-base");
    repo.advance_main("other.txt", "y\n");
    let main = main_commit(&repo);
    assert_prepared(&repo, &repo.prepare());
    let record = repo.origin(&["show", &format!("{}:harness/ledger/7.toml", repo.head())]);
    assert!(record.contains(&format!("base = \"{main}\"\n")), "{record}");
}

#[test]
fn a_record_measured_on_an_older_main_is_not_the_floor() {
    let repo = Repo::new("stale-record");
    repo.advance_main("harness/ledger/8.toml", &based(1, &"0".repeat(40)));
    assert_merged(&repo, &repo.merge());
}

#[test]
fn a_record_measured_on_its_parent_is_the_floor() {
    let repo = Repo::new("fresh-record");
    let parent = main_commit(&repo);
    repo.advance_main("harness/ledger/8.toml", &based(1, &parent));
    let output = repo.merge();
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(
        stdout(&output).contains("fail hills"),
        "{}",
        stdout(&output)
    );
}

/// `main` gains a commit that changes `path` from `before` to `after`, and adds
/// a ledger record and a repeated line. The branch undoes it, with a commit
/// message naming `claimed`, or that commit if there is none.
pub(super) fn revert_of(
    name: &str,
    (path, before, after): (&str, &str, &str),
    claimed: Option<&str>,
) -> (Repo, String) {
    let repo = Repo::new(name);
    repo.git(&["checkout", "-q", "main"]);
    repo.write(path, before);
    repo.write("order.txt", "a\nb\n");
    repo.commit();
    repo.git(&["push", "-q", "origin", "main"]);
    let parent = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
    repo.write(path, after);
    repo.write("order.txt", "b\na\nb\n");
    repo.write("harness/ledger/8.toml", &record(0, 1, ""));
    repo.commit();
    repo.git(&["push", "-q", "origin", "main"]);
    let added = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
    repo.git(&["checkout", "-q", "-B", BRANCH, "main"]);
    repo.write(path, before);
    repo.write("order.txt", "a\nb\n");
    repo.git(&["add", "-A"]);
    let message = format!(
        "Undo it\n\nThis reverts commit {}.",
        claimed.unwrap_or(&added)
    );
    repo.git(&["commit", "-q", "-m", &message]);
    repo.git(&["push", "-q", "-f", "origin", BRANCH]);
    (repo, parent)
}

pub(super) fn revert_of_a_check(name: &str, claimed: Option<&str>) -> (Repo, String) {
    revert_of(
        name,
        ("crates/demo/src/lib.rs", NO_CHECK, CHECK_ONE),
        claimed,
    )
}

#[track_caller]
fn assert_floor_held(repo: &Repo, output: &Output) {
    assert_eq!(output.status.code(), Some(1), "{}", stdout(output));
    assert!(
        stdout(output).contains("fail checks-kept"),
        "{}",
        stdout(output)
    );
    assert!(!repo.calls().contains("gh pr merge"), "{}", repo.calls());
}

#[test]
fn the_owner_can_merge_an_exact_revert_that_removes_a_check() {
    let (repo, _) = revert_of_a_check("exact-revert", None);
    let output = repo.prepare();
    assert!(
        stdout(&output).contains("pass checks-kept"),
        "{}",
        stdout(&output)
    );
    assert_prepared(&repo, &output);
    let comments = repo.pr_comments();
    assert!(comments.contains("exact revert"), "{comments}");
    assert_eq!(repo.calls().matches("gh pr comment").count(), 1);
}

#[test]
fn an_exact_revert_still_needs_the_owner() {
    let (repo, _) = revert_of_a_check("revert-no-owner", None);
    let output = repo.merge();
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(
        stdout(&output).contains("fail checks-kept"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn a_revert_with_anything_else_in_it_keeps_the_floor() {
    let (repo, _) = revert_of_a_check("revert-plus", None);
    repo.change_branch("docs.txt", "and something else\n");
    let output = repo.prepare();
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(
        stdout(&output).contains("fail checks-kept"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn a_revert_of_a_commit_outside_the_history_keeps_the_floor() {
    let (repo, parent) = revert_of_a_check("revert-elsewhere", Some(&"0".repeat(40)));
    repo.git(&["checkout", "-q", "--detach", &parent]);
    repo.write("crates/demo/src/lib.rs", CHECK_ONE);
    repo.write("order.txt", "b\na\nb\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "The same change, on no branch"]);
    let twin = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
    repo.git(&["checkout", "-q", BRANCH]);
    repo.git(&[
        "commit",
        "-q",
        "--amend",
        "-m",
        &format!("Undo it\n\nThis reverts commit {twin}."),
    ]);
    repo.git(&["push", "-q", "-f", "origin", BRANCH]);
    let output = repo.prepare();
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(
        stdout(&output).contains("fail checks-kept"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn a_message_that_names_no_commit_keeps_the_floor() {
    let (repo, _) = revert_of_a_check("revert-unnamed", Some(&"0".repeat(40)));
    let output = repo.prepare();
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(
        stdout(&output).contains("fail checks-kept"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn undoing_the_same_lines_somewhere_else_keeps_the_floor() {
    let (repo, _) = revert_of_a_check("revert-elsewhere-in-file", None);
    repo.change_branch("order.txt", "b\na\n");
    assert_floor_held(&repo, &repo.prepare());
}

#[test]
fn a_revert_that_edits_the_ledger_keeps_the_floor() {
    let (repo, _) = revert_of_a_check("revert-ledger", None);
    repo.change_branch("harness/ledger/3.toml", &record(0, 1, "version_ms = 1\n"));
    assert_floor_held(&repo, &repo.prepare());
}

#[test]
fn the_owner_can_merge_a_revert_of_a_commit_that_broke_the_build() {
    let broken = "fn main() { not rust }\n";
    let (repo, _) = revert_of(
        "revert-build",
        ("crates/ap/src/main.rs", FAST, broken),
        None,
    );
    assert_prepared(&repo, &repo.prepare());
}

#[test]
fn a_change_that_only_looks_like_a_revert_is_benched() {
    let broken = "fn main() { not rust }\n";
    let (repo, _) = revert_of(
        "fake-revert-build",
        ("crates/ap/src/main.rs", FAST, broken),
        None,
    );
    repo.change_branch("docs.txt", "and something else\n");
    let output = repo.prepare();
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(stderr(&output).contains("build"), "{}", stderr(&output));
    assert_eq!(last_post(&repo), "failure Stopped by an error");
    assert!(!repo.calls().contains("git push"), "{}", repo.calls());
}

#[test]
fn a_revert_without_the_owner_is_benched() {
    let broken = "fn main() { not rust }\n";
    let (repo, _) = revert_of(
        "revert-no-owner-bench",
        ("crates/ap/src/main.rs", FAST, broken),
        None,
    );
    let output = repo.merge();
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(stderr(&output).contains("build"), "{}", stderr(&output));
}
