//! What the nightly job does when `main` fails the gate: finds the first bad
//! commit, and opens a revert PR for the owner to merge.

use crate::forge::{Forge, Gh};
use crate::gate;
use crate::lock::{self, Hold};
use crate::merge::Tree;
use crate::{Problem, Verdict, base};
use std::path::Path;

const ISSUE_TITLE: &str = "`main` fails the gate";
const LEDGER: &str = "harness/ledger";
/// Starts a comment that says how a response ended.
const RESPONDED: &str = "<!-- nightly response -->";
/// How many commits to ask GitHub about, looking for a passing nightly.
const REACH: usize = 200;
/// Names the throwaway tree. An issue or PR never has this number.
const TREE: u64 = 0;

/// Files one issue and tries to open one revert PR. An issue that says how an
/// earlier night ended stops this, until the owner closes it. One with no such
/// comment is from a run that died, so this picks it up.
pub fn respond(root: &Path, head: &str, status: &str) -> Result<(), Problem> {
    let _slot = lock::response_slot(root)?;
    let gh = Gh::new(root);
    let open = gh
        .open_issues()?
        .into_iter()
        .find(|issue| issue.title == ISSUE_TITLE);
    let issue = match open {
        Some(issue) => {
            let owner = gh.owner()?;
            let responded = gh
                .issue(issue.number)?
                .comments
                .iter()
                .any(|comment| comment.author == owner && comment.body.starts_with(RESPONDED));
            if responded {
                println!("nightly: #{} already covers the failing gate", issue.number);
                return Ok(());
            }
            println!("nightly: picking up #{}", issue.number);
            issue.number
        }
        None => {
            let issue = gh.create_issue(ISSUE_TITLE, &issue_body(head))?;
            println!("nightly: filed #{issue}");
            issue
        }
    };
    let hold = lock::shared(root)?;
    let tree = Tree::create(root, TREE, head)?;
    let outcome = bisect_and_revert(root, &gh, &tree, &hold, head, status, issue)
        .unwrap_or_else(|Problem(message)| format!("Couldn't bisect or revert: {message}"));
    gh.post_comment(issue, &format!("{RESPONDED}\n{outcome}"))?;
    Ok(())
}

fn issue_body(head: &str) -> String {
    format!(
        "`cargo xtask nightly` found that `{head}` fails the gate.\n\n## Goal\n`main` passes the gate again.\n\n## Done when\n`cargo xtask gate` passes on `main`.\n\n## Touches\nWhatever the first bad commit touched.\n"
    )
}

/// What to tell the issue, when nothing went wrong with the bisect itself.
fn bisect_and_revert(
    root: &Path,
    gh: &Gh,
    tree: &Tree,
    hold: &Hold,
    head: &str,
    status: &str,
    issue: u64,
) -> Result<String, Problem> {
    let Some(good) = newest_passing(root, gh, head, status)? else {
        return Ok(format!(
            "Found no passing `{status}` in the {REACH} commits before `{head}`, so nothing was bisected."
        ));
    };
    let range = format!("{good}..{head}");
    let listing = base::git_text(root, &["rev-list", "--first-parent", "--reverse", &range])?;
    let commits: Vec<&str> = listing.lines().collect();
    let bad = first_bad(tree, hold, &commits)?;
    let pr = open_revert(root, gh, tree, head, bad, &good, issue)?;
    Ok(format!(
        "Bisected from `{good}`. The first bad commit is `{bad}`, and #{pr} reverts it."
    ))
}

fn newest_passing(
    root: &Path,
    gh: &Gh,
    head: &str,
    status: &str,
) -> Result<Option<String>, Problem> {
    let reach = REACH.to_string();
    let args = [
        "rev-list",
        "--first-parent",
        "--skip=1",
        "--max-count",
        &reach,
        head,
    ];
    for commit in base::git_text(root, &args)?.lines() {
        if gh.status_passed(commit, status)? {
            return Ok(Some(commit.to_string()));
        }
    }
    Ok(None)
}

/// The first commit that fails, given that the last one does. A commit that
/// can't be tested counts as failing, as it does in the nightly job.
fn first_bad<'a>(tree: &Tree, hold: &Hold, commits: &[&'a str]) -> Result<&'a str, Problem> {
    let (mut low, mut high) = (0, commits.len() - 1);
    while low < high {
        let middle = (low + high) / 2;
        if passes(tree, hold, commits[middle])? {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    Ok(commits[high])
}

fn passes(tree: &Tree, hold: &Hold, commit: &str) -> Result<bool, Problem> {
    base::git_text(&tree.path, &["checkout", "-q", "-f", "--detach", commit])?;
    Ok(match gate::check(&tree.path, hold, None) {
        Ok((verdict, _)) => verdict == Verdict::Pass,
        Err(Problem(message)) => {
            eprintln!("nightly: {commit}: {message}");
            false
        }
    })
}

/// Reverts `bad` on top of `head`, keeping the ledger as it is, because a
/// record says what was measured, and that doesn't stop being true.
fn open_revert(
    root: &Path,
    gh: &Gh,
    tree: &Tree,
    head: &str,
    bad: &str,
    good: &str,
    issue: u64,
) -> Result<u64, Problem> {
    let path = tree.path.as_path();
    base::git_text(path, &["checkout", "-q", "-f", "--detach", head])?;
    base::git_text(path, &["revert", "--no-commit", bad])?;
    if !base::git_text(path, &["ls-tree", "--name-only", head, LEDGER])?
        .trim()
        .is_empty()
    {
        base::git_text(path, &["reset", "-q", head, "--", LEDGER])?;
        base::git_text(path, &["checkout", "-q", head, "--", LEDGER])?;
    }
    let subject = base::git_text(root, &["show", "-s", "--format=%s", bad])?;
    let title = format!("Revert \"{}\"", subject.trim());
    let message = format!("This reverts commit {bad}.");
    base::git_text(path, &["commit", "-q", "-m", &title, "-m", &message])?;
    let short = base::git_text(root, &["rev-parse", "--short", bad])?;
    let branch = format!("{issue}-revert-{}", short.trim());
    let target = format!("HEAD:refs/heads/{branch}");
    // The name is this issue's, so all that can be there is a killed run's work.
    base::git_text(path, &["push", "-q", "--force", "origin", &target])?;
    let label = (!base::protected_changes(path, head)?.is_empty()).then_some(base::SPEC_CHANGE);
    let body = format!(
        "## Problem\n`main` fails the gate at `{head}`.\n\n## Change\nReverts `{bad}`, apart from its ledger record. It is the first bad commit between `{good}`, the newest with a passing nightly, and `{head}`.\n\n## Proof\n`cargo xtask nightly` ran the gate at each step of the bisect, and took `{head}` as bad from its own run. If the bisect blames only the newest commit, that run may have hit a flaky check.\n\n## Decisions\nThe owner merges this with `cargo xtask merge <pr> --owner`.\n\nHill: none, this is a revert.\n\nCloses #{issue}.\n\nCreated by `cargo xtask nightly`.\n"
    );
    gh.create_pr(&branch, &title, &body, label)
}
