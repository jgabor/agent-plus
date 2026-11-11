//! `cargo xtask refresh`: proves each `land-needed` PR again on the newest
//! `main`, so that a merge button is never pressed on a stale proof.

use crate::forge::{Gh, OpenPr};
use crate::work::LAND_NEEDED;
use crate::{Problem, Verdict, merge, workspace};
use std::collections::BTreeSet;
use std::env;
use std::path::Path;
use std::process::Command;

pub fn run() -> Result<Verdict, Problem> {
    let root = workspace::root()?;
    if !root.join(".git").exists() {
        return Err(Problem(
            "`refresh` works on a repository, so run it at the top of a git checkout".to_string(),
        ));
    }
    let gh = Gh::new(&root);
    let waiting: BTreeSet<u64> = gh
        .open_issues()?
        .into_iter()
        .filter(|issue| issue.labels.iter().any(|label| label == LAND_NEEDED))
        .map(|issue| issue.number)
        .collect();
    let mut prs = gh.open_prs()?;
    prs.retain(|pr| pr.closing_issues.iter().any(|n| waiting.contains(n)));
    // GitHub numbers PRs in the order they were opened.
    prs.sort_by_key(|pr| pr.number);

    let mut verdict = Verdict::Pass;
    let mut stale = Vec::new();
    // Every stale PR goes yellow first, because the proofs run one at a time.
    for pr in prs {
        match stale_head(&root, &pr) {
            Ok(None) => println!("refresh: PR {} is up to date", pr.number),
            Ok(Some(head)) => {
                merge::post_gate(&gh, &head, "pending", &queued(stale.len()));
                stale.push(pr);
            }
            Err(Problem(message)) => {
                eprintln!("refresh: PR {}: {message}", pr.number);
                verdict = Verdict::Fail;
            }
        }
    }
    for pr in &stale {
        if !prepare(&root, pr.number)? {
            eprintln!("refresh: PR {} wasn't proven", pr.number);
            let head = current_head(&root, pr);
            // A `prepare` that fails a PR says why on its head, and one that
            // was stopped after its green did prove the PR. One that was
            // stopped earlier, or that refused the PR before the fetch, left
            // no result.
            let result = gh.status(&head, merge::GATE);
            if !matches!(result, Ok(Some(state)) if state == "failure" || state == "success") {
                merge::post_gate(&gh, &head, "failure", "Not proven");
            }
            verdict = Verdict::Fail;
        }
    }
    Ok(verdict)
}

/// Where a PR stands, by how many are proven before it.
fn queued(ahead: usize) -> String {
    match ahead {
        0 => "Queued, next".to_string(),
        1 => "Queued, 1 PR ahead".to_string(),
        more => format!("Queued, {more} PRs ahead"),
    }
}

/// The head to mark, if this PR isn't on the newest `main`. A fork's branch
/// isn't here to compare, and `prepare` refuses it anyway.
fn stale_head(root: &Path, pr: &OpenPr) -> Result<Option<String>, Problem> {
    if pr.from_fork {
        return Ok(Some(pr.head.clone()));
    }
    let fetched = merge::fetch(root, &pr.head_ref)?;
    let args = [
        "merge-base",
        "--is-ancestor",
        &fetched.main,
        &fetched.branch,
    ];
    Ok((merge::exit_code(root, &args)? != 0).then_some(fetched.branch))
}

/// A `prepare` that failed may have pushed a new head before it stopped.
fn current_head(root: &Path, pr: &OpenPr) -> String {
    if pr.from_fork {
        return pr.head.clone();
    }
    merge::remote_commit(root, &format!("refs/heads/{}", pr.head_ref))
        .ok()
        .flatten()
        .unwrap_or_else(|| pr.head.clone())
}

/// Its own process, so that one PR's timeout or crash ends only that PR.
fn prepare(root: &Path, number: u64) -> Result<bool, Problem> {
    let program =
        env::current_exe().map_err(|e| Problem(format!("couldn't find this program: {e}")))?;
    let status = Command::new(program)
        .args(["prepare", &number.to_string(), "--keep-budgets"])
        .current_dir(root)
        .status()
        .map_err(|e| Problem(format!("couldn't start `prepare`: {e}")))?;
    Ok(status.success())
}

#[cfg(test)]
mod tests {
    use super::queued;

    #[test]
    fn a_queue_position_counts_the_prs_ahead() {
        assert_eq!(queued(0), "Queued, next");
        assert_eq!(queued(1), "Queued, 1 PR ahead");
        assert_eq!(queued(2), "Queued, 2 PRs ahead");
    }
}
