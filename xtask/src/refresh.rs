//! `cargo xtask refresh`: proves each `land-needed` PR that the owner approved
//! by arming auto-merge on the newest `main`, and merges it.

use crate::forge::{Armed, Gh, OpenPr};
use crate::lock::Hold;
use crate::work::LAND_NEEDED;
use crate::{Problem, Verdict, accepts, base, group, lock, merge, workspace};
use std::collections::BTreeSet;
use std::env;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

pub fn run() -> Result<Verdict, Problem> {
    let root = workspace::root()?;
    if !root.join(".git").exists() {
        return Err(Problem(
            "`refresh` works on a repository, so run it at the top of a git checkout".to_string(),
        ));
    }
    let Some(_running) = lock::refresh_slot(&root)? else {
        println!("refresh: another refresh runs, so this one proves nothing");
        return Ok(Verdict::Pass);
    };
    let gh = Gh::new(&root);
    let waiting = land_needed(&gh)?;
    let mut prs = gh.open_prs()?;
    prs.retain(|pr| pr.auto_merge && closes_one(pr, &waiting));
    // GitHub numbers PRs in the order they were opened.
    prs.sort_by_key(|pr| pr.number);

    let actions = env::var("GITHUB_ACTIONS").as_deref() == Ok("true");
    let mut verdict = Verdict::Pass;
    let mut rows = Vec::new();
    let mut ready = Vec::new();
    let mut count = 0;
    // Every PR to prove goes yellow first, because the proofs run one at a time.
    for pr in prs {
        let mut row = Row {
            number: pr.number,
            verdict: "not proven",
            said: String::new(),
            took: None,
            open: None,
        };
        match needs_proof(&root, &gh, &pr) {
            Ok((head, true)) => {
                merge::post_gate(&gh, &head, "pending", &queued(count));
                count += 1;
                ready.push((rows.len(), pr, Some(head)));
            }
            Ok((_, false)) => ready.push((rows.len(), pr, None)),
            Err(Problem(message)) => {
                eprintln!("refresh: PR {}: {message}", pr.number);
                row.said = message;
                verdict = Verdict::Fail;
            }
        }
        rows.push(row);
    }
    let mut proofs = 0;
    // Found at the first PR that fails, and only then.
    let mut main_proof: Option<Result<(), String>> = None;
    for (at, pr, queued) in &ready {
        let row = &mut rows[*at];
        // One lock orders this PR's proof and squash with every other merge.
        // The turn keeps its place while its `prepare` holds the slot.
        let mut slot = lock::merge_slot(&root, false, || {
            if let Some(head) = queued {
                merge::post_gate(&gh, head, "pending", "Waiting: the merge slot");
            }
            eprintln!(
                "refresh: PR {} waits for the merge slot, which another run holds",
                pr.number
            );
        })?;
        let turn = lock::merge_turn(&root)?;
        // Another run can merge the PR while this one waits for the slot, and
        // the owner can take the approval or the issue back.
        let armed = match changed(&gh, pr) {
            Ok(Ok(armed)) => armed,
            Ok(Err(why)) => {
                println!("refresh: PR {} is left as it is, because {why}", pr.number);
                continue;
            }
            Err(Problem(message)) => {
                eprintln!("refresh: PR {}: {message}", pr.number);
                row.said = message;
                verdict = Verdict::Fail;
                continue;
            }
        };
        // A merge earlier in this run can leave this PR behind.
        let (seen, stale) = match needs_proof(&root, &gh, pr) {
            Ok(seen) => seen,
            Err(Problem(message)) => {
                eprintln!("refresh: PR {}: {message}", pr.number);
                row.said = message;
                verdict = Verdict::Fail;
                continue;
            }
        };
        let mut proven = true;
        let mut head = seen.clone();
        // Whether GitHub failed to say where the proof left the PR.
        let mut unread = false;
        if stale {
            proofs += 1;
            count = count.max(proofs);
            let place = format!("PR {} ({proofs} of {count})", pr.number);
            println!("refresh: proving {place}");
            if actions {
                println!("::group::{place}: {}", escaped(&pr.title));
            }
            let started = Instant::now();
            // It takes the slot ahead, in this turn.
            drop(slot);
            let prepared = prepare(&root, pr.number, &turn);
            if actions {
                println!("::endgroup::");
            }
            row.took = Some(started.elapsed());
            slot = lock::merge_slot(&root, true, || {})?;
            proven = prepared?;
            match current_head(&root, pr) {
                Some(now) => head = now,
                None => unread = true,
            }
        }
        let result = gh.described_status(&head, merge::GATE);
        unread |= result.is_err();
        if let Ok(Some((state, said))) = &result {
            row.said.clone_from(said);
            if state == "success" {
                row.verdict = "proven";
                if !proven {
                    verdict = Verdict::Fail;
                }
                // A head that was green already had its comment when the
                // owner armed it, because a proof that found something new
                // turned auto-merge off.
                let new = if stale {
                    newly_accepted(&gh, pr.number, &head, armed)
                } else {
                    Ok(Vec::new())
                };
                let merged = match new {
                    Ok(new) if new.is_empty() => merge_if_armed(&gh, pr, &head, &slot),
                    Ok(new) => Ok(Landed::Open(format!(
                        "its proof accepts what the owner didn't approve: {}. Arming its auto-merge again approves it",
                        new.join("; ")
                    ))),
                    Err(problem) => Err(problem),
                };
                match merged {
                    Ok(Landed::Merged(left)) => {
                        row.verdict = "merged";
                        // The next run lists only open PRs, so it can't retry.
                        for undone in left {
                            eprintln!("refresh: PR {} merged, but {undone}", pr.number);
                            verdict = Verdict::Fail;
                        }
                    }
                    Ok(Landed::Open(why)) => {
                        let why = format!("PR {} stays open, because {why}", pr.number);
                        println!("refresh: {why}");
                        row.open = Some(why);
                    }
                    Err(Problem(message)) => {
                        eprintln!("refresh: PR {} wasn't merged: {message}", pr.number);
                        verdict = Verdict::Fail;
                    }
                }
                continue;
            }
        }
        // Nothing after this merges, and the proof of `main` can take long.
        drop((slot, turn));
        if proven {
            // Someone pushed after the proof, so its green is on an older head.
            row.verdict = "proven";
            continue;
        }
        eprintln!("refresh: PR {} wasn't proven", pr.number);
        verdict = Verdict::Fail;
        // A `prepare` that fails a PR says why on its head. One that was
        // stopped early, or that refused the PR before the fetch, left no result.
        let why = match &result {
            Ok(Some((state, said))) if state == "failure" => said.clone(),
            _ => {
                match &result {
                    // What `prepare` left may be there, so nothing goes over it.
                    Err(Problem(message)) => eprintln!("refresh: PR {}: {message}", pr.number),
                    Ok(_) => merge::post_gate(&gh, &head, "failure", NOT_PROVEN),
                }
                row.said = NOT_PROVEN.to_string();
                NOT_PROVEN.to_string()
            }
        };
        if why == merge::MAIN_MOVED {
            println!(
                "refresh: `main` moved during the proof of PR {}, so the next run proves it again",
                pr.number
            );
            continue;
        }
        // What stops a proof after it passed isn't taken as the change's.
        // `prepare` pushes only then, so a head that moved says so even when
        // GitHub took no status. A head that someone else pushed during the
        // proof moved too, and nothing proved it. A read that failed may
        // have hidden either.
        if why == merge::PROVEN_THEN_STOPPED || head != seen || unread {
            println!(
                "refresh: PR {} keeps `{LAND_NEEDED}`, because its proof passed, its head moved, or GitHub didn't say, so the next run proves it again",
                pr.number
            );
            continue;
        }
        // A machine that can't prove `main` says nothing about the PR.
        if machine_could_cause(&why)
            && main_proof
                .get_or_insert_with(|| prove_main(&root, actions))
                .is_err()
        {
            println!(
                "refresh: PR {} keeps `{LAND_NEEDED}`, because `main` can't be proven on this machine either",
                pr.number
            );
            continue;
        }
        if let Err(Problem(message)) = hand_back(&gh, pr, &head, &why, &waiting) {
            eprintln!("refresh: PR {}: {message}", pr.number);
        }
    }
    let mut table = summary(&rows);
    if let Some(Err(why)) = &main_proof {
        let said = format!("`main` can't be proven on this machine: {why}");
        eprintln!("refresh: {said}");
        table.push_str(&format!("\n{said}\n"));
    }
    if actions && let Ok(path) = env::var("GITHUB_STEP_SUMMARY") {
        let appended = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .and_then(|mut file| file.write_all(table.as_bytes()));
        if let Err(e) = appended {
            eprintln!("refresh: couldn't write the summary to {path}: {e}");
        }
    }
    Ok(verdict)
}

/// What `merge_if_armed` did with a proven PR.
enum Landed {
    /// It left the PR open, for this reason.
    Open(String),
    /// The PR merged, and each of these steps after the squash failed.
    Merged(Vec<String>),
}

/// Squashes a proven PR, if the owner's approval still stands for the head
/// that was proven. The caller holds the slot, from the proof on.
fn merge_if_armed(gh: &Gh, pr: &OpenPr, head: &str, slot: &Hold) -> Result<Landed, Problem> {
    let number = pr.number;
    let now = gh.armed(number)?;
    // GitHub's own auto-merge can get there first, and then GitHub closes
    // the PR's issues itself.
    let mut close = false;
    if now.state != "MERGED" {
        if let Err(why) = approval(&now) {
            return Ok(Landed::Open(why));
        }
        if now.head != head {
            return Ok(Landed::Open(format!(
                "its head moved from {head} to {}",
                now.head
            )));
        }
        let read = gh.pull_request(number)?;
        // A green from an earlier proof can predate the review.
        if read.changes_requested {
            return Ok(Landed::Open("a review of it requested changes".to_string()));
        }
        // The label goes from the issues this run listed, and they are the
        // ones to close.
        if read.closing_issues != pr.closing_issues {
            return Ok(Landed::Open("the issue it closes changed".to_string()));
        }
        if let Err(problem) = gh.merge_pr(number, head, slot)
            && gh.armed(number)?.state != "MERGED"
        {
            return Err(problem);
        }
        println!("refresh: PR {number} merged at {head}");
        // `gh` doesn't say whose merge landed the PR. It succeeds when
        // GitHub's auto-merge got there during the call, and it can fail
        // after its own squash. So the issues are closed either way, and one
        // that GitHub closed stays as it is.
        close = true;
    }
    // Every step gets a try, so this doesn't stop at the first failure.
    let mut left = Vec::new();
    // So that the merge waits for no `mirror-labels` run.
    let labeled = pr.closing_issues.iter().map(|n| ("issue", *n));
    for (kind, number) in labeled.chain([("PR", number)]) {
        if let Err(Problem(message)) = gh.remove_label(number, LAND_NEEDED) {
            left.push(format!("{kind} {number} keeps `{LAND_NEEDED}`: {message}"));
        }
    }
    // Nor for GitHub to close the issues of the PR, which it may not.
    if close {
        for number in &pr.closing_issues {
            if let Err(Problem(message)) = gh.close_issue(*number) {
                left.push(format!("issue {number} is still open: {message}"));
            }
        }
    }
    Ok(Landed::Merged(left))
}

/// The open issues that have `land-needed`.
fn land_needed(gh: &Gh) -> Result<BTreeSet<u64>, Problem> {
    Ok(gh
        .open_issues()?
        .into_iter()
        .filter(|issue| issue.labels.iter().any(|label| label == LAND_NEEDED))
        .map(|issue| issue.number)
        .collect())
}

/// Whether the PR closes one of these issues.
fn closes_one(pr: &OpenPr, issues: &BTreeSet<u64>) -> bool {
    pr.closing_issues.iter().any(|n| issues.contains(n))
}

/// When the owner armed the PR, in seconds, or why their approval no longer
/// stands for it.
fn approval(now: &Armed) -> Result<i64, String> {
    if now.state != "OPEN" {
        return Err(format!("it is {}", now.state));
    }
    now.auto_merge
        .ok_or_else(|| "its auto-merge was turned off".to_string())
}

/// Why the run should leave a PR it listed, now that it holds the slot: the
/// PR merged or closed, its auto-merge was turned off, or its issue lost
/// `land-needed`. When the PR is still one to prove or merge, when the owner
/// armed it.
fn changed(gh: &Gh, pr: &OpenPr) -> Result<Result<i64, String>, Problem> {
    let armed = approval(&gh.armed(pr.number)?);
    if armed.is_ok() && !closes_one(pr, &land_needed(gh)?) {
        return Ok(Err(format!("its issue no longer has `{LAND_NEEDED}`")));
    }
    Ok(armed)
}

/// What this head's proof accepted that the owner didn't approve by arming
/// the PR at `armed`, before the proof turned auto-merge off.
fn newly_accepted(gh: &Gh, number: u64, head: &str, armed: i64) -> Result<Vec<String>, Problem> {
    let lists = accepts::acceptances(&gh.acceptances(number)?);
    Ok(accepts::newly_accepted(&lists, head, armed))
}

/// Lets the dispatcher restart the PR's issue, and tells the PR why, once for
/// each head. The comment comes last, so it says only what was done. Its
/// auto-merge stays armed, so the owner's approval stands for the fix.
fn hand_back(
    gh: &Gh,
    pr: &OpenPr,
    head: &str,
    why: &str,
    waiting: &BTreeSet<u64>,
) -> Result<(), Problem> {
    let mut failed = Vec::new();
    let mut unlabel = |number: u64| match gh.remove_label(number, LAND_NEEDED) {
        Ok(()) => true,
        Err(Problem(message)) => {
            failed.push(message);
            false
        }
    };
    unlabel(pr.number);
    let issues: Vec<u64> = pr
        .closing_issues
        .iter()
        .copied()
        .filter(|n| waiting.contains(n))
        .collect();
    // Every issue gets a try, so this doesn't stop at the first failure.
    let unlabeled = issues
        .iter()
        .map(|n| unlabel(*n))
        .filter(|done| !done)
        .count()
        == 0;
    let mark = format!("<!-- refresh {head} -->");
    let commented = gh.comment_bodies(pr.number).and_then(|bodies| {
        if bodies.contains(&mark) {
            return Ok(());
        }
        let issue = if unlabeled {
            format!("Its issue no longer has `{LAND_NEEDED}`, so a worker takes it up again. ")
        } else {
            String::new()
        };
        let body = format!(
            "The refresh couldn't prove this PR: {why}\n\n{issue}Auto-merge stays armed, and the refresh merges the PR once its fix is proven.\n\n{mark}\n"
        );
        gh.comment_on_pr(pr.number, &body)
    });
    if let Err(Problem(message)) = commented {
        failed.push(message);
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(Problem(failed.join("\n")))
    }
}

const NOT_PROVEN: &str = "Not proven";

/// Whether a fault of this machine or of `main` could have failed the proof,
/// by what `xtask/gate` says: a check failed, an error or the timeout stopped
/// the proof, or it ended without a result. Anything else is a refusal that `prepare` gave
/// in words, from the PR and git, which `refresh` takes as the change's.
fn machine_could_cause(why: &str) -> bool {
    why.starts_with("Failed: ")
        || why.starts_with("Failed in its own xtask: ")
        || why == "Stopped by an error"
        || why == merge::STOPPED_AT_THE_TIMEOUT
        || why == NOT_PROVEN
}

/// What happened to one PR, as a row of the run's summary.
struct Row {
    number: u64,
    verdict: &'static str,
    /// The PR's `xtask/gate` description.
    said: String,
    /// How long its proof took, if it ran one.
    took: Option<Duration>,
    /// Why a proven PR stays open, if it does.
    open: Option<String>,
}

/// A Markdown table with a row per PR, for the page of an Actions run.
fn summary(rows: &[Row]) -> String {
    let mut table = String::from("\n| PR | Verdict | Status | Took |\n|---|---|---|---|\n");
    for row in rows {
        let took = row.took.map(took).unwrap_or_default();
        let said = row.said.replace('|', "\\|").replace('\n', " ");
        let cells = [
            format!("#{}", row.number),
            row.verdict.to_string(),
            said,
            took,
        ];
        table.push('|');
        for cell in cells {
            if cell.is_empty() {
                table.push_str(" |");
            } else {
                table.push_str(&format!(" {cell} |"));
            }
        }
        table.push('\n');
    }
    for why in rows.iter().filter_map(|row| row.open.as_ref()) {
        table.push_str(&format!("\n{why}\n"));
    }
    table
}

/// Text that Actions reads as one workflow command's data, as written.
fn escaped(data: &str) -> String {
    data.replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

/// A length of time, to the second, such as a proof's or a wait's.
pub fn took(length: Duration) -> String {
    let seconds = length.as_secs();
    match seconds / 60 {
        0 => format!("{seconds}s"),
        minutes => format!("{minutes}m {:02}s", seconds % 60),
    }
}

/// Where a PR stands, by how many are proven before it.
fn queued(ahead: usize) -> String {
    match ahead {
        0 => "Queued, next".to_string(),
        1 => "Queued, 1 PR ahead".to_string(),
        more => format!("Queued, {more} PRs ahead"),
    }
}

/// The PR's head, and whether it needs a proof: when it is behind the newest
/// `main`, or has no green. A fork's branch isn't here to compare, and
/// `prepare` refuses it anyway.
fn needs_proof(root: &Path, gh: &Gh, pr: &OpenPr) -> Result<(String, bool), Problem> {
    if pr.from_fork {
        return Ok((pr.head.clone(), true));
    }
    let fetched = merge::fetch(root, &pr.head_ref)?;
    let args = [
        "merge-base",
        "--is-ancestor",
        &fetched.main,
        &fetched.branch,
    ];
    let behind = merge::exit_code(root, &args)? != 0;
    let green = !behind && gh.status_passed(&fetched.branch, merge::GATE)?;
    Ok((fetched.branch, !green))
}

/// A `prepare` that failed may have pushed a new head before it stopped.
/// `None` when the head can't be read.
fn current_head(root: &Path, pr: &OpenPr) -> Option<String> {
    if pr.from_fork {
        return Some(pr.head.clone());
    }
    merge::remote_commit(root, &format!("refs/heads/{}", pr.head_ref))
        .ok()
        .map(|head| head.unwrap_or_else(|| pr.head.clone()))
}

/// Runs the gate on the newest `main`, in a tree of its own, and says what
/// failed if it didn't pass.
fn prove_main(root: &Path, actions: bool) -> Result<(), String> {
    println!("refresh: proving `main`, to tell a fault of this machine from the PR's");
    if actions {
        println!("::group::`main`");
    }
    let proven = gate_main(root).and_then(|why| why.map_or(Ok(()), |why| Err(Problem(why))));
    if actions {
        println!("::endgroup::");
    }
    proven.map_err(|Problem(why)| why)
}

/// How long the gate of `main` may run, which is the merge timeout. A PR's
/// proof stops there, and a gate that hangs would otherwise hold the refresh,
/// and every refresh after it, for good.
const MAIN_SECONDS: u64 = 30 * 60;
/// Replaces the merge timeout, and this one with it.
const MAIN_SECONDS_OVERRIDE: &str = "XTASK_MERGE_TIMEOUT_SECONDS";

/// What failed in the gate of `main`, if anything did. The gate runs in a
/// process group of its own, which everything it starts joins, and the group
/// is stopped once it has run for the timeout.
fn gate_main(root: &Path) -> Result<Option<String>, Problem> {
    let spec = format!("+refs/heads/main:{}", merge::MAIN);
    base::git_text(root, &["fetch", "-q", "origin", &spec])?;
    let main = base::git_text(root, &["rev-parse", merge::MAIN])?;
    let tree = merge::Tree::of_main(root, main.trim())?;
    let program =
        env::current_exe().map_err(|e| Problem(format!("couldn't find this program: {e}")))?;
    let seconds = env::var(MAIN_SECONDS_OVERRIDE)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(MAIN_SECONDS);
    let (mut child, watcher) = group::start(
        Command::new(program)
            .arg("gate")
            .current_dir(&tree.path)
            // Rustup sets this for its children, and it would outrank the
            // toolchain file of `main`.
            .env_remove("RUSTUP_TOOLCHAIN")
            .stdout(Stdio::piped()),
    )
    .map_err(|e| Problem(format!("couldn't start the gate: {e}")))?;
    let group = child.id();
    let (ended, end) = mpsc::channel::<()>();
    let timer = thread::spawn(move || {
        let late =
            end.recv_timeout(Duration::from_secs(seconds)) == Err(mpsc::RecvTimeoutError::Timeout);
        if late {
            group::stop(group);
        }
        late
    });
    let mut failed = Vec::new();
    if let Some(out) = child.stdout.take() {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            println!("{line}");
            if let Some(check) = line.strip_prefix("fail ") {
                failed.push(check.to_string());
            }
        }
    }
    // Until the gate is reaped, its group is its own, so the timer, this, and
    // the watcher stop nothing else. This stops what the gate left running,
    // before its tree goes. The watcher goes next, while the group is still
    // the gate's, so that it never signals a group that is gone.
    drop(ended);
    let timed_out = timer.join().unwrap_or(false);
    group::stop(group);
    watcher.end();
    let status = child
        .wait()
        .map_err(|e| Problem(format!("couldn't wait for the gate: {e}")))?;
    Ok(match (status.success(), failed.is_empty()) {
        // A gate that passed just as the timer fired did pass.
        (false, _) if timed_out => Some(format!("its gate ran past the {seconds} second timeout")),
        (true, _) => None,
        (false, false) => Some(format!("Failed: {}", failed.join(", "))),
        (false, true) => Some(format!("its gate stopped: {status}")),
    })
}

/// Its own process, so that one PR's timeout or crash ends only that PR. It
/// runs in the refresh's `turn`.
fn prepare(root: &Path, number: u64, turn: &Hold) -> Result<bool, Problem> {
    let program =
        env::current_exe().map_err(|e| Problem(format!("couldn't find this program: {e}")))?;
    let mut command = Command::new(program);
    turn.pass_turn_to(&mut command);
    let status = command
        .args(["prepare", &number.to_string(), "--keep-budgets"])
        .current_dir(root)
        .status()
        .map_err(|e| Problem(format!("couldn't start `prepare`: {e}")))?;
    Ok(status.success())
}

#[cfg(test)]
mod tests {
    use super::{NOT_PROVEN, Row, escaped, machine_could_cause, queued, summary, took};
    use crate::merge;
    use std::time::Duration;

    #[test]
    fn a_queue_position_counts_the_prs_ahead() {
        assert_eq!(queued(0), "Queued, next");
        assert_eq!(queued(1), "Queued, 1 PR ahead");
        assert_eq!(queued(2), "Queued, 2 PRs ahead");
    }

    #[test]
    fn only_a_check_or_an_error_can_be_the_machines_fault() {
        for why in [
            "Failed: lint, tests",
            "Failed in its own xtask: journey",
            "Stopped by an error",
            merge::STOPPED_AT_THE_TIMEOUT,
            NOT_PROVEN,
        ] {
            assert!(machine_could_cause(why), "{why}");
        }
        for why in [
            "the tip of PR 9 has no ledger record, so there are no budgets to keep",
            "the branch doesn't rebase onto `main`. Rebase it by hand, and push it",
            "PR 9 changed during the merge, and the checks read what it said before",
            "Failed",
            merge::MAIN_MOVED,
        ] {
            assert!(!machine_could_cause(why), "{why}");
        }
    }

    #[test]
    fn a_proof_takes_seconds_and_then_minutes() {
        assert_eq!(took(Duration::from_millis(999)), "0s");
        assert_eq!(took(Duration::from_secs(59)), "59s");
        assert_eq!(took(Duration::from_secs(60)), "1m 00s");
        assert_eq!(took(Duration::from_secs(605)), "10m 05s");
    }

    #[test]
    fn a_fold_title_keeps_to_its_line() {
        assert_eq!(escaped("100% %0A\r\nx"), "100%25 %250A%0D%0Ax");
    }

    #[test]
    fn a_summary_cell_keeps_to_its_row() {
        let row = Row {
            number: 3,
            verdict: "not proven",
            said: "a | b\nc".to_string(),
            took: None,
            open: None,
        };
        assert!(summary(&[row]).ends_with("\n| #3 | not proven | a \\| b c | |\n"));
    }
}
