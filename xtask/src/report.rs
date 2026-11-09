//! `cargo xtask report`: the last day on `main`, from the ledger and the open
//! issues.

use crate::bench;
use crate::forge::{Forge, Gh, OpenIssue};
use crate::json::quoted;
use crate::ledger::{self, Record};
use crate::{Problem, Verdict, base, journey, workspace};
use std::path::Path;

/// The report issue's title.
pub const ISSUE_TITLE: &str = "Nightly report";
const HEADING: &str = "# Nightly report for `";
/// How far back to look when no earlier report can say where to start.
const WINDOW: &str = "--since=24.hours";
const WAITING_LABEL: &str = "needs-owner";
const MUTATION_FAILED: &str = "\n- fail mutation\n";

struct Scores {
    proven: usize,
    promises: usize,
    passing: usize,
    steps: usize,
    met: usize,
    targets: usize,
}

struct Merge {
    commit: String,
    subject: String,
    issue: usize,
    pr: usize,
}

struct Regression {
    commit: String,
    issue: usize,
    pr: usize,
    what: String,
}

struct Unrecorded {
    commit: String,
    subject: String,
}

struct Waiting {
    issue: u64,
    title: String,
}

pub struct Report {
    commit: String,
    scores: Option<Scores>,
    left_out: usize,
    merged: Vec<Merge>,
    regressed: Vec<Regression>,
    unrecorded: Vec<Unrecorded>,
    waiting: Vec<Waiting>,
}

pub fn run(json: bool) -> Result<Verdict, Problem> {
    let report = build(&workspace::root()?)?;
    if json {
        println!("{}", report.json());
    } else {
        print!("{}", report.markdown(&[]));
    }
    Ok(Verdict::Pass)
}

/// The `git log` argument that limits it to what no report has covered yet.
pub fn start(root: &Path, gh: &Gh, issues: &[OpenIssue]) -> Result<String, Problem> {
    start_after(root, gh, issues, |_| true)
}

/// Like `start`, but a night when the mutation step failed covered nothing.
pub fn mutation_start(root: &Path, gh: &Gh, issues: &[OpenIssue]) -> Result<String, Problem> {
    start_after(root, gh, issues, |report| !report.contains(MUTATION_FAILED))
}

fn start_after(
    root: &Path,
    gh: &Gh,
    issues: &[OpenIssue],
    counts: fn(&str) -> bool,
) -> Result<String, Problem> {
    Ok(match last_reported(root, gh, issues, counts)? {
        Some(commit) => format!("^{commit}"),
        None => WINDOW.to_string(),
    })
}

pub fn build(root: &Path) -> Result<Report, Problem> {
    let budgets = bench::read_budgets(root)?;
    let scores = ledger::at_head(root)?.map(|record| {
        let (met, targets) = bench::met(&budgets, &record.budgets);
        Scores {
            proven: record.proven,
            promises: record.promises,
            passing: record.passing,
            steps: record.steps,
            met,
            targets,
        }
    });
    let mut report = Report {
        commit: base::git_text(root, &["rev-parse", "--short", "HEAD"])?
            .trim()
            .to_string(),
        scores,
        left_out: journey::left_out(root)?,
        merged: Vec::new(),
        regressed: Vec::new(),
        unrecorded: Vec::new(),
        waiting: Vec::new(),
    };
    let gh = Gh::new(root);
    let issues = gh.open_issues()?;
    let start = start(root, &gh, &issues)?;
    let log = base::git_text(root, &["log", &start, "--format=%H %h %s", "HEAD"])?;
    for line in log.lines() {
        let mut fields = line.splitn(3, ' ');
        let (Some(full), Some(short), subject) = (fields.next(), fields.next(), fields.next())
        else {
            return Err(Problem(format!("`git log` printed `{line}`")));
        };
        let (commit, subject) = (short.to_string(), subject.unwrap_or_default().to_string());
        let Some(record) = ledger::of_commit(root, full)? else {
            report.unrecorded.push(Unrecorded { commit, subject });
            continue;
        };
        for what in regressions(root, full, &record)? {
            report.regressed.push(Regression {
                commit: commit.clone(),
                issue: record.issue,
                pr: record.pr,
                what,
            });
        }
        report.merged.push(Merge {
            commit,
            subject,
            issue: record.issue,
            pr: record.pr,
        });
    }
    report.waiting = issues
        .into_iter()
        .filter(|issue| issue.labels.iter().any(|label| label == WAITING_LABEL))
        .map(|issue| Waiting {
            issue: issue.number,
            title: issue.title,
        })
        .collect();
    Ok(report)
}

/// The commit the newest posted report was for, so that a night that ran late
/// or not at all loses nothing. A comment is only data, so only the owner's
/// count, and anything but a commit in this history means there's no report.
fn last_reported(
    root: &Path,
    gh: &Gh,
    issues: &[OpenIssue],
    counts: fn(&str) -> bool,
) -> Result<Option<String>, Problem> {
    let Some(issue) = issues.iter().find(|issue| issue.title == ISSUE_TITLE) else {
        return Ok(None);
    };
    let comments = gh.issue(issue.number)?.comments;
    let owner = gh.owner()?;
    let newest = comments
        .iter()
        .rev()
        .filter(|comment| comment.author == owner && counts(&comment.body))
        .find_map(|comment| comment.body.strip_prefix(HEADING)?.split('`').next());
    let Some(commit) = newest else {
        return Ok(None);
    };
    if !commit.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(None);
    }
    let in_history = base::git(root, &["merge-base", "--is-ancestor", commit, "HEAD"])?
        .status
        .success();
    Ok(in_history.then(|| commit.to_string()))
}

/// What this commit's record made worse, and any reason it gave.
fn regressions(root: &Path, commit: &str, record: &Record) -> Result<Vec<String>, Problem> {
    let mut found = Vec::new();
    let parent = format!("{commit}~1");
    if base::commit_exists(root, &parent)?
        && let Some(previous) = ledger::newest(root, &parent)?
    {
        // Both sides use this commit's targets, so that a target changed later
        // isn't blamed on this commit.
        let budgets = bench::budgets_at(root, commit)?;
        let met = |record: &Record| bench::met(&budgets, &record.budgets).0;
        for (name, was, now) in [
            ("spec", previous.proven, record.proven),
            ("journey", previous.passing, record.passing),
            ("budgets", met(&previous), met(record)),
        ] {
            if now < was {
                found.push(format!("{name} fell from {was} to {now}"));
            }
        }
    }
    if !record.reason.is_empty() {
        found.push(format!("a budget went past its band: {}", record.reason));
    }
    Ok(found)
}

fn section(text: &mut String, heading: &str, lines: Vec<String>) {
    text.push_str(&format!("\n## {heading}\n\n"));
    if lines.is_empty() {
        text.push_str("Nothing.\n");
    }
    for line in lines {
        text.push_str(&format!("- {line}\n"));
    }
}

impl Report {
    /// The nightly job passes its checks. A report on its own has none.
    pub fn markdown(&self, checks: &[(&str, &str)]) -> String {
        let mut text = format!("# Nightly report for `{}`\n\n", self.commit);
        match &self.scores {
            Some(s) => text.push_str(&format!(
                "spec {} of {}, journey {} of {}, budgets {} of {}\n",
                s.proven, s.promises, s.passing, s.steps, s.met, s.targets
            )),
            None => text.push_str("No record in the ledger yet, so there are no scores.\n"),
        }
        text.push_str(&format!(
            "\nJourney steps that spend money, and weren't run: {}\n",
            self.left_out
        ));
        if !checks.is_empty() {
            let lines = checks
                .iter()
                .map(|(name, state)| format!("{state} {name}"))
                .collect();
            section(&mut text, "Checks", lines);
        }
        let merged = self
            .merged
            .iter()
            .map(|m| format!("`{}` {}, issue #{}", m.commit, m.subject, m.issue))
            .collect();
        section(&mut text, "Merged", merged);
        let regressed = self
            .regressed
            .iter()
            .map(|r| format!("`{}` {}", r.commit, r.what))
            .collect();
        section(&mut text, "Regressed", regressed);
        let unrecorded = self
            .unrecorded
            .iter()
            .map(|u| format!("`{}` {}", u.commit, u.subject))
            .collect();
        section(&mut text, "Not in the ledger", unrecorded);
        let waiting = self
            .waiting
            .iter()
            .map(|w| format!("#{} {}", w.issue, w.title))
            .collect();
        section(&mut text, "Waiting on the owner", waiting);
        text
    }

    fn json(&self) -> String {
        let scores = self.scores.as_ref().map_or("null".to_string(), |s| {
            format!(
                "{{\"spec\":{{\"proven\":{},\"promises\":{}}},\"journey\":{{\"passing\":{},\"steps\":{}}},\"budgets\":{{\"met\":{},\"targets\":{}}}}}",
                s.proven, s.promises, s.passing, s.steps, s.met, s.targets
            )
        });
        let merged: Vec<String> = self
            .merged
            .iter()
            .map(|m| {
                format!(
                    "{{\"commit\":{},\"subject\":{},\"issue\":{},\"pr\":{}}}",
                    quoted(&m.commit),
                    quoted(&m.subject),
                    m.issue,
                    m.pr
                )
            })
            .collect();
        let regressed: Vec<String> = self
            .regressed
            .iter()
            .map(|r| {
                format!(
                    "{{\"commit\":{},\"issue\":{},\"pr\":{},\"what\":{}}}",
                    quoted(&r.commit),
                    r.issue,
                    r.pr,
                    quoted(&r.what)
                )
            })
            .collect();
        let unrecorded: Vec<String> = self
            .unrecorded
            .iter()
            .map(|u| {
                format!(
                    "{{\"commit\":{},\"subject\":{}}}",
                    quoted(&u.commit),
                    quoted(&u.subject)
                )
            })
            .collect();
        let waiting: Vec<String> = self
            .waiting
            .iter()
            .map(|w| format!("{{\"issue\":{},\"title\":{}}}", w.issue, quoted(&w.title)))
            .collect();
        format!(
            "{{\"commit\":{},\"scores\":{scores},\"left_out\":{},\"merged\":[{}],\"regressed\":[{}],\"unrecorded\":[{}],\"waiting\":[{}]}}",
            quoted(&self.commit),
            self.left_out,
            merged.join(","),
            regressed.join(","),
            unrecorded.join(","),
            waiting.join(",")
        )
    }
}
