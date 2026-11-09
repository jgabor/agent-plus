//! `cargo xtask merge`: the merge sequence in HARNESS.md.
//!
//! Each step can die and be run again. Everything before the push happens in a
//! throwaway worktree, a rerun starts from a new fetch, and a record that an
//! earlier attempt pushed is dropped and written again.

use crate::forge::{Forge, Gh, PullRequest};
use crate::gate::{self, Merging};
use crate::ledger::{self, Record, Scores};
use crate::{Problem, Verdict, base, bench, journey, lock, workspace};
use std::env;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
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
fn decide(gh: &Gh, number: u64, prepare: bool) -> Result<Decision, Problem> {
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
    if pr.from_fork {
        return refused(format!(
            "PR {number} comes from a fork, and its branch isn't one this merge can rebase and push"
        ));
    }
    if pr.base_ref != "main" {
        return refused(format!(
            "PR {number} targets `{}`, and only `main` is merged here",
            pr.base_ref
        ));
    }
    let [issue] = pr.closing_issues[..] else {
        return refused(format!(
            "the body of PR {number} needs one `Closes #<issue>` line, because the ledger records one issue per merge"
        ));
    };
    if pr.changes_requested {
        return refused(format!(
            "a review of PR {number} requested changes. Address it, or dismiss it on GitHub"
        ));
    }
    if !prepare {
        if pr.labels.iter().any(|label| label == base::SPEC_CHANGE) {
            return refused(format!(
                "PR {number} is labeled `{}`, so the owner merges it. Run `cargo xtask prepare {number}` instead",
                base::SPEC_CHANGE
            ));
        }
        if !gh.issue(issue)?.labels.iter().any(|l| l == AUTO_MERGE) {
            return refused(format!(
                "issue {issue} has no `{AUTO_MERGE}` label, so the owner merges PR {number}. Run `cargo xtask prepare {number}` instead"
            ));
        }
    }
    Ok(Decision::Ready { pr, issue })
}

fn refuse(why: &str) -> Result<Verdict, Problem> {
    eprintln!("merge: {why}");
    Ok(Verdict::Fail)
}

/// The whole sequence, under the rules for an agent.
pub fn run(pr: &str) -> Result<Verdict, Problem> {
    sequence(pr, false)
}

/// The sequence up to the squash, which is the owner's. It also takes what
/// only the owner can accept, and says so on the PR.
pub fn prepare(pr: &str) -> Result<Verdict, Problem> {
    sequence(pr, true)
}

fn sequence(pr: &str, prepare: bool) -> Result<Verdict, Problem> {
    let command = if prepare { "prepare" } else { "merge" };
    let number: u64 = pr
        .parse()
        .map_err(|_| Problem(format!("`{command}` takes a PR number, but got `{pr}`")))?;
    let root = workspace::root()?;
    if !root.join(".git").exists() {
        return Err(Problem(format!(
            "`{command}` works on a repository, so run it at the top of a git checkout"
        )));
    }
    let gh = Gh::new(&root);
    if let Decision::Refused(why) = decide(&gh, number, prepare)? {
        return refuse(&why);
    }
    let slot = lock::merge_slot(&root)?;
    start_timeout();
    let (pr, issue) = match decide(&gh, number, prepare)? {
        Decision::Merged => {
            Tree::clear(&root, &Tree::path(&root, number)?);
            println!("merge: PR {number} is already merged");
            return Ok(Verdict::Pass);
        }
        Decision::Refused(why) => return refuse(&why),
        Decision::Ready { pr, issue } => (pr, issue),
    };

    let fetched = fetch(&root, &pr.head_ref)?;
    let tree = Tree::create(&root, number, &fetched.branch)?;
    // Nothing was pushed, so this is the head that an earlier green would be on.
    let unproven = || {
        if prepare {
            post_failure(&gh, &fetched.branch);
        }
    };
    drop_earlier_record(&tree.path, issue)?;
    if !rebase(&tree.path, &fetched.main)? {
        unproven();
        return refuse("the branch doesn't rebase onto `main`. Rebase it by hand, and push it");
    }
    let gated = head(&tree.path)?;
    let scoring = Scoring {
        number,
        issue,
        prepare,
        pr: &pr,
        gated: &gated,
    };
    let Some(Proof { record, accepted }) = prove(&tree.path, &scoring)? else {
        unproven();
        return Ok(Verdict::Fail);
    };

    if let Some(why) = changed_during_proof(&gh, &root, &scoring, &fetched.main)? {
        return refuse(&why);
    }

    let path = ledger::store(&tree.path, &record)?;
    base::git_text(&tree.path, &["add", "--", &path])?;
    let message = format!("Record issue {issue} in the ledger");
    base::git_text(&tree.path, &["commit", "-q", "-m", &message])?;
    let recorded = head(&tree.path)?;
    let lease = format!(
        "--force-with-lease=refs/heads/{}:{}",
        pr.head_ref, fetched.branch
    );
    let target = format!("HEAD:refs/heads/{}", pr.head_ref);
    base::git_text(&tree.path, &["push", "-q", &lease, "origin", &target])?;
    let changed = base::git_text(&tree.path, &["diff", "--name-only", &gated, &recorded])?;
    if changed.trim() != path {
        return Err(Problem(format!(
            "{recorded} should differ from the gated head {gated} only by {path}, but it changes:\n{}",
            changed.trim_end()
        )));
    }
    // The comment comes first, so that a green PR always has it.
    if !accepted.is_empty() {
        gh.comment_on_pr(number, &acceptance(&recorded, &accepted))?;
    }
    gh.post_status(&recorded, "xtask/gate", "success")?;
    if prepare {
        println!("prepare: PR {number} is ready to merge at {recorded}");
        return Ok(Verdict::Pass);
    }
    gh.merge_pr(number, &recorded, &slot)?;
    println!("merge: PR {number} merged at {recorded}");
    Ok(Verdict::Pass)
}

/// A status that can't be posted leaves the verdict as it is.
fn post_failure(gh: &Gh, commit: &str) {
    if let Err(Problem(message)) = gh.post_status(commit, "xtask/gate", "failure") {
        eprintln!("merge: couldn't post `xtask/gate` on {commit}: {message}");
    }
}

fn acceptance(head: &str, accepted: &[String]) -> String {
    let lines: Vec<String> = accepted.iter().map(|item| format!("- {item}")).collect();
    format!(
        "Merging this PR at `{head}` accepts:\n\n{}\n",
        lines.join("\n")
    )
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

/// The commits the merge started from.
struct Fetched {
    main: String,
    branch: String,
}

/// Updates the two remote-tracking branches, and says where they are now.
/// Other worktrees can fetch too, so the merge doesn't read these refs again.
fn fetch(root: &Path, branch: &str) -> Result<Fetched, Problem> {
    let remote = format!("refs/remotes/origin/{branch}");
    let main = format!("+refs/heads/main:{MAIN}");
    let spec = format!("+refs/heads/{branch}:{remote}");
    base::git_text(root, &["fetch", "-q", "origin", &main, &spec])?;
    let resolve = |name: &str| -> Result<String, Problem> {
        Ok(base::git_text(root, &["rev-parse", name])?
            .trim()
            .to_string())
    };
    Ok(Fetched {
        main: resolve(MAIN)?,
        branch: resolve(&remote)?,
    })
}

/// Where a ref is on GitHub now, or `None` if it isn't there.
fn remote_commit(root: &Path, name: &str) -> Result<Option<String>, Problem> {
    let listing = base::git_text(root, &["ls-remote", "origin", name])?;
    Ok(listing.split_whitespace().next().map(str::to_string))
}

/// The proof takes minutes, and meanwhile a label or a review can change, or
/// an earlier merge that was killed can land. Nothing on GitHub enforces these
/// rules afterwards, so this is the last place to stop. Says why, if it must.
fn changed_during_proof(
    gh: &Gh,
    root: &Path,
    scoring: &Scoring,
    main: &str,
) -> Result<Option<String>, Problem> {
    let number = scoring.number;
    match decide(gh, number, scoring.prepare)? {
        Decision::Refused(why) => return Ok(Some(why)),
        Decision::Merged => {
            return Ok(Some(format!(
                "PR {number} was merged while this merge was running"
            )));
        }
        Decision::Ready { pr, issue } => {
            let was = scoring.pr;
            if issue != scoring.issue
                || pr.head_ref != was.head_ref
                || pr.body != was.body
                || pr.labels != was.labels
            {
                return Ok(Some(format!(
                    "PR {number} changed during the merge, and the checks read what it said before. Run it again"
                )));
            }
        }
    }
    if remote_commit(root, "refs/heads/main")?.as_deref() != Some(main) {
        return Ok(Some(
            "`main` moved during the merge, so this head wasn't tested on it. Run it again"
                .to_string(),
        ));
    }
    Ok(None)
}

/// A detached worktree for the rebase and the gate, so that the merge leaves
/// the caller's checkout alone. It goes away with the merge. It sits outside
/// the checkout, because Cargo reads `.cargo/config.toml` from every parent
/// directory, and the settings of the caller's checkout aren't this commit's.
pub struct Tree {
    root: PathBuf,
    pub path: PathBuf,
}

impl Tree {
    /// Named by repository, so that a rerun from another of its worktrees
    /// finds and clears what an earlier attempt left.
    fn path(root: &Path, number: u64) -> Result<PathBuf, Problem> {
        let mut hasher = DefaultHasher::new();
        lock::directory(root)?.hash(&mut hasher);
        let name = format!("xtask-merge-{:016x}-{number}", hasher.finish());
        Ok(bench::temp_outside(root)?.join(name))
    }

    /// Starts from nothing, because an earlier attempt may have died at any
    /// point in this worktree.
    pub fn create(root: &Path, number: u64, commit: &str) -> Result<Tree, Problem> {
        let path = Tree::path(root, number)?;
        Tree::clear(root, &path);
        let name = path.to_string_lossy();
        base::git_text(root, &["worktree", "add", "-q", "--detach", &name, commit])?;
        Ok(Tree {
            root: root.to_path_buf(),
            path,
        })
    }

    /// Does what it can, and the next attempt clears the rest. `--force` twice
    /// is what removes a locked worktree, which a killed `git worktree add`
    /// leaves.
    fn clear(root: &Path, path: &Path) {
        let name = path.to_string_lossy();
        let args = ["worktree", "remove", "--force", "--force", &name];
        let _ = base::git(root, &args);
        let _ = fs::remove_dir_all(path);
        let _ = base::git(root, &["worktree", "prune"]);
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        Tree::clear(&self.root, &self.path);
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
fn rebase(tree: &Path, main: &str) -> Result<bool, Problem> {
    let output = base::git(tree, &["rebase", "-q", main])?;
    if !output.status.success() {
        eprintln!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(output.status.success())
}

struct Scoring<'a> {
    number: u64,
    issue: u64,
    prepare: bool,
    pr: &'a PullRequest,
    gated: &'a str,
}

/// What the proof earned, and what merging it accepts.
struct Proof {
    record: Record,
    accepted: Vec<String>,
}

/// Runs the gate, the journey, and any bench on the rebased head, and returns
/// the record they earn. `None` means the merge must stop, and the reasons
/// have been printed.
fn prove(tree: &Path, scoring: &Scoring) -> Result<Option<Proof>, Problem> {
    let Scoring { prepare, pr, .. } = *scoring;
    let hold = lock::shared(tree)?;
    let merging = Merging {
        labels: &pr.labels,
        prepare,
    };
    let (verdict, score) = gate::check(tree, &hold, Some(&merging))?;
    if verdict == Verdict::Fail {
        return Ok(None);
    }
    let (passing, steps) = journey::count(tree)?;
    drop(hold);

    let commit = base::find(tree)?
        .ok_or_else(|| Problem("there is no origin/main after the fetch".to_string()))?;
    // The base of a revert may be what doesn't build, and a revert puts back a
    // tree that was already measured.
    let revert = prepare && base::is_exact_revert(tree, &commit)?;
    if !revert
        && budget_can_change(tree, &commit)?
        && bench::compare(tree, Some(&pr.body))? == Verdict::Fail
    {
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
        Some(&commit),
        scores,
    )?;
    let Some(mut accepted) = compare_counts(tree, &commit, &record, scoring)? else {
        return Ok(None);
    };
    if revert {
        accepted
            .push("an exact revert, so it can remove checks, and the bench didn't run".to_string());
    }
    Ok(Some(Proof { record, accepted }))
}

/// The spec count is compared by the gate. This compares the two that need the
/// journey and the bench, and then asks whether anything rose. Returns the
/// counts that fell and are allowed to, or `None` if the PR fails here.
fn compare_counts(
    tree: &Path,
    commit: &str,
    record: &Record,
    scoring: &Scoring,
) -> Result<Option<Vec<String>>, Problem> {
    let budgets = bench::read_budgets(tree)?;
    let before = ledger::newest(tree, commit)?;
    let (proven, passing, budgets_met) = before.map_or((0, 0, 0), |base| {
        let met = bench::met(&budgets, &base.budgets).0;
        (base.proven, base.passing, met)
    });
    let met_now = bench::met(&budgets, &record.budgets).0;

    let mut verdict = Verdict::Pass;
    let mut accepted = Vec::new();
    if record.proven < proven {
        accepted.push(format!("spec fell from {proven} to {}", record.proven));
    }
    for (name, before, after) in [
        ("journey", passing, record.passing),
        ("budgets", budgets_met, met_now),
    ] {
        if after < before {
            eprintln!(
                "{name}: the count fell from {before} to {after}. Restore it, or hand the PR to the owner"
            );
            accepted.push(format!("{name} fell from {before} to {after}"));
        }
        gate::record(name, after >= before || scoring.prepare, &mut verdict);
    }
    let rose = record.proven > proven || record.passing > passing || met_now > budgets_met;
    if !rose && !declares_no_hill(&scoring.pr.body) {
        eprintln!(
            "hills: no count rose, so the PR body needs a `Hill: none, <reason>` line, or a change that raises one"
        );
        gate::record("hill-line", false, &mut verdict);
    }
    Ok((verdict == Verdict::Pass).then_some(accepted))
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
