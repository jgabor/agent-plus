//! `cargo xtask merge`: the merge sequence in HARNESS.md.
//!
//! Each step can die and be run again. Everything before the push happens in a
//! throwaway worktree, a rerun starts from a new fetch, and a record that an
//! earlier attempt pushed is dropped and written again.

use crate::forge::{Forge, Gh, PullRequest};
use crate::gate::{self, Merging};
use crate::ledger::{self, Record, Scores};
use crate::{Problem, Verdict, base, bench, journey, lock, work, workspace};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::thread;
use std::time::Duration;

const AUTO_MERGE: &str = "auto:merge";
const MAIN: &str = "refs/remotes/origin/main";
const TIMEOUT_SECONDS: u64 = 30 * 60;
/// Replaces the timeout, so that a test doesn't wait for it.
const TIMEOUT_OVERRIDE: &str = "XTASK_MERGE_TIMEOUT_SECONDS";

enum Decision {
    Merged,
    Refused(String),
    Ready { pr: PullRequest, issue: u64 },
}

/// Whether this PR may merge, from what GitHub says now. It writes nothing.
fn decide(gh: &Gh, number: u64, owner: bool) -> Result<Decision, Problem> {
    let pr = gh.pull_request(number)?;
    match pr.state.as_str() {
        "MERGED" => return Ok(Decision::Merged),
        "OPEN" => {}
        other => {
            return Err(Problem(format!(
                "PR {number} is {other}, so there is nothing to merge"
            )));
        }
    }
    let refused = |why: String| Ok(Decision::Refused(why));
    if pr.base_ref != "main" {
        return refused(format!(
            "PR {number} targets `{}`, and only `main` is merged here",
            pr.base_ref
        ));
    }
    let issues = work::references(&pr.body, "Closes");
    let [issue] = issues[..] else {
        return refused(format!(
            "the body of PR {number} needs one `Closes #<issue>` line, because the ledger records one issue per merge"
        ));
    };
    if pr.review_decision == "CHANGES_REQUESTED" {
        return refused(format!(
            "a review of PR {number} requested changes. Address it, or dismiss it on GitHub"
        ));
    }
    if !owner {
        if pr.labels.iter().any(|label| label == base::SPEC_CHANGE) {
            return refused(format!(
                "PR {number} is labeled `{}`, so only the owner merges it, with `--owner`",
                base::SPEC_CHANGE
            ));
        }
        if !gh.issue(issue)?.labels.iter().any(|l| l == AUTO_MERGE) {
            return refused(format!(
                "issue {issue} has no `{AUTO_MERGE}` label, so only the owner merges PR {number}, with `--owner`"
            ));
        }
    }
    Ok(Decision::Ready { pr, issue })
}

fn refuse(why: &str) -> Result<Verdict, Problem> {
    eprintln!("merge: {why}");
    Ok(Verdict::Fail)
}

pub fn run(pr: &str, owner: bool) -> Result<Verdict, Problem> {
    let number: u64 = pr
        .parse()
        .map_err(|_| Problem(format!("`merge` takes a PR number, but got `{pr}`")))?;
    let root = workspace::root()?;
    if !root.join(".git").exists() {
        return Err(Problem(
            "`merge` works on a repository, so run it at the top of a git checkout".to_string(),
        ));
    }
    let gh = Gh::new(&root);
    if let Decision::Refused(why) = decide(&gh, number, owner)? {
        return refuse(&why);
    }
    let _slot = lock::merge_slot(&root)?;
    start_timeout();
    let (pr, issue) = match decide(&gh, number, owner)? {
        Decision::Merged => {
            Tree::clear(&root, number);
            println!("merge: PR {number} is already merged");
            return Ok(Verdict::Pass);
        }
        Decision::Refused(why) => return refuse(&why),
        Decision::Ready { pr, issue } => (pr, issue),
    };

    let fetched = fetch(&root, &pr.head_ref)?;
    let tree = Tree::create(&root, number, &fetched)?;
    drop_earlier_record(&tree.path, issue)?;
    if !rebase(&tree.path)? {
        return refuse("the branch doesn't rebase onto `main`. Rebase it by hand, and push it");
    }
    let gated = head(&tree.path)?;
    let scoring = Scoring {
        number,
        issue,
        owner,
        pr: &pr,
        gated: &gated,
    };
    let Some(record) = prove(&tree.path, &scoring)? else {
        return Ok(Verdict::Fail);
    };

    let path = ledger::store(&tree.path, &record)?;
    base::git_text(&tree.path, &["add", "--", &path])?;
    let message = format!("Record issue {issue} in the ledger");
    base::git_text(&tree.path, &["commit", "-q", "-m", &message])?;
    let recorded = head(&tree.path)?;
    let lease = format!("--force-with-lease=refs/heads/{0}:{fetched}", pr.head_ref);
    let target = format!("HEAD:refs/heads/{}", pr.head_ref);
    base::git_text(&tree.path, &["push", "-q", &lease, "origin", &target])?;
    let changed = base::git_text(&tree.path, &["diff", "--name-only", &gated, &recorded])?;
    if changed.trim() != path {
        return Err(Problem(format!(
            "{recorded} should differ from the gated head {gated} only by {path}, but it changes:\n{}",
            changed.trim_end()
        )));
    }
    gh.post_status(&recorded, "success")?;
    gh.merge_pr(number, &recorded)?;
    println!("merge: PR {number} merged at {recorded}");
    Ok(Verdict::Pass)
}

/// Stops a stuck merge, because it would hold the slot for every other merge.
/// The kernel releases the slot when the process exits.
fn start_timeout() {
    let seconds = env::var(TIMEOUT_OVERRIDE)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(TIMEOUT_SECONDS);
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(seconds));
        eprintln!("merge: stopped at the {seconds} second timeout. Run it again to restart");
        process::exit(1);
    });
}

/// Updates the two remote-tracking branches, and returns the branch's commit.
fn fetch(root: &Path, branch: &str) -> Result<String, Problem> {
    let remote = format!("refs/remotes/origin/{branch}");
    let main = format!("+refs/heads/main:{MAIN}");
    let spec = format!("+refs/heads/{branch}:{remote}");
    base::git_text(root, &["fetch", "-q", "origin", &main, &spec])?;
    let commit = base::git_text(root, &["rev-parse", &remote])?;
    Ok(commit.trim().to_string())
}

/// A detached worktree for the rebase and the gate, so that the merge leaves
/// the caller's checkout alone. It goes away with the merge.
struct Tree {
    root: PathBuf,
    number: u64,
    path: PathBuf,
}

impl Tree {
    fn path(root: &Path, number: u64) -> PathBuf {
        root.join("target/merge").join(number.to_string())
    }

    /// Starts from nothing, because an earlier attempt may have died at any
    /// point in this worktree.
    fn create(root: &Path, number: u64, commit: &str) -> Result<Tree, Problem> {
        Tree::clear(root, number);
        let path = Tree::path(root, number);
        let parent = path.parent().expect("the path has a parent");
        fs::create_dir_all(parent)
            .map_err(|e| Problem(format!("couldn't create {}: {e}", parent.display())))?;
        let name = path.to_string_lossy();
        base::git_text(root, &["worktree", "add", "-q", "--detach", &name, commit])?;
        Ok(Tree {
            root: root.to_path_buf(),
            number,
            path,
        })
    }

    /// Does what it can. What remains is under `target/`, and the next attempt
    /// clears it. `--force` twice is what removes a locked worktree, which a
    /// killed `git worktree add` leaves.
    fn clear(root: &Path, number: u64) {
        let path = Tree::path(root, number);
        let name = path.to_string_lossy();
        let args = ["worktree", "remove", "--force", "--force", &name];
        let _ = base::git(root, &args);
        let _ = fs::remove_dir_all(&path);
        let _ = base::git(root, &["worktree", "prune"]);
        if let Some(parent) = path.parent() {
            let _ = fs::remove_dir(parent);
        }
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        Tree::clear(&self.root, self.number);
    }
}

fn head(tree: &Path) -> Result<String, Problem> {
    Ok(base::git_text(tree, &["rev-parse", "HEAD"])?
        .trim()
        .to_string())
}

/// A record at the tip came from an earlier attempt. It describes a head that
/// is about to change, and the merge writes a new one.
fn drop_earlier_record(tree: &Path, issue: u64) -> Result<(), Problem> {
    let args = ["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"];
    let touched = base::git_text(tree, &args)?;
    if touched.trim() == ledger::path(issue as usize) {
        base::git_text(tree, &["reset", "-q", "--hard", "HEAD~1"])?;
    }
    Ok(())
}

/// Whether the rebase worked. A failed one is left for the worktree's removal
/// to clean up, so the branch on GitHub never sees it.
fn rebase(tree: &Path) -> Result<bool, Problem> {
    let output = base::git(tree, &["rebase", "-q", MAIN])?;
    if !output.status.success() {
        eprintln!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(output.status.success())
}

struct Scoring<'a> {
    number: u64,
    issue: u64,
    owner: bool,
    pr: &'a PullRequest,
    gated: &'a str,
}

/// Runs the gate, the journey, and any bench on the rebased head, and returns
/// the record they earn. `None` means the merge must stop, and the reasons
/// have been printed.
fn prove(tree: &Path, scoring: &Scoring) -> Result<Option<Record>, Problem> {
    let Scoring { owner, pr, .. } = *scoring;
    let hold = lock::shared(tree)?;
    let merging = Merging {
        labels: &pr.labels,
        owner,
    };
    let (verdict, score) = gate::check(tree, &hold, Some(&merging))?;
    if verdict == Verdict::Fail {
        return Ok(None);
    }
    let (passing, steps) = journey::count(tree)?;
    drop(hold);

    let commit = base::find(tree)?
        .ok_or_else(|| Problem("there is no origin/main after the fetch".to_string()))?;
    if budget_can_change(tree, &commit)? && bench::compare(tree, Some(&pr.body))? == Verdict::Fail {
        return Ok(None);
    }
    let scores = Scores {
        proven: score.proven,
        promises: score.promises.len(),
        passing,
        steps,
    };
    let record = ledger::compose(
        tree,
        scoring.issue as usize,
        scoring.number as usize,
        scoring.gated,
        scores,
    )?;
    Ok(compare_counts(tree, &commit, &record, scoring)?.then_some(record))
}

/// The spec count is compared by the gate. This compares the two that need the
/// journey and the bench, and then asks whether anything rose.
fn compare_counts(
    tree: &Path,
    commit: &str,
    record: &Record,
    scoring: &Scoring,
) -> Result<bool, Problem> {
    let budgets = bench::read_budgets(tree)?;
    let before = ledger::newest(tree, commit)?;
    let (proven, passing, budgets_met) = before.map_or((0, 0, 0), |base| {
        let met = bench::met(&budgets, &base.budgets).0;
        (base.proven, base.passing, met)
    });
    let met_now = bench::met(&budgets, &record.budgets).0;

    let mut verdict = Verdict::Pass;
    for (name, before, after) in [
        ("journey", passing, record.passing),
        ("budgets", budgets_met, met_now),
    ] {
        if after < before {
            eprintln!(
                "{name}: the count fell from {before} to {after}. Restore it, or hand the PR to the owner"
            );
        }
        gate::record(name, after >= before || scoring.owner, &mut verdict);
    }
    let rose = record.proven > proven || record.passing > passing || met_now > budgets_met;
    if !rose && !declares_no_hill(&scoring.pr.body) {
        eprintln!(
            "hills: no count rose, so the PR body needs a `Hill: none, <reason>` line, or a change that raises one"
        );
        gate::record("hill-line", false, &mut verdict);
    }
    Ok(verdict == Verdict::Pass)
}

fn declares_no_hill(body: &str) -> bool {
    body.lines()
        .filter_map(|line| line.trim().strip_prefix("Hill: none,"))
        .any(|reason| !reason.trim().is_empty())
}

/// Whether the change can move a budget: the product's code, what it is built
/// from, or how it is built.
fn budget_can_change(tree: &Path, commit: &str) -> Result<bool, Problem> {
    let args = ["diff", "--name-only", "--no-renames", "-z", commit, "HEAD"];
    let changed = base::git_text(tree, &args)?;
    Ok(changed.split('\0').any(|path| {
        path.starts_with("crates/")
            || path.starts_with(".cargo/")
            || ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml"].contains(&path)
    }))
}
