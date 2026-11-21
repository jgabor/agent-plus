//! `cargo xtask refresh`: proves each `land-needed` PR that the owner approved
//! by arming auto-merge on the newest `main`, and merges it.

use crate::forge::{Gh, OpenPr};
use crate::work::LAND_NEEDED;
use crate::{Problem, Verdict, base, lock, merge, workspace};
use std::collections::BTreeSet;
use std::env;
use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
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
    let gh = Gh::new(&root);
    let waiting: BTreeSet<u64> = gh
        .open_issues()?
        .into_iter()
        .filter(|issue| issue.labels.iter().any(|label| label == LAND_NEEDED))
        .map(|issue| issue.number)
        .collect();
    let mut prs = gh.open_prs()?;
    prs.retain(|pr| pr.auto_merge && pr.closing_issues.iter().any(|n| waiting.contains(n)));
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
        };
        match needs_proof(&root, &gh, &pr) {
            Ok((head, true)) => {
                merge::post_gate(&gh, &head, "pending", &queued(count));
                count += 1;
                ready.push((rows.len(), pr));
            }
            Ok((_, false)) => ready.push((rows.len(), pr)),
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
    for (at, pr) in &ready {
        let row = &mut rows[*at];
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
        let mut head = seen;
        if stale {
            proofs += 1;
            count = count.max(proofs);
            let place = format!("PR {} ({proofs} of {count})", pr.number);
            println!("refresh: proving {place}");
            if actions {
                println!("::group::{place}: {}", escaped(&pr.title));
            }
            let started = Instant::now();
            let prepared = prepare(&root, pr.number);
            if actions {
                println!("::endgroup::");
            }
            row.took = Some(started.elapsed());
            proven = prepared?;
            head = current_head(&root, pr);
        }
        let result = gh.described_status(&head, merge::GATE);
        if let Ok(Some((state, said))) = &result {
            row.said.clone_from(said);
            if state == "success" {
                row.verdict = "proven";
                if !proven {
                    verdict = Verdict::Fail;
                }
                match merge_if_armed(&root, &gh, pr, &head) {
                    Ok(None) => row.verdict = "merged",
                    Ok(Some(why)) => {
                        println!("refresh: PR {} stays open, because {why}", pr.number)
                    }
                    Err(Problem(message)) => {
                        eprintln!("refresh: PR {} wasn't merged: {message}", pr.number);
                        verdict = Verdict::Fail;
                    }
                }
                continue;
            }
        }
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
                merge::post_gate(&gh, &head, "failure", NOT_PROVEN);
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
        // A runner that can't prove `main` says nothing about the PR.
        if runner_could_cause(&why)
            && main_proof
                .get_or_insert_with(|| prove_main(&root, actions))
                .is_err()
        {
            println!(
                "refresh: PR {} keeps `{LAND_NEEDED}`, because `main` can't be proven on the runner either",
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
        let said = format!("`main` can't be proven on the runner: {why}");
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

/// Squashes a proven PR, if the owner's approval still stands for the head
/// that was proven. Returns why it left the PR open, if it did.
fn merge_if_armed(
    root: &Path,
    gh: &Gh,
    pr: &OpenPr,
    head: &str,
) -> Result<Option<String>, Problem> {
    let number = pr.number;
    let slot = lock::merge_slot(root)?;
    let now = gh.armed(number)?;
    // GitHub's own auto-merge can get there first.
    if now.state != "MERGED" {
        if now.state != "OPEN" {
            return Ok(Some(format!("it is {}", now.state)));
        }
        if !now.auto_merge {
            return Ok(Some("its auto-merge was turned off".to_string()));
        }
        if now.head != head {
            return Ok(Some(format!("its head moved from {head} to {}", now.head)));
        }
        // A green from an earlier proof can predate the review.
        if gh.pull_request(number)?.changes_requested {
            return Ok(Some("a review of it requested changes".to_string()));
        }
        if let Err(problem) = gh.merge_pr(number, head, &slot)
            && gh.armed(number)?.state != "MERGED"
        {
            return Err(problem);
        }
        println!("refresh: PR {number} merged at {head}");
    }
    // A merge with the workflow's token starts no `mirror-labels` run.
    remove_land_needed(gh, pr)?;
    Ok(None)
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

/// From the PR and every issue it closes, which `remove_label` allows when
/// one has no such label.
fn remove_land_needed(gh: &Gh, pr: &OpenPr) -> Result<(), Problem> {
    for number in pr.closing_issues.iter().chain([&pr.number]) {
        gh.remove_label(*number, LAND_NEEDED)?;
    }
    Ok(())
}

const NOT_PROVEN: &str = "Not proven";

/// Whether a fault of the runner or of `main` could have failed the proof,
/// by what `xtask/gate` says: a check failed, an error stopped the proof, or
/// it ended without a result. Anything else is a refusal that `prepare` gave
/// in words, from the PR and git, which `refresh` takes as the change's.
fn runner_could_cause(why: &str) -> bool {
    why.starts_with("Failed: ")
        || why.starts_with("Failed in its own xtask: ")
        || why == "Stopped by an error"
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
    table
}

/// Text that Actions reads as one workflow command's data, as written.
fn escaped(data: &str) -> String {
    data.replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

/// A proof's length, to the second.
fn took(length: Duration) -> String {
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
fn current_head(root: &Path, pr: &OpenPr) -> String {
    if pr.from_fork {
        return pr.head.clone();
    }
    merge::remote_commit(root, &format!("refs/heads/{}", pr.head_ref))
        .ok()
        .flatten()
        .unwrap_or_else(|| pr.head.clone())
}

/// Runs the gate on the newest `main`, in a tree of its own, and says what
/// failed if it didn't pass.
fn prove_main(root: &Path, actions: bool) -> Result<(), String> {
    println!("refresh: proving `main`, to tell a fault of the runner from the PR's");
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
/// proof stops there, and a gate that hangs on the runner would otherwise hold
/// the refresh for as long as its job may run.
const MAIN_SECONDS: u64 = 30 * 60;
/// Replaces the merge timeout, and this one with it.
const MAIN_SECONDS_OVERRIDE: &str = "XTASK_MERGE_TIMEOUT_SECONDS";
/// Stops the gate's group when this process dies, as on Ctrl-C, which reaches
/// only this process's group. Only this process holds the other end of its
/// input. A merge's supervisor has the same watcher, for the same reason.
const WATCHER: &str = "read _; env kill -STOP -- \"-$1\"; env kill -KILL -- \"-$1\"";

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
    // Made first, so that nothing can fail between the gate's start and its
    // watcher's.
    let (watched, _alive) =
        io::pipe().map_err(|e| Problem(format!("couldn't make a pipe: {e}")))?;
    let mut child = Command::new(program)
        .arg("gate")
        .current_dir(&tree.path)
        // Rustup sets this for its children, and it would outrank the
        // toolchain file of `main`.
        .env_remove("RUSTUP_TOOLCHAIN")
        .stdout(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| Problem(format!("couldn't start the gate: {e}")))?;
    let group = child.id();
    let watcher = Command::new("sh")
        .args(["-c", WATCHER, "sh", &group.to_string()])
        .stdin(watched)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();
    let mut watcher = match watcher {
        Ok(watcher) => watcher,
        Err(error) => {
            stop_group(group);
            let _ = child.wait();
            return Err(Problem(format!(
                "couldn't start the gate's watcher: {error}"
            )));
        }
    };
    let (ended, end) = mpsc::channel::<()>();
    let timer = thread::spawn(move || {
        let late =
            end.recv_timeout(Duration::from_secs(seconds)) == Err(mpsc::RecvTimeoutError::Timeout);
        if late {
            stop_group(group);
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
    // Until the gate is reaped, its group is its own, so the timer and this
    // stop nothing else. This stops what the gate left running, before its
    // tree goes. After it, the watcher has nothing to stop, and it mustn't
    // signal a group that is gone.
    drop(ended);
    let timed_out = timer.join().unwrap_or(false);
    stop_group(group);
    let status = child.wait();
    let _ = watcher.kill();
    let _ = watcher.wait();
    let status = status.map_err(|e| Problem(format!("couldn't wait for the gate: {e}")))?;
    Ok(match (status.success(), failed.is_empty()) {
        // A gate that passed just as the timer fired did pass.
        (false, _) if timed_out => Some(format!("its gate ran past the {seconds} second timeout")),
        (true, _) => None,
        (false, false) => Some(format!("Failed: {}", failed.join(", "))),
        (false, true) => Some(format!("its gate stopped: {status}")),
    })
}

/// Stops the group first, so that none of it runs while the kill goes through
/// it, as a merge's supervisor stops its worker. `xtask` has no dependencies,
/// so `kill` sends the signals, and it fails on a group that is gone.
fn stop_group(group: u32) {
    let target = format!("-{group}");
    for signal in ["-STOP", "-KILL"] {
        let _ = Command::new("kill")
            .args([signal, "--", &target])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
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
    use super::{NOT_PROVEN, Row, escaped, queued, runner_could_cause, summary, took};
    use crate::merge;
    use std::time::Duration;

    #[test]
    fn a_queue_position_counts_the_prs_ahead() {
        assert_eq!(queued(0), "Queued, next");
        assert_eq!(queued(1), "Queued, 1 PR ahead");
        assert_eq!(queued(2), "Queued, 2 PRs ahead");
    }

    #[test]
    fn only_a_check_or_an_error_can_be_the_runners_fault() {
        for why in [
            "Failed: lint, tests",
            "Failed in its own xtask: journey",
            "Stopped by an error",
            NOT_PROVEN,
        ] {
            assert!(runner_could_cause(why), "{why}");
        }
        for why in [
            "the tip of PR 9 has no ledger record, so there are no budgets to keep",
            "the branch doesn't rebase onto `main`. Rebase it by hand, and push it",
            "PR 9 changed during the merge, and the checks read what it said before",
            "Failed",
            merge::MAIN_MOVED,
        ] {
            assert!(!runner_could_cause(why), "{why}");
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
        };
        assert!(summary(&[row]).ends_with("\n| #3 | not proven | a \\| b c | |\n"));
    }
}
