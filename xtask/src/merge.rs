//! `cargo xtask merge`: the merge sequence in HARNESS.md.
//!
//! Each step can die and be run again. Everything before the push happens in a
//! throwaway worktree, a rerun starts from a new fetch, and a record that an
//! earlier attempt pushed is dropped and written again.

use crate::accepts::{
    self, Decision, Mode, Proof, Scoring, acceptance, decide, fails_early, hold_back, prove,
};
use crate::forge::{Gh, PullRequest};
use crate::lock::{self, Hold};
use crate::{Problem, Verdict, base, bench, ledger, waits, workspace};
use std::env;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread;
use std::time::{Duration, Instant};

/// How a proof ends that passed, and then an error stopped, as GitHub can.
pub const PROVEN_THEN_STOPPED: &str = "Proven, then stopped by an error";
/// How a merge ends that its supervisor stopped at the timeout.
pub const STOPPED_AT_THE_TIMEOUT: &str = "Stopped at the timeout";
/// Why a proof failed when the change may be fine.
pub const MAIN_MOVED: &str =
    "`main` moved during the merge, so this head wasn't tested on it. Run it again";
/// The one status that `main` requires.
pub const GATE: &str = "xtask/gate";
pub const MAIN: &str = "refs/remotes/origin/main";
const TIMEOUT_SECONDS: u64 = 30 * 60;
/// Replaces the timeout, so that a test doesn't wait for it.
const TIMEOUT_OVERRIDE: &str = "XTASK_MERGE_TIMEOUT_SECONDS";
/// What the worker tells its supervisor: it holds the slot, which starts the
/// timeout again, it waits for other work to free the slot, a place, or the
/// activity lock, which stops it, or its bench's wait is over, which lets the
/// timeout go on from where it stopped.
pub const SLOT: u8 = b's';
pub const WAITING: u8 = b'w';
pub const GOING: u8 = b'g';
/// Set in the worker that a merge runs in, to the id of the worker's parent,
/// or of the xtask of `main` that the worker runs, which names itself.
const WORKER_OF: &str = "XTASK_MERGE_WORKER_OF";
/// Stops the worker's group when the supervisor dies, as on Ctrl-C, which
/// reaches only the supervisor's process group. Only the supervisor holds the
/// other end of its input, so the input ends when the supervisor does. It runs
/// the same `kill` as `stop_group`, because dash's builtin refuses `--`.
const WATCHER: &str = "read _; env kill -STOP -- \"-$1\"; env kill -KILL -- \"-$1\"";
/// Set in a merge that runs the xtask of `main`, to that `main`.
const CHECKS_OF: &str = "XTASK_MERGE_CHECKS_OF";
/// Set in the xtask of `main` that a merge runs, to the head on which the
/// merge posted a `pending` that nothing has ended yet.
const POSTED: &str = "XTASK_MERGE_POSTED";
/// Runs the xtask of `main` in the shell's own process, which it names in
/// `WORKER_OF`, as it would be named if this merge ran it with `exec`.
const AS_WORKER: &str = "export XTASK_MERGE_WORKER_OF=$$; exec \"$0\" \"$@\"";
/// Set in a PR's own xtask, to the id of the xtask of `main` that runs its
/// `prove` and reads its input.
const PROVING_FOR: &str = "XTASK_MERGE_PROVING_FOR";
/// Where a PR's own xtask, run in its rebased tree, finds the record whose
/// budgets it keeps, and leaves its result.
const OWN: &str = "target/xtask-own";

fn refuse(why: &str) -> Result<Verdict, Problem> {
    eprintln!("merge: {why}");
    Ok(Verdict::Fail)
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
        supervise(pr);
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
    let status = Status::watched(&root, number);
    if mode != Mode::Land {
        match decide(&gh, number, mode, thread)? {
            // In `main`'s xtask, the PR can have changed since the merge
            // posted a wait.
            Decision::Refused(why) => {
                status.stops(&why);
                return refuse(&why);
            }
            // `main`'s xtask, which a merge runs next, checks under the slot.
            Decision::Ready { pr, issue } if !runs_the_checks_of_main() => {
                if fails_before_the_slot(&root, &gh, number, &pr, issue, mode)? {
                    return Ok(Verdict::Fail);
                }
            }
            Decision::Ready { .. } => {}
            Decision::Merged => {}
        }
    }
    // The xtask of `main` takes the slot again, ahead of every other merge,
    // because the merge that runs it lets it go, and the timeout has started
    // by then. So does a merge that a refresh runs in its turn.
    tell_supervisor(WAITING)?;
    let ahead = runs_the_checks_of_main() || lock::has_turn(&root)?;
    let slot = lock::merge_slot(&root, ahead, || {
        status.waits("the merge slot");
        eprintln!("{command}: PR {number} waits for the merge slot, which another run holds");
    })?;
    status.goes_on();
    report_slot()?;
    Tree::sweep(&root)?;
    let (pr, issue) = match decide(&gh, number, mode, thread)? {
        Decision::Merged => {
            status.close();
            Tree::clear(&root, &Tree::path(&root, number)?);
            println!("merge: PR {number} is already merged");
            return Ok(Verdict::Pass);
        }
        // The PR changed while it waited for the slot.
        Decision::Refused(why) => {
            status.stops(&why);
            return refuse(&why);
        }
        Decision::Ready { pr, issue } => (pr, issue),
    };

    let fetched = fetch(&root, &pr.head_ref)?;
    status.on(&fetched.branch);
    // After the slot and the fetch, because a merge during the wait can change
    // the checks.
    if env::var(CHECKS_OF).ok().as_deref() != Some(fetched.main.as_str()) {
        return run_the_checks_of(&root, &fetched.main, &status, slot);
    }
    let tree = Tree::create(&root, number, &fetched.branch)?;
    // Nothing was pushed, so this is the head that the PR shows.
    let unproven = |why: &str| {
        status.failed(why);
        refuse(why)
    };
    let kept = if keep_budgets {
        if !record_at(&tree.path, "HEAD", issue)? {
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
        slot: Some(&slot),
    };
    // The lint is the first check that takes long.
    status.proving("lint");
    let starting = |check: &str| status.proving(check);
    let mut proof = match prove(&tree.path, &scoring, &starting, &tell_supervisor)? {
        Ok(proof) => proof,
        Err(failed) => {
            status.failed(&format!("Failed: {}", failed.join(", ")));
            return Ok(Verdict::Fail);
        }
    };
    let place = proof.place.take();
    if accepts::proves_with_its_own(&tree.path, &fetched.main, mode)? {
        status.proving("its own xtask");
        match prove_with_its_own(&tree.path, &scoring, place, &status)? {
            Ok(own) => proof = own.after(proof),
            Err(failed) => {
                status.failed(&format!("Failed in its own xtask: {}", failed.join(", ")));
                return Ok(Verdict::Fail);
            }
        }
    } else {
        drop(place);
    }
    let Proof {
        record, accepted, ..
    } = proof;
    status.proven();

    if let Some(why) = again(|| changed_during_proof(&gh, &root, &scoring, &fetched.main))? {
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
    // A push that landed but failed all the same is up to date the second
    // time.
    again(|| base::git_text(&tree.path, &["push", "-q", &lease, "origin", &target]))?;
    status.on(&recorded);
    let changed = base::git_text(&tree.path, &["diff", "--name-only", &gated, &recorded])?;
    if changed.trim() != path {
        return Err(Problem(format!(
            "{recorded} should differ from the gated head {gated} only by {path}, but it changes:\n{}",
            changed.trim_end()
        )));
    }
    // The comment comes first, so that a green PR always has it.
    if !accepted.is_empty() {
        let earlier = again(|| gh.acceptances(number))?;
        let body = acceptance(&recorded, &accepted);
        again(|| {
            // A call that failed can have posted it all the same.
            let posted = accepts::acceptances(&gh.acceptances(number)?);
            if posted.iter().any(|(at, _)| *at == recorded) {
                return Ok(());
            }
            gh.comment_on_pr(number, &body)
        })?;
        // It only reads, and turns auto-merge off if it is on, so it can run
        // twice.
        again(|| hold_back(&gh, number, command, &earlier, &accepted))?;
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
/// that one fetches, it builds again. This keeps the merge's turn until that
/// xtask ends, so no other merge takes the slot in between, and ends as that
/// xtask does. The xtask of `main` has its turn kept by the merge that ran it,
/// and a merge that a refresh runs has it kept by the refresh.
fn run_the_checks_of(
    root: &Path,
    main: &str,
    status: &Status,
    slot: Hold,
) -> Result<Verdict, Problem> {
    let directory = bench::temp_outside(root)?.join(format!("xtask-checks-{}", repository(root)?));
    let tree = Tree::at(root, directory.join("tree"), main)?;
    let built = build_xtask(
        &tree.path,
        &directory.join("target"),
        &lock::shared(&tree.path)?,
    )?;
    drop(tree);
    let program = keep(&directory.join("bin"), &built, main)?;
    let turn = if runs_the_checks_of_main() || lock::has_turn(root)? {
        None
    } else {
        Some(lock::merge_turn(root)?)
    };
    drop(slot);
    let mut command = process::Command::new("sh");
    // That xtask ends what this one posted, and this one ends with no drop.
    // What an outer merge posted, as for a merge that the gate's tests run,
    // isn't this one's.
    match status.posted() {
        Some(head) => command.env(POSTED, head),
        None => command.env_remove(POSTED),
    };
    let mut checks = command
        .args(["-c", AS_WORKER])
        .arg(program)
        .args(env::args_os().skip(1))
        .current_dir(root)
        // Rustup sets this for its children, and it would outrank the
        // toolchain file of the tree that the checks build.
        .env_remove("RUSTUP_TOOLCHAIN")
        .env(CHECKS_OF, main)
        .spawn()
        .map_err(|e| Problem(format!("couldn't run the xtask of `main`: {e}")))?;
    let ended = checks
        .wait()
        .map_err(|e| Problem(format!("couldn't wait for the xtask of `main`: {e}")))?;
    drop(turn);
    // The shell's own codes, when it couldn't run that xtask, which then
    // ended nothing this one posted. No xtask exits with either.
    if let Some(code @ (126 | 127)) = ended.code() {
        return Err(Problem(format!(
            "couldn't run the xtask of `main`: `sh` exited {code}"
        )));
    }
    end_as(ended)
}

/// A copy of the xtask built from `main`, named by that commit, which no later
/// build replaces. The next merge builds in the same target while this one
/// runs, and the checks run this program again by its path. The caller holds
/// the merge slot, so nothing else writes here, and it keeps its turn until
/// the copy ends.
fn keep(bin: &Path, built: &Path, main: &str) -> Result<PathBuf, Problem> {
    let problem = |e: io::Error| Problem(format!("couldn't keep {}: {e}", built.display()));
    fs::create_dir_all(bin).map_err(problem)?;
    sweep_kept(bin);
    let kept = bin.join(main);
    if !kept.exists() {
        let partial = bin.join(format!("{main}.{}", process::id()));
        fs::copy(built, &partial).map_err(problem)?;
        fs::rename(&partial, &kept).map_err(problem)?;
    }
    Ok(kept)
}

/// Removes the copies that no process runs, which a run leaves when it ends.
/// It sees only the processes of its own PID namespace. Where the processes
/// can't be listed, as on macOS, it keeps them all.
fn sweep_kept(bin: &Path) {
    let Ok(processes) = fs::read_dir("/proc") else {
        return;
    };
    let running: Vec<(u64, u64)> = processes
        .filter_map(|entry| fs::metadata(entry.ok()?.path().join("exe")).ok())
        .map(|m| (m.dev(), m.ino()))
        .collect();
    let Ok(entries) = fs::read_dir(bin) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(copy) = entry.metadata() else {
            continue;
        };
        if !running.contains(&(copy.dev(), copy.ino())) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Builds the xtask of a tree, and says where it is. The build runs under
/// `_shared`, a shared hold of the activity lock, so that it doesn't slow a
/// bench.
fn build_xtask(tree: &Path, target: &Path, _shared: &Hold) -> Result<PathBuf, Problem> {
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

/// The `xtask/gate` of one merge: what it waits for, what its proof is doing,
/// and then how it ended. A merge that posted a `pending`, and ends without
/// saying how, was stopped by an error. A wait for a lock reaches it from
/// `lock`, which tells it as the wait starts and ends.
struct Status {
    gh: Gh,
    number: u64,
    said: Mutex<Said>,
}

#[derive(Default)]
struct Said {
    /// The head that the PR shows, which the push replaces. Until the fetch,
    /// it is read from GitHub when a post first needs it.
    head: Option<String>,
    /// What the proof is doing, which it says again once a wait ends.
    phase: Option<String>,
    waiting: bool,
    /// Whether a `pending` went up that nothing has ended yet.
    open: bool,
    /// Whether the proof passed, so that what stops it after is no fault of
    /// the change.
    proven: bool,
}

impl Status {
    fn new(root: &Path, number: u64) -> Status {
        Status {
            gh: Gh::new(root),
            number,
            said: Mutex::default(),
        }
    }

    /// A status that hears this process's waits for a place and the activity
    /// lock. In the xtask of `main`, it goes on from what the merge posted.
    fn watched(root: &Path, number: u64) -> Arc<Status> {
        let status = Arc::new(Status::new(root, number));
        if runs_the_checks_of_main()
            && let Ok(head) = env::var(POSTED)
        {
            let mut said = status.said();
            said.head = Some(head);
            said.open = true;
        }
        let watched = Arc::downgrade(&status);
        lock::watch(Box::new(move |wait| {
            let Some(status) = watched.upgrade() else {
                return;
            };
            match wait {
                Some(lock::Wait::Place) => status.waits("a place"),
                Some(lock::Wait::Activity) => status.waits("the activity lock"),
                None => status.goes_on(),
            }
        }));
        status
    }

    fn said(&self) -> std::sync::MutexGuard<'_, Said> {
        self.said.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The head of a `pending` that nothing has ended yet.
    fn posted(&self) -> Option<String> {
        let said = self.said();
        said.head.clone().filter(|_| said.open)
    }

    /// The head that the PR shows from now on.
    fn on(&self, head: &str) {
        self.said().head = Some(head.to_string());
    }

    /// Says whether there was a head to post on.
    fn post(&self, said: &mut Said, state: &str, text: &str) -> bool {
        if said.head.is_none() {
            match self.gh.armed(self.number) {
                // A PR that merged or closed keeps what it ended with.
                Ok(now) if now.state != "OPEN" => return false,
                Ok(now) => said.head = Some(now.head),
                Err(Problem(message)) => {
                    eprintln!("merge: couldn't post `{GATE}`: {message}");
                    return false;
                }
            }
        }
        if let Some(head) = &said.head {
            post_gate(&self.gh, head, state, text);
        }
        true
    }

    fn waits(&self, what: &str) {
        let mut said = self.said();
        said.waiting = true;
        if self.post(&mut said, "pending", &format!("Waiting: {what}")) {
            said.open = true;
        }
    }

    /// Once a wait ends, the proof says again what it is doing, if it has
    /// begun.
    fn goes_on(&self) {
        let mut said = self.said();
        if !std::mem::take(&mut said.waiting) {
            return;
        }
        if let Some(phase) = said.phase.clone() {
            self.post(&mut said, "pending", &format!("Proving: {phase}"));
        }
    }

    fn proving(&self, phase: &str) {
        let mut said = self.said();
        said.phase = Some(phase.to_string());
        said.waiting = false;
        said.open = true;
        self.post(&mut said, "pending", &format!("Proving: {phase}"));
    }

    fn proven(&self) {
        self.said().proven = true;
    }

    fn failed(&self, why: &str) {
        let mut said = self.said();
        said.open = false;
        self.post(&mut said, "failure", why);
    }

    /// Ends a `pending` that a wait posted, saying why the merge stops.
    fn stops(&self, why: &str) {
        if self.said().open {
            self.failed(why);
        }
    }

    /// Leaves what is posted, as for a PR that merged meanwhile.
    fn close(&self) {
        self.said().open = false;
    }

    /// A green that can't be posted is a problem, because the PR can't merge.
    fn passed(&self, score: &str) -> Result<(), Problem> {
        let mut said = self.said();
        let head = said
            .head
            .clone()
            .ok_or_else(|| Problem("there is no head to post on".to_string()))?;
        let link = run_url();
        again(|| {
            self.gh
                .post_status(&head, GATE, "success", Some(score), link.as_deref())
        })?;
        said.open = false;
        Ok(())
    }
}

impl Drop for Status {
    fn drop(&mut self) {
        let (open, proven) = {
            let said = self.said();
            (said.open, said.proven)
        };
        if open {
            self.failed(if proven {
                PROVEN_THEN_STOPPED
            } else {
                "Stopped by an error"
            });
        }
    }
}

/// How long a call that failed after the proof waits for its second try.
const AGAIN: Duration = Duration::from_secs(2);

/// GitHub can fail a call that it takes a moment later. Once the proof has
/// passed, each call that can fail this way gets a second try, so the call
/// must be safe to make twice.
fn again<T>(mut call: impl FnMut() -> Result<T, Problem>) -> Result<T, Problem> {
    call().or_else(|Problem(why)| {
        eprintln!("merge: {why}, so it is tried again");
        thread::sleep(AGAIN);
        call()
    })
}

/// Whether this process is the worker that a supervisor started. The variable
/// names the worker's parent, or the xtask of `main` that the worker runs,
/// which names itself. So it means nothing to a merge that the worker's own
/// tests start.
fn is_worker() -> bool {
    let ids = [std::os::unix::process::parent_id(), process::id()];
    env::var(WORKER_OF).is_ok_and(|named| ids.iter().any(|id| named == id.to_string()))
}

/// Whether this process is the xtask of `main` that a merge runs, which names
/// itself in the variable, unlike a worker or a merge that its tests start.
fn runs_the_checks_of_main() -> bool {
    env::var(WORKER_OF).is_ok_and(|named| named == process::id().to_string())
}

/// Tells the supervisor that the worker holds the slot, so that the timeout
/// starts. The xtask of `main` says so again, and gets a new timeout, and
/// again before a PR's own xtask proves it.
fn report_slot() -> Result<(), Problem> {
    tell_supervisor(SLOT)
}

/// Tells the supervisor that the worker waits for a place, or has one. The
/// timeout stops during the wait, and starts again after it.
fn report_place(waiting: bool) -> Result<(), Problem> {
    tell_supervisor(if waiting { WAITING } else { SLOT })
}

fn tell_supervisor(report: u8) -> Result<(), Problem> {
    let couldnt = |e: io::Error| Problem(format!("couldn't tell the supervisor: {e}"));
    let fd = io::stdin().as_fd().try_clone_to_owned().map_err(couldnt)?;
    fs::File::from(fd).write_all(&[report]).map_err(couldnt)
}

/// Runs this merge again as a worker in a process group of its own, which
/// everything the merge starts joins, and stops the group when the worker
/// ends, however it ends. So nothing the merge started outlives it, even a
/// command that ignores its parent's exit. Only `gh pr merge` leaves the
/// group, so that it keeps the slot until it finishes.
fn supervise(pr: &str) -> ! {
    match supervised(pr) {
        Ok(code) => process::exit(code),
        Err(Problem(message)) => {
            eprintln!("error: {message}");
            process::exit(2);
        }
    }
}

fn supervised(pr: &str) -> Result<i32, Problem> {
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
    if status.code().is_none() && timed_out.load(Ordering::SeqCst) {
        eprintln!("merge: stopped at the {seconds} second timeout. Run it again to restart");
        // The worker is gone, so this is the head that the PR shows.
        match (pr.parse(), workspace::root()) {
            (Ok(number), Ok(root)) => Status::new(&root, number).failed(STOPPED_AT_THE_TIMEOUT),
            (_, Err(Problem(message))) => eprintln!("merge: couldn't post `{GATE}`: {message}"),
            (Err(_), _) => {}
        }
        return Ok(1);
    }
    end_as(status)
}

/// Exits as a child did, and dies as it did if it was killed, so that a
/// killed merge looks killed.
fn end_as(status: process::ExitStatus) -> ! {
    if let Some(code) = status.code() {
        process::exit(code);
    }
    let signal = status.signal().unwrap_or(9);
    let _ = process::Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(process::id().to_string())
        .status();
    process::exit(128 + signal)
}

/// Stops the worker's group once the worker has held the slot for the timeout,
/// because a stuck merge would hold it for every other merge. A wait for the
/// slot or a place doesn't count, because other work holds them, and neither
/// does a bench's wait for the activity lock, which other work holds. Says
/// whether it did. Only the caller exits, once the worker is reaped, because
/// this thread can be slow to go on after the kill lands, and the caller would
/// then see a killed worker and die as it did.
fn time_out(reports: io::PipeReader, group: u32, seconds: u64) -> Arc<AtomicBool> {
    let timed_out = Arc::new(AtomicBool::new(false));
    let said = Arc::clone(&timed_out);
    let (slot_taken, taken) = mpsc::channel();
    thread::spawn(move || {
        let mut reports = reports;
        let mut report = [0; 1];
        while reports.read(&mut report).is_ok_and(|read| read > 0) {
            if slot_taken.send(report[0]).is_err() {
                break;
            }
        }
    });
    thread::spawn(move || {
        let Ok(mut last) = taken.recv() else {
            return;
        };
        let whole = Duration::from_secs(seconds);
        let mut left = whole;
        loop {
            let since = Instant::now();
            let next = match last {
                WAITING => taken
                    .recv()
                    .map_err(|_| mpsc::RecvTimeoutError::Disconnected),
                _ => taken.recv_timeout(left),
            };
            match next {
                Ok(report) => {
                    left = left_after(last, report, left, since.elapsed(), whole);
                    last = report;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => break,
            }
        }
        said.store(true, Ordering::SeqCst);
        stop_group(group);
    });
    timed_out
}

/// What is left of the timeout once `report` follows `last`. The worker ran or
/// waited for `spent` since `last`, and had `left` then. Holding the slot gives
/// it the `whole` timeout again. Otherwise what it ran counts, and what it
/// waited doesn't, so a wait that ends goes on with what was left.
fn left_after(last: u8, report: u8, left: Duration, spent: Duration, whole: Duration) -> Duration {
    if report == SLOT {
        whole
    } else if last == WAITING {
        left
    } else {
        left.saturating_sub(spent)
    }
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

/// The proof takes minutes, and meanwhile the PR or a review can change, or
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
            if issue != scoring.issue || pr.head_ref != was.head_ref || pr.body != was.body {
                return Ok(Some(format!(
                    "PR {number} changed during the merge, and the checks read what it said before. Run it again"
                )));
            }
        }
    }
    if remote_commit(root, "refs/heads/main")?.as_deref() != Some(main) {
        return Ok(Some(MAIN_MOVED.to_string()));
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
    /// Says that a live run holds the tree. The kernel lets go when the run
    /// dies, so that the next merge can tell the tree was left behind.
    _held: fs::File,
}

impl Tree {
    /// Named by repository, so that a rerun from another of its worktrees
    /// finds and clears what an earlier attempt left.
    fn path(root: &Path, number: u64) -> Result<PathBuf, Problem> {
        let name = format!("xtask-merge-{}-{number}", repository(root)?);
        Ok(long(&bench::temp_outside(root)?, &name))
    }

    pub fn create(root: &Path, number: u64, commit: &str) -> Result<Tree, Problem> {
        Tree::at(root, Tree::path(root, number)?, commit)
    }

    /// For a proof of `main` alone, as long as a PR's, so that it fails where
    /// a PR's would.
    pub fn of_main(root: &Path, commit: &str) -> Result<Tree, Problem> {
        let name = format!("xtask-main-{}", repository(root)?);
        Tree::at(root, long(&bench::temp_outside(root)?, &name), commit)
    }

    /// Starts from nothing, because an earlier attempt may have died at any
    /// point in this worktree.
    fn at(root: &Path, path: PathBuf, commit: &str) -> Result<Tree, Problem> {
        let held = hold(&path, true)?.expect("a blocking lock is always taken");
        Tree::clear(root, &path);
        let name = path.to_string_lossy();
        base::git_text(root, &["worktree", "add", "-q", "--detach", &name, commit])?;
        Ok(Tree {
            root: root.to_path_buf(),
            path,
            _held: held,
        })
    }

    /// Removes this repository's merge trees that no live run holds, which
    /// a killed run leaves. Nothing else would remove them until the same PR
    /// is proven again. The system's temp directory is swept too, for trees
    /// left there before `temp_outside` chose another place.
    fn sweep(root: &Path) -> Result<(), Problem> {
        let prefix = format!("xtask-merge-{}-", repository(root)?);
        let mut places = vec![bench::temp_outside(root)?];
        places.extend(fs::canonicalize(env::temp_dir()).ok());
        places.dedup();
        let mut trees = Vec::new();
        for place in places {
            let entries = fs::read_dir(&place)
                .map_err(|e| Problem(format!("couldn't read {}: {e}", place.display())))?;
            trees.extend(entries.filter_map(|entry| {
                let name = entry.ok()?.file_name().into_string().ok()?;
                let name = name.strip_suffix(".lock").unwrap_or(&name);
                name.starts_with(&prefix).then(|| place.join(name))
            }));
        }
        trees.sort();
        trees.dedup();
        for tree in trees {
            if let Some(_held) = hold(&tree, false)? {
                Tree::clear(root, &tree);
                let _ = fs::remove_file(lock_of(&tree));
            }
        }
        Ok(())
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

/// How long a tree's path is, at least. A worker's worktree is longer than a
/// Unix socket path may be, so a check that needs a short path fails here
/// too. It stays under 256 bytes, where the kernel cuts a `#!` line.
const TREE_PATH: usize = 160;

/// `name` in `temp`, padded to `TREE_PATH` bytes.
fn long(temp: &Path, name: &str) -> PathBuf {
    let short = temp.join(name).as_os_str().len();
    let padding = "_".repeat(TREE_PATH.saturating_sub(short));
    temp.join(format!("{name}{padding}"))
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
        let _ = fs::remove_file(lock_of(&self.path));
    }
}

fn lock_of(tree: &Path) -> PathBuf {
    let mut name = tree.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

/// Locks the file beside a tree that says a run holds it, or says that one
/// does when `wait` is false. Whoever removes the tree unlinks the file while
/// it holds the lock, so a lock on a file that is gone is taken again.
fn hold(tree: &Path, wait: bool) -> Result<Option<fs::File>, Problem> {
    let path = lock_of(tree);
    let problem = |e: io::Error| Problem(format!("couldn't lock {}: {e}", path.display()));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(problem)?;
    }
    loop {
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(problem)?;
        if wait {
            file.lock().map_err(problem)?;
        } else {
            match file.try_lock() {
                Ok(()) => {}
                Err(fs::TryLockError::WouldBlock) => return Ok(None),
                Err(fs::TryLockError::Error(e)) => return Err(problem(e)),
            }
        }
        let held = file.metadata().map_err(problem)?;
        let current = fs::metadata(&path);
        if current.is_ok_and(|m| m.dev() == held.dev() && m.ino() == held.ino()) {
            return Ok(Some(file));
        }
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
    if record_at(tree, "HEAD", issue)? {
        base::git_text(tree, &["reset", "-q", "--hard", "HEAD~1"])?;
    }
    Ok(())
}

/// Whether `commit` does nothing but write this issue's record.
fn record_at(root: &Path, commit: &str, issue: u64) -> Result<bool, Problem> {
    let args = ["diff-tree", "--no-commit-id", "--name-only", "-r", commit];
    let touched = base::git_text(root, &args)?;
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

/// Whether the proof would fail the PR before its gate, so that the merge
/// needn't wait for the slot. It compares `main` with the tree that merging
/// the branch into it makes, which is what the rebase under the slot makes
/// unless they conflict. The xtask that starts a merge can be any, so
/// `main`'s checks again under the slot, and decides. A failure is posted on
/// the branch's head.
fn fails_before_the_slot(
    root: &Path,
    gh: &Gh,
    number: u64,
    pr: &PullRequest,
    issue: u64,
    mode: Mode,
) -> Result<bool, Problem> {
    let Some(fetched) = Early::fetch(root, number, &pr.head_ref)? else {
        return Ok(false);
    };
    let Early { main, branch, .. } = &fetched;
    let head = if record_at(root, branch, issue)? {
        format!("{branch}~1")
    } else {
        branch.clone()
    };
    // A conflict is for the rebase under the slot to report.
    let merged = base::git(root, &["merge-tree", "--write-tree", main, &head])?;
    if !merged.status.success() {
        return Ok(false);
    }
    let merged = String::from_utf8_lossy(&merged.stdout);
    let tree = merged.lines().next().unwrap_or_default();
    let waiting = if mode == Mode::Merge {
        waits::reasons(root, main, tree)?
    } else {
        Vec::new()
    };
    let body = &pr.body;
    let Some(failed) = fails_early(root, main, tree, &waiting, number, body, mode)? else {
        return Ok(false);
    };
    let said = format!("Failed: {}", failed.join(", "));
    post_gate(gh, branch, "failure", &said);
    Ok(true)
}

/// `main` and the branch, fetched before the slot into refs of this run's
/// own. So the fetch never holds a lock that the merge in the slot needs, and
/// it fetches no tags and starts no maintenance, which would take locks too.
/// It leaves the caller's `FETCH_HEAD` alone.
/// The refs go when this does.
struct Early<'a> {
    root: &'a Path,
    refs: [String; 2],
    main: String,
    branch: String,
}

impl<'a> Early<'a> {
    /// `None` when it can't fetch, which the fetch under the slot reports.
    fn fetch(root: &'a Path, number: u64, branch: &str) -> Result<Option<Early<'a>>, Problem> {
        let name = format!("refs/xtask/early-{number}-{}", process::id());
        let mut early = Early {
            root,
            refs: [format!("{name}/main"), format!("{name}/branch")],
            main: String::new(),
            branch: String::new(),
        };
        let main = format!("+refs/heads/main:{}", early.refs[0]);
        let branch = format!("+refs/heads/{branch}:{}", early.refs[1]);
        let args = [
            "fetch",
            "-q",
            "--no-tags",
            "--no-auto-maintenance",
            "--no-write-fetch-head",
            "origin",
            &main,
            &branch,
        ];
        if !base::git(root, &args)?.status.success() {
            return Ok(None);
        }
        let resolve = |name: &str| -> Result<String, Problem> {
            Ok(base::git_text(root, &["rev-parse", name])?
                .trim()
                .to_string())
        };
        early.main = resolve(&early.refs[0])?;
        early.branch = resolve(&early.refs[1])?;
        Ok(Some(early))
    }
}

impl Drop for Early<'_> {
    fn drop(&mut self) {
        for name in &self.refs {
            let _ = base::git(self.root, &["update-ref", "-d", name]);
        }
    }
}

/// Builds the xtask of the rebased tree, and proves the PR with it, in `place`
/// if `main`'s proof kept one. Its `prove` writes the record into the tree,
/// and its result into a file.
fn prove_with_its_own(
    tree: &Path,
    scoring: &Scoring,
    place: Option<Hold>,
    status: &Status,
) -> Result<Result<Proof, Vec<String>>, Problem> {
    // Each proof can take most of the timeout, so its own gets a new one once
    // it has a place. The PR's code can't renew the timeout, so the place it
    // tests in is taken here.
    // Its build waits for the activity lock too, which a bench can hold for
    // minutes.
    report_place(true)?;
    let place = match place {
        Some(place) => place,
        None => lock::place(tree, scoring.slot)?,
    };
    let building = place.share()?;
    report_place(false)?;
    let program = build_xtask(tree, &tree.join("target"), &building)?;
    let place = building.keep_place();
    let own = tree.join(OWN);
    let couldnt = |e: io::Error| Problem(format!("couldn't prepare {}: {e}", own.display()));
    if own.exists() {
        fs::remove_dir_all(&own).map_err(couldnt)?;
    }
    fs::create_dir_all(&own).map_err(couldnt)?;
    let (reports, report) =
        io::pipe().map_err(|e| Problem(format!("couldn't make a pipe: {e}")))?;
    let mut command = process::Command::new(program);
    // Its input reaches this xtask, not the supervisor, so that the PR's code
    // can't renew the timeout.
    command
        .args(["prove", &scoring.number.to_string(), scoring.mode.command()])
        .current_dir(tree)
        .env(PROVING_FOR, process::id().to_string())
        .stdin(report);
    place.pass_to(&mut command);
    if let Some(kept) = scoring.kept {
        let file = own.join("kept.toml");
        ledger::write_file(&file, kept)?;
        command.arg("--keep").arg(file);
    }
    let mut child = command
        .spawn()
        .map_err(|e| Problem(format!("couldn't run its own xtask: {e}")))?;
    drop(command);
    let ended = relay(&mut child, reports, tree, status)?;
    if !matches!(ended.code(), Some(0 | 1)) {
        return Err(Problem(format!("its own xtask stopped: {ended}")));
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
    if !ended.success() {
        return Ok(Err(failed));
    }
    let record = ledger::read(tree, scoring.issue as usize)?;
    Ok(Ok(Proof {
        record,
        accepted,
        place: None,
    }))
}

/// Tells the supervisor and `status` that the bench of the PR's own xtask
/// waits for the activity lock, for as long as the PR's xtask says so and
/// this one sees for itself that the bench waits for work that shares it. The
/// PR's xtask can say nothing else to the supervisor. Returns once it exits.
/// On an error, it stops the PR's xtask, and the timeout runs again.
fn relay(
    child: &mut process::Child,
    reports: io::PipeReader,
    tree: &Path,
    status: &Status,
) -> Result<process::ExitStatus, Problem> {
    let mut stopped = false;
    let ended = listen(child, reports, tree, status, &mut stopped);
    if stopped {
        let told = tell_supervisor(GOING);
        if ended.is_ok() {
            told?;
        }
    }
    if ended.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    ended
}

/// `relay` until the PR's xtask exits. `stopped` says whether the supervisor
/// last heard that the bench waits.
fn listen(
    child: &mut process::Child,
    reports: io::PipeReader,
    tree: &Path,
    status: &Status,
    stopped: &mut bool,
) -> Result<process::ExitStatus, Problem> {
    let directory = lock::directory(tree)?;
    let couldnt = |e: io::Error| Problem(format!("couldn't wait for its own xtask: {e}"));
    let (said, heard) = mpsc::channel();
    thread::spawn(move || {
        let mut reports = reports;
        let mut report = [0; 1];
        while reports.read(&mut report).is_ok_and(|read| read > 0) {
            if said.send(report[0]).is_err() {
                break;
            }
        }
    });
    let mut claimed = false;
    loop {
        let open = match heard.recv_timeout(Duration::from_secs(1)) {
            // Only the newest report counts.
            Ok(report) => {
                claimed = heard.try_iter().last().unwrap_or(report) == WAITING;
                true
            }
            Err(mpsc::RecvTimeoutError::Timeout) => true,
            Err(mpsc::RecvTimeoutError::Disconnected) => false,
        };
        let waits = open && claimed && lock::bench_waits(&directory)?;
        if waits != *stopped {
            tell_supervisor(if waits { WAITING } else { GOING })?;
            if waits {
                status.waits("the activity lock");
            } else {
                status.goes_on();
            }
            *stopped = waits;
        }
        if !open {
            return child.wait().map_err(couldnt);
        }
        if let Some(status) = child.try_wait().map_err(couldnt)? {
            return Ok(status);
        }
    }
}

/// Tells the xtask of `main` that runs this `prove` when its bench starts to
/// wait for the activity lock, and when it stops, through this one's input.
/// A `prove` that another process runs, such as an older xtask of `main`,
/// says nothing.
fn tell_main(report: u8) -> Result<(), Problem> {
    let parent = std::os::unix::process::parent_id().to_string();
    if env::var(PROVING_FOR).ok() != Some(parent) {
        return Ok(());
    }
    let couldnt = |e: io::Error| Problem(format!("couldn't tell the xtask of `main`: {e}"));
    let fd = io::stdin().as_fd().try_clone_to_owned().map_err(couldnt)?;
    fs::File::from(fd).write_all(&[report]).map_err(couldnt)
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
        slot: None,
    };
    let (lines, verdict) = match prove(&tree, &scoring, &|_| {}, &tell_main)? {
        Ok(Proof {
            record, accepted, ..
        }) => {
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

#[cfg(test)]
mod tests {
    use super::{GOING, SLOT, TREE_PATH, WAITING, keep, left_after, long};
    use std::fs;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    #[test]
    fn a_wait_costs_none_of_the_timeout_and_only_the_slot_starts_it_again() {
        let seconds = Duration::from_secs;
        let whole = seconds(60);
        // The proof ran for 20 seconds, and then its bench waits.
        assert_eq!(
            left_after(SLOT, WAITING, whole, seconds(20), whole),
            seconds(40)
        );
        // However long it waits, it goes on with what was left.
        assert_eq!(
            left_after(WAITING, GOING, seconds(40), seconds(3600), whole),
            seconds(40)
        );
        // What the bench then runs counts, and can use up the rest.
        assert_eq!(
            left_after(GOING, WAITING, seconds(40), seconds(15), whole),
            seconds(25)
        );
        assert_eq!(
            left_after(GOING, WAITING, seconds(40), seconds(50), whole),
            seconds(0)
        );
        // A place, or the slot once more, starts the whole timeout again.
        assert_eq!(
            left_after(WAITING, SLOT, seconds(40), seconds(5), whole),
            whole
        );
        assert_eq!(
            left_after(GOING, SLOT, seconds(40), seconds(5), whole),
            whole
        );
    }

    #[test]
    fn a_tree_path_is_padded_to_its_length() {
        let path = long(Path::new("/tmp"), "xtask-merge-0123456789abcdef-9");
        assert_eq!(path.as_os_str().len(), TREE_PATH);
        assert!(path.starts_with("/tmp"));
        let deep = format!("/{}", "d".repeat(200));
        let path = long(Path::new(&deep), "xtask-merge-0123456789abcdef-9");
        assert_eq!(
            path,
            Path::new(&deep).join("xtask-merge-0123456789abcdef-9")
        );
    }

    /// A copy stays while a process runs it, and goes once none does.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_kept_xtask_stays_while_it_runs() {
        let bin = std::env::temp_dir().join(format!("xtask-kept-{}", std::process::id()));
        let _ = fs::remove_dir_all(&bin);
        let running = keep(&bin, Path::new("/bin/sh"), "running").unwrap();
        let spawn = || {
            Command::new(&running)
                .args(["-c", "read line"])
                .stdin(Stdio::piped())
                .spawn()
        };
        // Another test that forks while the copy is open for writing makes
        // it busy until that child runs its program.
        let mut tries = 0;
        let mut child = loop {
            match spawn() {
                Err(e) if e.raw_os_error() == Some(26) && tries < 1000 => {
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(1));
                }
                spawned => break spawned.unwrap(),
            }
        };
        let idle = keep(&bin, Path::new("/bin/sh"), "idle").unwrap();
        let kept = keep(&bin, Path::new("/bin/sh"), "next").unwrap();
        drop(child.stdin.take());
        child.wait().unwrap();

        assert!(running.is_file());
        assert!(!idle.exists());
        assert!(kept.is_file());
        fs::remove_dir_all(&bin).unwrap();
    }
}
