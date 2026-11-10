//! `cargo xtask roadmap`: the milestones, what comes after them, and what
//! waits on the owner. Nobody types it: every line comes from the journey
//! file, the journey's results, or GitHub.

use crate::forge::{Gh, OpenPr, RoadmapIssue};
use crate::journey::{self, Milestone, Outcome};
use crate::work::{ANSWER_NEEDED, LAND_NEEDED, PARKED};
use crate::{Problem, Verdict, lock, report, workspace};
use std::path::Path;

/// The roadmap issue's title. The lowest open issue with it is the one.
const ISSUE_TITLE: &str = "Roadmap";
const HEADER: &str = "The nightly job writes this issue from `cargo xtask roadmap`, so don't edit it. To change it, change `harness/journey.toml` or the issues it lists.\n";

/// Prints the roadmap, and writes nothing.
pub fn run() -> Result<Verdict, Problem> {
    let root = workspace::root()?;
    // GitHub comes first, so that a run that can't reach it stops early.
    let gh = Gh::new(&root);
    let (issues, prs) = (gh.roadmap_issues()?, gh.open_prs()?);
    let hold = lock::shared(&root)?;
    let outcomes = journey::outcomes(&root)?;
    drop(hold);
    let milestones = journey::milestones(&root)?;
    print!("{}", markdown(&milestones, &outcomes, &issues, &prs));
    Ok(Verdict::Pass)
}

/// Makes the roadmap the body of the open issue with the roadmap's title,
/// unless it already is. With no such issue, there is nowhere to write.
pub fn write(root: &Path, outcomes: &[Outcome]) -> Result<(), Problem> {
    let gh = Gh::new(root);
    let issues = gh.roadmap_issues()?;
    let Some(issue) = roadmap_issue(&issues) else {
        return Ok(());
    };
    let milestones = journey::milestones(root)?;
    let text = markdown(&milestones, outcomes, &issues, &gh.open_prs()?);
    // An edit on the website saves its line ends as `\r\n`.
    if issue.body.replace("\r\n", "\n").trim_end() != text.trim_end() {
        gh.edit_issue(issue.number, &text)?;
    }
    Ok(())
}

fn roadmap_issue(issues: &[RoadmapIssue]) -> Option<&RoadmapIssue> {
    let titled = |issue: &&RoadmapIssue| issue.open && issue.title == ISSUE_TITLE;
    issues
        .iter()
        .filter(titled)
        .min_by_key(|issue| issue.number)
}

/// The same inputs give the same text, whatever order GitHub listed them in.
fn markdown(
    milestones: &[Milestone],
    outcomes: &[Outcome],
    all: &[RoadmapIssue],
    prs: &[OpenPr],
) -> String {
    let mut issues: Vec<&RoadmapIssue> = all.iter().collect();
    issues.sort_by_key(|issue| issue.number);
    let (passing, steps) = journey::tally(outcomes);
    let mut text = format!("{HEADER}\n## v0.1.0\n\njourney {passing} of {steps}\n");
    for (index, milestone) in milestones.iter().enumerate() {
        let in_it = |step: &&Outcome| step.milestone == milestone.name;
        let steps: Vec<&Outcome> = outcomes.iter().filter(in_it).collect();
        let done = steps.iter().all(|step| step.state == "pass");
        let commands: Vec<String> = milestone
            .commands
            .iter()
            .map(|command| format!("`{command}`"))
            .collect();
        let commands = if commands.is_empty() {
            "none.".to_string()
        } else {
            commands.join(", ")
        };
        text.push_str(&format!(
            "\n### {}. {}\n\nParent: {}\nCommands: {commands}\n\n",
            index + 1,
            milestone.name,
            parent(&issues, &milestone.name, done)
        ));
        for step in steps {
            let (mark, note) = match step.state {
                "pass" => ("x", ""),
                "fail" => (" ", " (fails)"),
                _ => (" ", " (skipped)"),
            };
            let (id, name) = (step.id, &step.name);
            text.push_str(&format!("- [{mark}] Step {id}: {name}{note}\n"));
        }
    }

    let roadmap = roadmap_issue(all).map(|issue| issue.number);
    let later = issues
        .iter()
        .filter(|issue| issue.open && roadmap.is_some() && issue.parent == roadmap)
        .map(|issue| line(issue.number, &issue.title))
        .collect();
    section(&mut text, "## After v0.1.0", later);

    text.push_str("\n## Waiting on the owner\n");
    section(&mut text, "### To merge", parked(&issues, prs, LAND_NEEDED));
    let to_answer = parked(&issues, prs, ANSWER_NEEDED);
    section(&mut text, "### To answer", to_answer);
    let unlabeled = issues
        .iter()
        .filter(|issue| issue.open && issue.parent.is_none() && !written(&issue.title))
        .filter(|issue| {
            let waits = |label: &String| label.starts_with("auto:") || is_parked(label);
            !issue.labels.iter().any(waits)
        })
        .map(|issue| line(issue.number, &issue.title))
        .collect();
    section(&mut text, "### To label or close", unlabeled);
    text
}

/// An issue that `xtask` writes, which the owner neither labels nor closes.
fn written(title: &str) -> bool {
    title == ISSUE_TITLE || title == report::ISSUE_TITLE
}

fn is_parked(label: &str) -> bool {
    PARKED.contains(&label)
}

fn line(number: u64, title: &str) -> String {
    format!("#{number} {title}")
}

fn section(text: &mut String, heading: &str, lines: Vec<String>) {
    text.push_str(&format!("\n{heading}\n\n"));
    if lines.is_empty() {
        text.push_str("Nothing.\n");
    }
    for line in lines {
        text.push_str(&format!("- {line}\n"));
    }
}

/// The issue that delivers this milestone, which is the one whose body names
/// it on a line of its own, and how far its sub-issues are. An open issue
/// comes before a closed one, and then the lowest number does.
fn parent(issues: &[&RoadmapIssue], milestone: &str, done: bool) -> String {
    let named = format!("Milestone: {milestone}.");
    let names_it = |issue: &&&RoadmapIssue| issue.body.lines().any(|line| line.trim() == named);
    let found = issues
        .iter()
        .filter(names_it)
        .min_by_key(|issue| (!issue.open, issue.number));
    let Some(found) = found else {
        return "none.".to_string();
    };
    let under = |issue: &&&RoadmapIssue| issue.parent == Some(found.number);
    let total = issues.iter().filter(under).count();
    let closed = issues.iter().filter(under).filter(|sub| !sub.open).count();
    let state = if found.open { "open" } else { "closed" };
    let ready = if found.open && done {
        " Every step passes, so it is ready to close."
    } else {
        ""
    };
    format!(
        "#{}, {state}, with {closed} of {total} sub-issues closed.{ready}",
        found.number
    )
}

/// The open PRs with this label, then the open issues with it that none of
/// those PRs closes. A label is on both sides of a PR and its issue, and the
/// PR is the one to act on.
fn parked(issues: &[&RoadmapIssue], prs: &[OpenPr], label: &str) -> Vec<String> {
    let has = |labels: &[String]| labels.iter().any(|name| name == label);
    let prs: Vec<&OpenPr> = prs.iter().filter(|pr| has(&pr.labels)).collect();
    let mut lines: Vec<(u64, String)> = Vec::new();
    for pr in &prs {
        let mut closes = pr.closing_issues.clone();
        closes.sort_unstable();
        let closes: Vec<String> = closes.iter().map(|issue| format!("#{issue}")).collect();
        let closes = if closes.is_empty() {
            String::new()
        } else {
            format!(" (closes {})", closes.join(", "))
        };
        lines.push((pr.number, format!("{}{closes}", line(pr.number, &pr.title))));
    }
    for issue in issues
        .iter()
        .filter(|issue| issue.open && has(&issue.labels))
    {
        if !prs
            .iter()
            .any(|pr| pr.closing_issues.contains(&issue.number))
        {
            lines.push((issue.number, line(issue.number, &issue.title)));
        }
    }
    lines.sort();
    lines.into_iter().map(|(_, line)| line).collect()
}
