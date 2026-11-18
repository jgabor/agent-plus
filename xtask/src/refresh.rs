//! `cargo xtask refresh`: proves each `land-needed` PR that the owner approved
//! by arming auto-merge on the newest `main`, and merges it.

use crate::forge::{Gh, OpenPr};
use crate::work::LAND_NEEDED;
use crate::{Problem, Verdict, lock, merge, workspace};
use std::collections::BTreeSet;
use std::env;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::process::Command;
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
        if let Err(Problem(message)) = hand_back(&gh, pr, &why, &waiting) {
            eprintln!("refresh: PR {}: {message}", pr.number);
        }
    }
    if actions && let Ok(path) = env::var("GITHUB_STEP_SUMMARY") {
        let appended = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .and_then(|mut file| file.write_all(summary(&rows).as_bytes()));
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
        if let Err(problem) = gh.merge_pr(number, head, &slot) {
            if gh.armed(number)?.state != "MERGED" {
                return Err(problem);
            }
        }
        println!("refresh: PR {number} merged at {head}");
    }
    // A merge with the workflow's token starts no `mirror-labels` run.
    remove_land_needed(gh, pr)?;
    Ok(None)
}

/// Tells the PR why it failed, and lets the dispatcher restart its issue.
/// Its auto-merge stays armed, so the owner's approval stands for the fix.
fn hand_back(gh: &Gh, pr: &OpenPr, why: &str, waiting: &BTreeSet<u64>) -> Result<(), Problem> {
    let body = format!(
        "The refresh couldn't prove this PR: {why}\n\nIts issue no longer has `{LAND_NEEDED}`, so a worker takes it up again. Auto-merge stays armed, and the refresh merges the PR once its fix is proven."
    );
    gh.comment_on_pr(pr.number, &body)?;
    let issues = pr.closing_issues.iter().filter(|n| waiting.contains(n));
    for issue in issues {
        gh.remove_label(*issue, LAND_NEEDED)?;
    }
    gh.remove_label(pr.number, LAND_NEEDED)
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
    use super::{Row, escaped, queued, summary, took};
    use std::time::Duration;

    #[test]
    fn a_queue_position_counts_the_prs_ahead() {
        assert_eq!(queued(0), "Queued, next");
        assert_eq!(queued(1), "Queued, 1 PR ahead");
        assert_eq!(queued(2), "Queued, 2 PRs ahead");
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
