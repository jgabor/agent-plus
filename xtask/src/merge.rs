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
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process;
use std::thread;
use std::time::Duration;

const AUTO_MERGE: &str = "auto:merge";
const MAIN: &str = "refs/remotes/origin/main";
const TIMEOUT_SECONDS: u64 = 30 * 60;
/// Replaces the timeout, so that a test doesn't wait for it.
const TIMEOUT_OVERRIDE: &str = "XTASK_MERGE_TIMEOUT_SECONDS";
/// Set in the worker that a merge runs in, to the id of its supervisor.
const SUPERVISOR: &str = "XTASK_MERGE_SUPERVISOR";
/// How often the worker checks that its supervisor is still there.
const WATCH_INTERVAL: Duration = Duration::from_secs(1);

enum Decision {
    Merged,
    Refused(String),
    Ready { pr: PullRequest, issue: u64 },
}

/// The one PR label that the checks read.
fn has_spec_change(pr: &PullRequest) -> bool {
    pr.labels.iter().any(|label| label == base::SPEC_CHANGE)
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
        if has_spec_change(&pr) {
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

/// Whose rules the sequence follows, and how far it goes.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// An agent's rules, through the squash.
    Merge,
    /// The owner's rules, up to the squash.
    Prepare,
    /// The owner's rules, through the squash.
    Land,
}

impl Mode {
    fn command(self) -> &'static str {
        match self {
            Mode::Merge => "merge",
            Mode::Prepare => "prepare",
            Mode::Land => "land",
        }
    }
}

/// The whole sequence, under the rules for an agent.
pub fn run(pr: &str) -> Result<Verdict, Problem> {
    sequence(pr, Mode::Merge, false)
}

/// The sequence up to the squash, which is the owner's. It also takes what
/// only the owner can accept, and says so on the PR. With `keep_budgets` it
/// runs no bench, and the new record keeps what the branch's record measured.
pub fn prepare(pr: &str, keep_budgets: bool) -> Result<Verdict, Problem> {
    sequence(pr, Mode::Prepare, keep_budgets)
}

/// `prepare`, then the squash, in one hold of the merge slot. It stops short
/// of the squash if merging would accept something, so the owner reads it.
pub fn land(pr: &str) -> Result<Verdict, Problem> {
    sequence(pr, Mode::Land, false)
}

fn sequence(pr: &str, mode: Mode, keep_budgets: bool) -> Result<Verdict, Problem> {
    let Some(supervisor) = env::var_os(SUPERVISOR) else {
        supervise()
    };
    watch_supervisor(&supervisor)?;
    let command = mode.command();
    let prepare = mode != Mode::Merge;
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
    if mode != Mode::Land
        && let Decision::Refused(why) = decide(&gh, number, prepare)?
    {
        return refuse(&why);
    }
    let slot = lock::merge_slot(&root)?;
    start_timeout();
    // After the slot, because a merge during the wait can change the checks.
    if mode == Mode::Land
        && let Some(verdict) = use_the_newest_checks(&root, number)?
    {
        return Ok(verdict);
    }
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
    let kept = if keep_budgets {
        // xtask compiled from this checkout, so these checks are for its `main`.
        if head(&root)? != fetched.main {
            unproven();
            return refuse(&format!(
                "these checks were built from an older `main`, so they can't prove PR {number} on the newest one"
            ));
        }
        if !record_at_tip(&tree.path, issue)? {
            unproven();
            return refuse(&format!(
                "the tip of PR {number} has no ledger record, so there are no budgets to keep. Run `cargo xtask prepare {number}`"
            ));
        }
        Some(ledger::read(&tree.path, issue as usize).inspect_err(|_| unproven())?)
    } else {
        None
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
        kept: kept.as_ref(),
    };
    let proof = prove(&tree.path, &scoring).inspect_err(|_| unproven())?;
    let Some(Proof { record, accepted }) = proof else {
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
    if mode == Mode::Prepare {
        println!("prepare: PR {number} is ready to merge at {recorded}");
        return Ok(Verdict::Pass);
    }
    if !accepted.is_empty() {
        eprintln!("land: PR {number} is ready at {recorded}, but merging it accepts:");
        for item in &accepted {
            eprintln!("- {item}");
        }
        return refuse("read that, and use a merge button if you accept it");
    }
    gh.merge_pr(number, &recorded, &slot)?;
    println!("{command}: PR {number} merged at {recorded}");
    Ok(Verdict::Pass)
}

/// xtask compiles from this checkout, so an old one would run old checks. If
/// `main` is behind, this updates it and starts `land` again from the new
/// code, which takes the slot again. Returns a verdict if `land` must stop.
fn use_the_newest_checks(root: &Path, number: u64) -> Result<Option<Verdict>, Problem> {
    let stop = |why: &str| refuse(why).map(Some);
    base::git_text(
        root,
        &["fetch", "-q", "origin", &format!("+refs/heads/main:{MAIN}")],
    )?;
    if exit_code(root, &["diff", "--quiet", "HEAD"])? != 0 {
        return stop(
            "this checkout has changes to tracked files, and the checks run from it. Commit or discard them",
        );
    }
    let newest = base::git_text(root, &["rev-parse", MAIN])?;
    if head(root)? == newest.trim() {
        return Ok(None);
    }
    if base::git_text(root, &["branch", "--show-current"])?.trim() != "main" {
        return stop(
            "this checkout isn't on `main`, and isn't at `origin/main`. Run this on `main`",
        );
    }
    if exit_code(root, &["merge-base", "--is-ancestor", "HEAD", MAIN])? != 0 {
        return stop(
            "`main` here has local commits, and the checks run from it. Push or drop them",
        );
    }
    base::git_text(root, &["merge", "-q", "--ff-only", MAIN])?;
    // Rustup sets this for its children, and it would outrank a toolchain file
    // that the update just changed.
    let problem = process::Command::new("cargo")
        .args(["xtask", "land", &number.to_string()])
        .current_dir(root)
        .env_remove("RUSTUP_TOOLCHAIN")
        .exec();
    Err(Problem(format!(
        "couldn't restart `cargo xtask land`: {problem}"
    )))
}

/// For the git commands that answer yes or no by exiting 0 or 1. Anything else
/// is git failing, which isn't a no.
pub fn exit_code(root: &Path, args: &[&str]) -> Result<i32, Problem> {
    let output = base::git(root, args)?;
    match output.status.code() {
        Some(code @ (0 | 1)) => Ok(code),
        _ => Err(Problem(format!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))),
    }
}

/// A status that can't be posted leaves the verdict as it is.
pub fn post_failure(gh: &Gh, commit: &str) {
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

/// Runs this merge again as a worker in a process group of its own, and stops
/// the group when the worker ends, however it ends. Everything the merge starts
/// joins the group, so nothing it started outlives it, even a command that
/// ignores its parent's exit. Only `gh pr merge` leaves it, to keep the slot.
fn supervise() -> ! {
    let started = env::current_exe().and_then(|program| {
        process::Command::new(program)
            .args(env::args_os().skip(1))
            .env(SUPERVISOR, process::id().to_string())
            .process_group(0)
            .spawn()
    });
    let mut worker = match started {
        Ok(worker) => worker,
        Err(error) => {
            eprintln!("error: couldn't start the merge: {error}");
            process::exit(2);
        }
    };
    let group = worker.id();
    let status = worker.wait();
    stop_group(group);
    match status {
        Ok(status) => {
            if let Some(code) = status.code() {
                process::exit(code);
            }
            // Dies as the worker did, so that a killed merge looks killed.
            if let Some(signal) = status.signal() {
                let _ = process::Command::new("kill")
                    .arg(format!("-{signal}"))
                    .arg(process::id().to_string())
                    .status();
                process::exit(128 + signal);
            }
            process::exit(1);
        }
        Err(error) => {
            eprintln!("error: couldn't wait for the merge: {error}");
            process::exit(2);
        }
    }
}

/// Ctrl-C reaches only the supervisor's process group, so a worker whose
/// supervisor died stops its own group. The group's leader is the worker, or
/// the `cargo` that `land` restarts as, and its parent is the supervisor.
fn watch_supervisor(supervisor: &std::ffi::OsStr) -> Result<(), Problem> {
    let unknown = || Problem(format!("{SUPERVISOR} must be a process id"));
    let supervisor: u32 = supervisor
        .to_str()
        .and_then(|text| text.parse().ok())
        .ok_or_else(unknown)?;
    let (_, group) = parent_and_group("self")
        .ok_or_else(|| Problem("couldn't read this process's group".to_string()))?;
    thread::spawn(move || {
        loop {
            thread::sleep(WATCH_INTERVAL);
            let leader = parent_and_group(&group.to_string()).map(|(parent, _)| parent);
            if leader != Some(supervisor) {
                eprintln!("merge: stopped, because the command that started it stopped");
                stop_group(group);
            }
        }
    });
    Ok(())
}

/// A process's parent and process group, from `/proc`. The name can hold any
/// character, so the fields are read after its closing parenthesis.
fn parent_and_group(pid: &str) -> Option<(u32, u32)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    let mut fields = rest.split_whitespace().skip(1);
    let parent = fields.next()?.parse().ok()?;
    let group = fields.next()?.parse().ok()?;
    Some((parent, group))
}

/// `xtask` has no dependencies, so it can't signal a group itself. An empty
/// group is already stopped, so `kill` failing on one is no problem.
fn stop_group(group: u32) {
    let killed = process::Command::new("kill")
        .args(["-KILL", "--", &format!("-{group}")])
        .stdout(process::Stdio::null())
        .stderr(process::Stdio::null())
        .status();
    if let Err(error) = killed {
        eprintln!("merge: couldn't stop process group {group}: {error}");
    }
}

/// The commits the merge started from.
pub struct Fetched {
    pub main: String,
    pub branch: String,
}

/// Updates the two remote-tracking branches, and says where they are now.
/// Other worktrees can fetch too, so the merge doesn't read these refs again.
pub fn fetch(root: &Path, branch: &str) -> Result<Fetched, Problem> {
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
pub fn remote_commit(root: &Path, name: &str) -> Result<Option<String>, Problem> {
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
                || has_spec_change(&pr) != has_spec_change(was)
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
    if record_at_tip(tree, issue)? {
        base::git_text(tree, &["reset", "-q", "--hard", "HEAD~1"])?;
    }
    Ok(())
}

/// Whether the newest commit does nothing but write this issue's record.
fn record_at_tip(tree: &Path, issue: u64) -> Result<bool, Problem> {
    let args = ["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"];
    let touched = base::git_text(tree, &args)?;
    Ok(touched.trim() == ledger::path(issue as usize))
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
    /// The record whose budgets stand in for a bench.
    kept: Option<&'a Record>,
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
        && scoring.kept.is_none()
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
    let mut record = ledger::compose(
        tree,
        scoring.issue as usize,
        scoring.number as usize,
        scoring.gated,
        Some(&commit),
        scores,
    )?;
    if let Some(kept) = scoring.kept {
        record.budgets.extend(kept.budgets.clone());
        record.reason = kept.reason.clone();
    }
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
/// from, how it is built, what it boots, or how a budget is measured.
fn budget_can_change(tree: &Path, commit: &str) -> Result<bool, Problem> {
    let args = ["diff", "--name-only", "--no-renames", "-z", commit, "HEAD"];
    let changed = base::git_text(tree, &args)?;
    Ok(changed.split('\0').any(|path| {
        path.starts_with("crates/")
            || path.starts_with(".cargo/")
            || path.starts_with("guest/kernel/")
            || [
                "Cargo.toml",
                "Cargo.lock",
                "rust-toolchain.toml",
                "harness/budgets.toml",
                "xtask/src/bench.rs",
            ]
            .contains(&path)
    }))
}
