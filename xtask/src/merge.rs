//! `cargo xtask merge`: the merge sequence in HARNESS.md.
//!
//! Each step can die and be run again. Everything before the push happens in a
//! throwaway worktree, a rerun starts from a new fetch, and a record that an
//! earlier attempt pushed is dropped and written again.

use crate::forge::{Forge, Gh, PullRequest};
use crate::gate::{self, Failed, Merging};
use crate::ledger::{self, Record, Scores};
use crate::{Problem, Verdict, base, bench, journey, lock, work, workspace};
use std::cell::Cell;
use std::env;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

const AUTO_MERGE: &str = "auto:merge";
/// The one status that `main` requires.
pub const GATE: &str = "xtask/gate";
const MAIN: &str = "refs/remotes/origin/main";
const TIMEOUT_SECONDS: u64 = 30 * 60;
/// Replaces the timeout, so that a test doesn't wait for it.
const TIMEOUT_OVERRIDE: &str = "XTASK_MERGE_TIMEOUT_SECONDS";
/// Set in the worker that a merge runs in, to the id of the worker's parent,
/// or of the worker itself once it runs the xtask of `main`.
const WORKER_OF: &str = "XTASK_MERGE_WORKER_OF";
/// Stops the worker's group when the supervisor dies, as on Ctrl-C, which
/// reaches only the supervisor's process group. Only the supervisor holds the
/// other end of its input, so the input ends when the supervisor does. It runs
/// the same `kill` as `stop_group`, because dash's builtin refuses `--`.
const WATCHER: &str = "read _; env kill -STOP -- \"-$1\"; env kill -KILL -- \"-$1\"";
/// Set in a merge that runs the xtask of `main`, to that `main`.
const CHECKS_OF: &str = "XTASK_MERGE_CHECKS_OF";
/// Where a PR's own xtask, run in its rebased tree, finds the record whose
/// budgets it keeps, and leaves its result.
const OWN: &str = "target/xtask-own";

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
/// `merge` and `prepare` refuse a thread that doesn't hold the PR's issue.
fn decide(gh: &Gh, number: u64, mode: Mode, thread: Option<&str>) -> Result<Decision, Problem> {
    let prepare = mode != Mode::Merge;
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
    if mode != Mode::Land
        && let Some(why) = work::refusal(
            &gh.issue(issue)?,
            &gh.owner()?,
            work::now_seconds()?,
            thread,
        )
    {
        return refused(why);
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
pub fn run(pr: &str, thread: Option<&str>) -> Result<Verdict, Problem> {
    sequence(pr, Mode::Merge, false, thread)
}

/// The sequence up to the squash, which is the owner's. It also takes what
/// only the owner can accept, and says so on the PR. With `keep_budgets` it
/// runs no bench, and the new record keeps what the branch's record measured.
pub fn prepare(pr: &str, keep_budgets: bool, thread: Option<&str>) -> Result<Verdict, Problem> {
    sequence(pr, Mode::Prepare, keep_budgets, thread)
}

/// `prepare`, then the squash, in one hold of the merge slot. It stops short
/// of the squash if merging would accept something, so the owner reads it.
pub fn land(pr: &str) -> Result<Verdict, Problem> {
    sequence(pr, Mode::Land, false, None)
}

fn sequence(
    pr: &str,
    mode: Mode,
    keep_budgets: bool,
    thread: Option<&str>,
) -> Result<Verdict, Problem> {
    if !is_worker() {
        supervise();
    }
    let command = mode.command();
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
        && let Decision::Refused(why) = decide(&gh, number, mode, thread)?
    {
        return refuse(&why);
    }
    let slot = lock::merge_slot(&root)?;
    report_slot()?;
    let (pr, issue) = match decide(&gh, number, mode, thread)? {
        Decision::Merged => {
            Tree::clear(&root, &Tree::path(&root, number)?);
            println!("merge: PR {number} is already merged");
            return Ok(Verdict::Pass);
        }
        Decision::Refused(why) => return refuse(&why),
        Decision::Ready { pr, issue } => (pr, issue),
    };

    let fetched = fetch(&root, &pr.head_ref)?;
    // After the slot and the fetch, because a merge during the wait can change
    // the checks.
    if env::var(CHECKS_OF).ok().as_deref() != Some(fetched.main.as_str()) {
        return run_the_checks_of(&root, &fetched.main);
    }
    let tree = Tree::create(&root, number, &fetched.branch)?;
    // Nothing was pushed, so this is the head that the PR shows.
    let unproven = |why: &str| {
        post_gate(&gh, &fetched.branch, "failure", why);
        refuse(why)
    };
    let kept = if keep_budgets {
        if !record_at_tip(&tree.path, issue)? {
            return unproven(&format!(
                "the tip of PR {number} has no ledger record, so there are no budgets to keep. Run `cargo xtask prepare {number}`"
            ));
        }
        Some(ledger::read(&tree.path, issue as usize)?)
    } else {
        None
    };
    drop_earlier_record(&tree.path, issue)?;
    if !rebase(&tree.path, &fetched.main)? {
        return unproven("the branch doesn't rebase onto `main`. Rebase it by hand, and push it");
    }
    let gated = head(&tree.path)?;
    let scoring = Scoring {
        number,
        issue,
        mode,
        thread,
        pr: &pr,
        gated: &gated,
        kept: kept.as_ref(),
    };
    let mut status = Status::begin(&gh, &fetched.branch);
    let starting = |check: &str| status.proving(check);
    let mut proof = match prove(&tree.path, &scoring, &starting)? {
        Ok(proof) => proof,
        Err(failed) => {
            status.failed(&format!("Failed: {}", failed.join(", ")));
            return Ok(Verdict::Fail);
        }
    };
    // The xtask of the rebased tree is what measures `main` after the merge.
    // An exact revert puts back an xtask that was already measured, and that
    // may not know how to `prove`.
    if changes_xtask(&tree.path, &fetched.main)?
        && !(mode != Mode::Merge && base::is_exact_revert(&tree.path, &fetched.main)?)
    {
        status.proving("its own xtask");
        match prove_with_its_own(&tree.path, &scoring)? {
            Ok(own) => proof = own.after(proof),
            Err(failed) => {
                status.failed(&format!("Failed in its own xtask: {}", failed.join(", ")));
                return Ok(Verdict::Fail);
            }
        }
    }
    let Proof { record, accepted } = proof;

    if let Some(why) = changed_during_proof(&gh, &root, &scoring, &fetched.main)? {
        status.failed(&why);
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
    status.head = recorded.clone();
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
    status.passed(&format!(
        "spec {} of {}, journey {} of {}",
        record.proven, record.promises, record.passing, record.steps
    ))?;
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

/// The checks are those of the xtask that runs them, and the checkout this one
/// was built from can be anything. So this builds the xtask of `main` and runs
/// the merge again in it, which takes the slot again. If `main` moves before
/// that one fetches, it builds again.
fn run_the_checks_of(root: &Path, main: &str) -> Result<Verdict, Problem> {
    let directory = bench::temp_outside(root)?.join(format!("xtask-checks-{}", repository(root)?));
    let tree = Tree::at(root, directory.join("tree"), main)?;
    let program = build_xtask(&tree.path, &directory.join("target"))?;
    drop(tree);
    let problem = process::Command::new(program)
        .args(env::args_os().skip(1))
        .current_dir(root)
        // Rustup sets this for its children, and it would outrank the
        // toolchain file of the tree that the checks build.
        .env_remove("RUSTUP_TOOLCHAIN")
        .env(WORKER_OF, process::id().to_string())
        .env(CHECKS_OF, main)
        .exec();
    Err(Problem(format!(
        "couldn't run the xtask of `main`: {problem}"
    )))
}

/// Builds the xtask of a tree, and says where it is. A build shares the
/// activity lock, so that it doesn't slow a bench.
fn build_xtask(tree: &Path, target: &Path) -> Result<PathBuf, Problem> {
    let _hold = lock::shared(tree)?;
    let target_dir = target.to_string_lossy();
    let output = process::Command::new("cargo")
        .args([
            "build",
            "--quiet",
            "--package",
            "xtask",
            "--target-dir",
            &target_dir,
        ])
        .current_dir(tree)
        .env_remove("RUSTUP_TOOLCHAIN")
        .output()
        .map_err(workspace::no_cargo)?;
    if !output.status.success() {
        return Err(Problem(format!(
            "couldn't build the xtask of {}: {}",
            tree.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(target.join("debug/xtask"))
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

/// The page of the GitHub Actions run that this is part of, if it is in one.
fn run_url() -> Option<String> {
    let set = |name: &str| env::var(name).ok().filter(|value| !value.is_empty());
    Some(format!(
        "{}/{}/actions/runs/{}",
        set("GITHUB_SERVER_URL")?,
        set("GITHUB_REPOSITORY")?,
        set("GITHUB_RUN_ID")?
    ))
}

/// Posts `xtask/gate`, saying what is happening or why a proof failed. One
/// that can't be posted leaves the verdict as it is.
pub fn post_gate(gh: &Gh, commit: &str, state: &str, said: &str) {
    let posted = gh.post_status(commit, GATE, state, Some(said), run_url().as_deref());
    if let Err(Problem(message)) = posted {
        eprintln!("merge: couldn't post `{GATE}` on {commit}: {message}");
    }
}

/// The `xtask/gate` of one proof: what the proof is doing, and then how it
/// ended. A proof that ends without saying how was stopped by an error.
struct Status<'a> {
    gh: &'a Gh,
    /// The head that the PR shows, which the push replaces.
    head: String,
    ended: Cell<bool>,
}

impl<'a> Status<'a> {
    /// The lint is the first check that takes long.
    fn begin(gh: &'a Gh, head: &str) -> Status<'a> {
        let status = Status {
            gh,
            head: head.to_string(),
            ended: Cell::new(false),
        };
        status.proving("lint");
        status
    }

    fn proving(&self, phase: &str) {
        post_gate(self.gh, &self.head, "pending", &format!("Proving: {phase}"));
    }

    fn failed(&self, why: &str) {
        self.ended.set(true);
        post_gate(self.gh, &self.head, "failure", why);
    }

    /// A green that can't be posted is a problem, because the PR can't merge.
    fn passed(&self, score: &str) -> Result<(), Problem> {
        self.gh.post_status(
            &self.head,
            GATE,
            "success",
            Some(score),
            run_url().as_deref(),
        )?;
        self.ended.set(true);
        Ok(())
    }
}

impl Drop for Status<'_> {
    fn drop(&mut self) {
        if !self.ended.get() {
            self.failed("Stopped by an error");
        }
    }
}

fn acceptance(head: &str, accepted: &[String]) -> String {
    let lines: Vec<String> = accepted.iter().map(|item| format!("- {item}")).collect();
    format!(
        "Merging this PR at `{head}` accepts:\n\n{}\n",
        lines.join("\n")
    )
}

/// Whether this process is the worker that a supervisor started. The variable
/// names the worker's parent, or the worker itself once it runs the xtask of
/// `main`, so it means nothing to a merge that the worker's own tests start.
fn is_worker() -> bool {
    let ids = [std::os::unix::process::parent_id(), process::id()];
    env::var(WORKER_OF).is_ok_and(|named| ids.iter().any(|id| named == id.to_string()))
}

/// Tells the supervisor that the worker holds the slot, so that the timeout
/// starts. The xtask of `main` says so again, and gets a new timeout.
fn report_slot() -> Result<(), Problem> {
    let couldnt = |e: io::Error| Problem(format!("couldn't tell the supervisor: {e}"));
    let fd = io::stdin().as_fd().try_clone_to_owned().map_err(couldnt)?;
    fs::File::from(fd).write_all(b"s").map_err(couldnt)
}

/// Runs this merge again as a worker in a process group of its own, which
/// everything the merge starts joins, and stops the group when the worker
/// ends, however it ends. So nothing the merge started outlives it, even a
/// command that ignores its parent's exit. Only `gh pr merge` leaves the
/// group, so that it keeps the slot until it finishes.
fn supervise() -> ! {
    match supervised() {
        Ok(code) => process::exit(code),
        Err(Problem(message)) => {
            eprintln!("error: {message}");
            process::exit(2);
        }
    }
}

fn supervised() -> Result<i32, Problem> {
    let couldnt = |what: &str, e: io::Error| Problem(format!("couldn't {what}: {e}"));
    let (reports, report) = io::pipe().map_err(|e| couldnt("make a pipe", e))?;
    let program = env::current_exe().map_err(|e| couldnt("find this program", e))?;
    let mut worker = process::Command::new(program)
        .args(env::args_os().skip(1))
        .env(WORKER_OF, process::id().to_string())
        .stdin(report)
        .process_group(0)
        .spawn()
        .map_err(|e| couldnt("start the merge", e))?;
    let group = worker.id();
    // Until the worker is reaped, its group exists, even if it has exited.
    let (watched, _alive) = io::pipe().map_err(|e| couldnt("make a pipe", e))?;
    let watcher = process::Command::new("sh")
        .args(["-c", WATCHER, "sh", &group.to_string()])
        .stdin(watched)
        .stdout(process::Stdio::null())
        .stderr(process::Stdio::null())
        .process_group(0)
        .spawn();
    if let Err(error) = watcher {
        stop_group(group);
        let _ = worker.wait();
        return Err(couldnt("start the merge's watcher", error));
    }
    let seconds = env::var(TIMEOUT_OVERRIDE)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(TIMEOUT_SECONDS);
    let timed_out = time_out(reports, group, seconds);
    let status = worker.wait().map_err(|e| couldnt("wait for the merge", e));
    stop_group(group);
    let status = status?;
    if let Some(code) = status.code() {
        return Ok(code);
    }
    if timed_out.load(Ordering::SeqCst) {
        eprintln!("merge: stopped at the {seconds} second timeout. Run it again to restart");
        return Ok(1);
    }
    // Dies as the worker did, so that a killed merge looks killed.
    let signal = status.signal().unwrap_or(9);
    let _ = process::Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(process::id().to_string())
        .status();
    Ok(128 + signal)
}

/// Stops the worker's group once the worker has held the slot for the timeout,
/// because a stuck merge would hold it for every other merge. Says whether it
/// did. Only the caller exits, once the worker is reaped, because this thread
/// can be slow to go on after the kill lands, and the caller would then see a
/// killed worker and die as it did.
fn time_out(reports: io::PipeReader, group: u32, seconds: u64) -> Arc<AtomicBool> {
    let timed_out = Arc::new(AtomicBool::new(false));
    let said = Arc::clone(&timed_out);
    let (slot_taken, taken) = mpsc::channel();
    thread::spawn(move || {
        let mut reports = reports;
        let mut report = [0; 1];
        while reports.read(&mut report).is_ok_and(|read| read > 0) {
            if slot_taken.send(()).is_err() {
                break;
            }
        }
    });
    thread::spawn(move || {
        if taken.recv().is_err() {
            return;
        }
        loop {
            match taken.recv_timeout(Duration::from_secs(seconds)) {
                Ok(()) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => break,
            }
        }
        said.store(true, Ordering::SeqCst);
        stop_group(group);
    });
    timed_out
}

/// Stops the group first, so that none of it runs while the kill goes through
/// it. The worker holds the slot, and frees it only as it dies. `xtask` has no
/// dependencies, so `kill` sends the signals. An empty group is already
/// stopped, so `kill` failing on one is no problem.
fn stop_group(group: u32) {
    let target = format!("-{group}");
    for signal in ["-STOP", "-KILL"] {
        let sent = process::Command::new("kill")
            .args([signal, "--", &target])
            .stdout(process::Stdio::null())
            .stderr(process::Stdio::null())
            .status();
        if let Err(error) = sent {
            eprintln!("merge: couldn't stop process group {group}: {error}");
        }
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
    match decide(gh, number, scoring.mode, scoring.thread)? {
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
        let name = format!("xtask-merge-{}-{number}", repository(root)?);
        Ok(bench::temp_outside(root)?.join(name))
    }

    pub fn create(root: &Path, number: u64, commit: &str) -> Result<Tree, Problem> {
        Tree::at(root, Tree::path(root, number)?, commit)
    }

    /// Starts from nothing, because an earlier attempt may have died at any
    /// point in this worktree.
    fn at(root: &Path, path: PathBuf, commit: &str) -> Result<Tree, Problem> {
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

/// Names the repository, whichever of its worktrees this is.
fn repository(root: &Path) -> Result<String, Problem> {
    let mut hasher = DefaultHasher::new();
    lock::directory(root)?.hash(&mut hasher);
    Ok(format!("{:016x}", hasher.finish()))
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
    mode: Mode,
    thread: Option<&'a str>,
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

/// Runs the gate, the journey, and any bench on the rebased head, saying which
/// it is on, and returns the record they earn. If any check failed, the merge
/// must stop, and the reasons have been printed.
fn prove(
    tree: &Path,
    scoring: &Scoring,
    starting: &dyn Fn(&str),
) -> Result<Result<Proof, Failed>, Problem> {
    let Scoring { mode, pr, .. } = *scoring;
    let prepare = mode != Mode::Merge;
    let hold = lock::shared(tree)?;
    let merging = Merging {
        labels: &pr.labels,
        prepare,
        starting,
    };
    let (failed, score) = gate::check(tree, &hold, Some(&merging))?;
    if !failed.is_empty() {
        return Ok(Err(failed));
    }
    starting("journey");
    let (passing, steps) = journey::count(tree)?;
    drop(hold);

    let commit = base::find(tree)?
        .ok_or_else(|| Problem("there is no origin/main after the fetch".to_string()))?;
    // The base of a revert may be what doesn't build, and a revert puts back a
    // tree that was already measured.
    let revert = prepare && base::is_exact_revert(tree, &commit)?;
    if !revert && scoring.kept.is_none() && budget_can_change(tree, &commit)? {
        starting("bench");
        if bench::compare(tree, Some(&pr.body))? == Verdict::Fail {
            return Ok(Err(vec!["bench"]));
        }
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
    let mut accepted = match compare_counts(tree, &commit, &record, scoring)? {
        Ok(accepted) => accepted,
        Err(failed) => return Ok(Err(failed)),
    };
    if revert {
        accepted
            .push("an exact revert, so it can remove checks, and the bench didn't run".to_string());
    }
    Ok(Ok(Proof { record, accepted }))
}

/// The spec count is compared by the gate. This compares the two that need the
/// journey and the bench, and then asks whether anything rose. Returns the
/// counts that fell and are allowed to, or the checks that the PR fails here.
fn compare_counts(
    tree: &Path,
    commit: &str,
    record: &Record,
    scoring: &Scoring,
) -> Result<Result<Vec<String>, Failed>, Problem> {
    let budgets = bench::read_budgets(tree)?;
    let before = ledger::newest(tree, commit)?;
    let (proven, passing, budgets_met) = before.map_or((0, 0, 0), |base| {
        let met = bench::met(&budgets, &base.budgets).0;
        (base.proven, base.passing, met)
    });
    let met_now = bench::met(&budgets, &record.budgets).0;

    let mut failed = Failed::new();
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
        gate::record(
            name,
            after >= before || scoring.mode != Mode::Merge,
            &mut failed,
        );
    }
    let rose = record.proven > proven || record.passing > passing || met_now > budgets_met;
    if !rose && !declares_no_hill(&scoring.pr.body) {
        eprintln!(
            "hills: no count rose, so the PR body needs a `Hill: none, <reason>` line, or a change that raises one"
        );
        gate::record("hill-line", false, &mut failed);
    }
    Ok(if failed.is_empty() {
        Ok(accepted)
    } else {
        Err(failed)
    })
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

/// Whether the PR changes the xtask that proves it.
fn changes_xtask(tree: &Path, main: &str) -> Result<bool, Problem> {
    let args = ["diff", "--name-only", main, "HEAD", "--", "xtask/"];
    Ok(!base::git_text(tree, &args)?.trim().is_empty())
}

impl Proof {
    /// This proof, by the PR's own xtask, after `main`'s. Its record is the
    /// one kept, and it names what `main`'s xtask accepted that it didn't.
    fn after(mut self, mains: Proof) -> Proof {
        for item in mains.accepted {
            if !self.accepted.contains(&item) {
                self.accepted.push(format!("by `main`'s xtask, {item}"));
            }
        }
        self
    }
}

/// Builds the xtask of the rebased tree, and proves the PR with it. Its
/// `prove` writes the record into the tree, and its result into a file.
fn prove_with_its_own(
    tree: &Path,
    scoring: &Scoring,
) -> Result<Result<Proof, Vec<String>>, Problem> {
    let program = build_xtask(tree, &tree.join("target"))?;
    let own = tree.join(OWN);
    let couldnt = |e: io::Error| Problem(format!("couldn't prepare {}: {e}", own.display()));
    if own.exists() {
        fs::remove_dir_all(&own).map_err(couldnt)?;
    }
    fs::create_dir_all(&own).map_err(couldnt)?;
    let mut command = process::Command::new(program);
    command
        .args(["prove", &scoring.number.to_string(), scoring.mode.command()])
        .current_dir(tree);
    if let Some(kept) = scoring.kept {
        let file = own.join("kept.toml");
        ledger::write_file(&file, kept)?;
        command.arg("--keep").arg(file);
    }
    let status = command
        .status()
        .map_err(|e| Problem(format!("couldn't run its own xtask: {e}")))?;
    if !matches!(status.code(), Some(0 | 1)) {
        return Err(Problem(format!("its own xtask stopped: {status}")));
    }
    let result = own.join("result");
    let text = fs::read_to_string(&result)
        .map_err(|e| Problem(format!("couldn't read {}: {e}", result.display())))?;
    let mut failed = Vec::new();
    let mut accepted = Vec::new();
    for line in text.lines() {
        if let Some(item) = line.strip_prefix("accepted ") {
            accepted.push(item.to_string());
        } else if let Some(check) = line.strip_prefix("failed ") {
            failed.push(check.to_string());
        }
    }
    if !status.success() {
        return Ok(Err(failed));
    }
    let record = ledger::read(tree, scoring.issue as usize)?;
    Ok(Ok(Proof { record, accepted }))
}

/// `prove`, run in a PR's rebased tree by the xtask of `main`: proves the PR
/// with this xtask, as the merge sequence would, and writes the record into
/// the tree. Its result goes to `target/xtask-own/result`, a line per check
/// that failed or thing that merging accepts.
pub fn prove_here(pr: &str, mode: &str, keep: Option<&str>) -> Result<Verdict, Problem> {
    let number: u64 = pr
        .parse()
        .map_err(|_| Problem(format!("`prove` takes a PR number, but got `{pr}`")))?;
    let mode = match mode {
        "merge" => Mode::Merge,
        "prepare" => Mode::Prepare,
        "land" => Mode::Land,
        other => {
            return Err(Problem(format!(
                "`prove` takes `merge`, `prepare`, or `land`, but got `{other}`"
            )));
        }
    };
    let tree = workspace::root()?;
    let pr = Gh::new(&tree).pull_request(number)?;
    let [issue] = pr.closing_issues[..] else {
        return Err(Problem(format!(
            "PR {number} needs to close exactly one issue"
        )));
    };
    let kept = keep
        .map(|file| ledger::read_file(Path::new(file)))
        .transpose()?;
    let gated = head(&tree)?;
    let scoring = Scoring {
        number,
        issue,
        mode,
        thread: None,
        pr: &pr,
        gated: &gated,
        kept: kept.as_ref(),
    };
    let (lines, verdict) = match prove(&tree, &scoring, &|_| {})? {
        Ok(Proof { record, accepted }) => {
            ledger::store(&tree, &record)?;
            let lines: Vec<String> = accepted.iter().map(|a| format!("accepted {a}\n")).collect();
            (lines, Verdict::Pass)
        }
        Err(failed) => {
            let lines: Vec<String> = failed.iter().map(|f| format!("failed {f}\n")).collect();
            (lines, Verdict::Fail)
        }
    };
    let result = tree.join(OWN).join("result");
    fs::create_dir_all(tree.join(OWN))
        .and_then(|()| fs::write(&result, lines.concat()))
        .map_err(|e| Problem(format!("couldn't write {}: {e}", result.display())))?;
    Ok(verdict)
}
