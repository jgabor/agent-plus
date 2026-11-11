//! `cargo xtask refresh`: proves each `land-needed` PR again on the newest
//! `main`, so that a merge button is never pressed on a stale proof.

use crate::forge::{Gh, OpenPr};
use crate::work::LAND_NEEDED;
use crate::{Problem, Verdict, merge, workspace};
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
    prs.retain(|pr| pr.closing_issues.iter().any(|n| waiting.contains(n)));
    // GitHub numbers PRs in the order they were opened.
    prs.sort_by_key(|pr| pr.number);

    let actions = env::var("GITHUB_ACTIONS").as_deref() == Ok("true");
    let mut verdict = Verdict::Pass;
    let mut rows = Vec::new();
    let mut stale = Vec::new();
    // Every stale PR goes yellow first, because the proofs run one at a time.
    for pr in prs {
        let mut row = Row {
            number: pr.number,
            verdict: "not proven",
            said: String::new(),
            took: None,
        };
        match stale_head(&root, &pr) {
            Ok(None) => {
                println!("refresh: PR {} is up to date", pr.number);
                row.verdict = "up to date";
            }
            Ok(Some(head)) => {
                merge::post_gate(&gh, &head, "pending", &queued(stale.len()));
                stale.push((rows.len(), pr));
            }
            Err(Problem(message)) => {
                eprintln!("refresh: PR {}: {message}", pr.number);
                row.said = message;
                verdict = Verdict::Fail;
            }
        }
        rows.push(row);
    }
    let count = stale.len();
    for (i, (at, pr)) in stale.iter().enumerate() {
        let place = format!("PR {} ({} of {count})", pr.number, i + 1);
        println!("refresh: proving {place}");
        if actions {
            println!("::group::{place}: {}", pr.title);
        }
        let started = Instant::now();
        let proven = prepare(&root, pr.number);
        if actions {
            println!("::endgroup::");
        }
        let row = &mut rows[*at];
        row.took = Some(started.elapsed());
        let proven = proven?;
        let head = current_head(&root, pr);
        let result = gh.described_status(&head, merge::GATE);
        if let Ok(Some((_, said))) = &result {
            row.said.clone_from(said);
        }
        if proven {
            row.verdict = "proven";
            continue;
        }
        eprintln!("refresh: PR {} wasn't proven", pr.number);
        // A `prepare` that fails a PR says why on its head, and one that was
        // stopped after its green did prove the PR. One that was stopped
        // earlier, or that refused the PR before the fetch, left no result.
        if !matches!(&result, Ok(Some((state, _))) if state == "failure" || state == "success") {
            merge::post_gate(&gh, &head, "failure", NOT_PROVEN);
            row.said = NOT_PROVEN.to_string();
        }
        verdict = Verdict::Fail;
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
    use super::{Row, queued, summary, took};
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
