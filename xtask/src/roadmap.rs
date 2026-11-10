//! `cargo xtask roadmap`: the milestones, what comes after them, and what
//! waits on the owner. Nobody types it: every line comes from the journey
//! file, the journey's results, SPEC.md and the checks that prove it, or
//! GitHub.

use crate::forge::{Gh, OpenPr, RoadmapIssue};
use crate::journey::{self, Milestone, Outcome};
use crate::score::{self, Score, Sentence};
use crate::spec::{self, Arrives, Command};
use crate::work::{ANSWER_NEEDED, LAND_NEEDED, PARKED};
use crate::{Problem, Verdict, lock, report, testrun, workspace};
use std::fs;
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
    let score = score::compute(&root, &testrun::run(&root, &hold)?)?;
    drop(hold);
    let milestones = journey::milestones(&root)?;
    let proof = (commands(&root)?, &score);
    print!(
        "{}",
        markdown(&milestones, &outcomes, &proof, &issues, &prs)
    );
    Ok(Verdict::Pass)
}

/// The commands in SPEC.md, and which of its promises are proven.
type Proof<'a> = (Vec<Command>, &'a Score);

fn commands(root: &Path) -> Result<Vec<Command>, Problem> {
    let text = fs::read_to_string(root.join("SPEC.md"))
        .map_err(|e| Problem(format!("couldn't read SPEC.md: {e}")))?;
    Ok(spec::commands(&text))
}

/// Makes the roadmap the body of the open issue with the roadmap's title,
/// unless it already is. With no such issue, there is nowhere to write.
pub fn write(root: &Path, outcomes: &[Outcome], score: &Score) -> Result<(), Problem> {
    let gh = Gh::new(root);
    let issues = gh.roadmap_issues()?;
    let Some(issue) = roadmap_issue(&issues) else {
        return Ok(());
    };
    let milestones = journey::milestones(root)?;
    let proof = (commands(root)?, score);
    let text = markdown(&milestones, outcomes, &proof, &issues, &gh.open_prs()?);
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
    (commands, score): &Proof,
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
        text.push_str(&format!(
            "\n### {}. {}\n\nParent: {}\n\n",
            index + 1,
            shown(&milestone.name),
            parent(&issues, &milestone.name, done)
        ));
        if milestone.commands.is_empty() {
            text.push_str("Commands: none.\n");
        } else {
            text.push_str("Commands:\n\n");
        }
        for name in &milestone.commands {
            text.push_str(&format!("- {}\n", command_line(name, commands, score)));
        }
        text.push_str("\nSteps:\n\n");
        for step in steps {
            let (mark, note) = match step.state {
                "pass" => ("x", ""),
                "fail" => (" ", " (fails)"),
                _ => (" ", " (skipped)"),
            };
            let (id, name) = (step.id, shown(&step.name));
            text.push_str(&format!("- [{mark}] Step {id}: {name}{note}\n"));
        }
    }

    let roadmap = roadmap_issue(all).map(|issue| issue.number);
    let mut later: Vec<String> = commands
        .iter()
        .filter_map(|command| match command.arrives {
            Arrives::V010 => None,
            Arrives::Later => Some(format!("`{}`: later", command.name)),
            Arrives::Proposal => Some(format!("`{}`: a proposal", command.name)),
        })
        .collect();
    let gaps = issues
        .iter()
        .filter(|issue| issue.open && roadmap.is_some() && issue.parent == roadmap)
        .map(|issue| line(issue.number, &issue.title));
    later.extend(gaps);
    section(&mut text, "## After v0.1.0", later);

    // A parked PR is the thing to act on, so it stands for the issues it closes.
    let is_parked = |label: &String| PARKED.contains(&label.as_str());
    let prs: Vec<&OpenPr> = prs
        .iter()
        .filter(|pr| pr.labels.iter().any(is_parked))
        .collect();
    let covered = |issue: &&&RoadmapIssue| {
        let closes = |pr: &&OpenPr| pr.closing_issues.contains(&issue.number);
        prs.iter().any(closes)
    };
    let waiting: Vec<&RoadmapIssue> = issues
        .iter()
        .filter(|issue| issue.open && !covered(issue))
        .copied()
        .collect();
    text.push_str("\n## Waiting on the owner\n");
    let to_merge = parked(&waiting, &prs, LAND_NEEDED);
    section(&mut text, "### To merge", to_merge);
    let to_answer = parked(&waiting, &prs, ANSWER_NEEDED);
    section(&mut text, "### To answer", to_answer);
    let unlabeled = waiting
        .iter()
        .filter(|issue| issue.parent.is_none() && !written(&issue.title))
        .filter(|issue| {
            let waits = |label: &String| label.starts_with("auto:") || is_parked(label);
            !issue.labels.iter().any(waits)
        })
        .map(|issue| line(issue.number, &issue.title))
        .collect();
    section(&mut text, "### To label or close", unlabeled);
    text
}

/// A command's line in its milestone. Its promises are those under its own
/// heading in SPEC.md, and under each heading that its entry says specifies
/// it. It is built when every one is proven.
fn command_line(name: &str, commands: &[Command], score: &Score) -> String {
    let Some(command) = commands.iter().find(|command| command.name == name) else {
        return format!("[ ] `{name}`: not in SPEC.md");
    };
    let counts = |sentence: &&Sentence| {
        sentence.0 == command.name || command.specified_by.contains(&sentence.0)
    };
    let promises = score.promises.iter().filter(counts).count();
    let unproven = score.unproven.iter().filter(counts).count();
    if promises == 0 {
        return format!("[ ] `{name}`: no promises in SPEC.md");
    }
    let mark = if unproven == 0 { "x" } else { " " };
    let proven = promises - unproven;
    format!("[{mark}] `{name}`: proven {proven} of {promises}")
}

/// An issue that `xtask` writes, which the owner neither labels nor closes.
fn written(title: &str) -> bool {
    title == ISSUE_TITLE || title == report::ISSUE_TITLE
}

fn line(number: u64, title: &str) -> String {
    format!("#{number} {}", shown(title))
}

/// A title or a name as GitHub should show it. Outside a code span, `<` would
/// start a tag, and GitHub drops a tag it doesn't know along with its text.
fn shown(text: &str) -> String {
    let parts: Vec<String> = text
        .split('`')
        .enumerate()
        .map(|(index, part)| match index % 2 {
            0 => part.replace('<', "\\<"),
            _ => part.to_string(),
        })
        .collect();
    parts.join("`")
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

/// The parked PRs with this label, then the waiting issues with it.
fn parked(waiting: &[&RoadmapIssue], prs: &[&OpenPr], label: &str) -> Vec<String> {
    let has = |labels: &[String]| labels.iter().any(|name| name == label);
    let mut lines: Vec<(u64, String)> = Vec::new();
    for pr in prs.iter().filter(|pr| has(&pr.labels)) {
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
    for issue in waiting.iter().filter(|issue| has(&issue.labels)) {
        lines.push((issue.number, line(issue.number, &issue.title)));
    }
    lines.sort();
    lines.into_iter().map(|(_, line)| line).collect()
}
