//! `cargo xtask roadmap`: the milestones and what comes after them, in words
//! that anyone can read. `--details` prints the owner's view: each milestone's
//! parent, steps, and proof, and what waits on the owner. Nobody types either
//! one: every line comes from the journey file, the journey's results, the
//! README, SPEC.md and the checks that prove it, or GitHub.

use crate::forge::{Gh, OpenPr, RoadmapIssue};
use crate::journey::{self, Milestone, Outcome};
use crate::readme::{self, Line, README_FILE};
use crate::score::{self, Score, Sentence};
use crate::spec::{self, Arrives, Command};
use crate::work::{self, ANSWER_NEEDED, LAND_NEEDED, PARKED};
use crate::{Problem, Verdict, base, lock, report, testrun, workspace};
use std::fs;
use std::path::Path;

/// The roadmap issue's title. The lowest open issue with it is the one.
const ISSUE_TITLE: &str = "Roadmap";
/// The title of the issue that holds the details, found the same way.
const DETAILS_TITLE: &str = "Roadmap details";
const HEADER: &str = "This page shows what works in Agent+ today, and what comes next. The nightly job writes it from the project's checks, so don't edit it.\n";
const DETAILS_HEADER: &str = "The nightly job writes this issue from `cargo xtask roadmap --details`, so don't edit it. To change it, change `harness/journey.toml` or the issues it lists.\n";
/// The details list this many of the Rust files that the most commits changed.
const BUSIEST: usize = 10;
/// And then every other Rust file with at least this many lines.
const LARGE: usize = 2000;
/// The time before HEAD in which a commit counts toward a file, in seconds.
const WEEK: u64 = 7 * 24 * 3600;

/// Prints the roadmap, or its details, and writes nothing.
pub fn run(details: bool) -> Result<Verdict, Problem> {
    let root = workspace::root()?;
    // GitHub comes first, so that a run that can't reach it stops early.
    let gh = Gh::new(&root);
    let (issues, prs) = (gh.roadmap_issues("all")?, gh.open_prs()?);
    let mut hold = match details {
        true => lock::testing(&root)?,
        false => lock::shared(&root)?,
    };
    let outcomes = journey::outcomes(&root, &journey::Keys::new())?;
    // Only the details count proven promises, which takes a run of the tests.
    let score = match details {
        true => Some(score::compute(&root, &testrun::run(&root, &mut hold)?)?),
        false => None,
    };
    drop(hold);
    let milestones = journey::milestones(&root)?;
    let commands = commands(&root)?;
    let text = match &score {
        Some(score) => {
            let proof = (commands.as_slice(), score);
            let files = rust_files(&root)?;
            self::details(&milestones, &outcomes, &proof, &issues, &prs, &files)
        }
        None => {
            let details = titled(&issues, DETAILS_TITLE).map(|issue| issue.number);
            let words = (commands.as_slice(), lines(&root));
            roadmap(&milestones, &outcomes, &words, &issues, details)
        }
    };
    print!("{text}");
    Ok(Verdict::Pass)
}

/// The commands in SPEC.md, and which of its promises are proven.
type Proof<'a> = (&'a [Command], &'a Score);

/// The commands in SPEC.md, and the README's line for each.
type Words<'a> = (&'a [Command], Vec<Line>);

fn commands(root: &Path) -> Result<Vec<Command>, Problem> {
    let text = fs::read_to_string(root.join("SPEC.md"))
        .map_err(|e| Problem(format!("couldn't read SPEC.md: {e}")))?;
    Ok(spec::commands(&text))
}

/// A workspace with no README describes no command, and the gate's `spec`
/// check is what says so.
fn lines(root: &Path) -> Vec<Line> {
    let text = fs::read_to_string(root.join(README_FILE)).unwrap_or_default();
    readme::command_lines(&text)
}

/// Makes the roadmap the body of the open issue with the roadmap's title, and
/// the details the body of the open issue with theirs, which it files when
/// there is none. A body that already says it is left alone. With no roadmap
/// issue, there is nowhere to write.
pub fn write(root: &Path, outcomes: &[Outcome], score: &Score) -> Result<(), Problem> {
    let gh = Gh::new(root);
    let issues = gh.roadmap_issues("all")?;
    let Some(issue) = titled(&issues, ISSUE_TITLE) else {
        return Ok(());
    };
    let milestones = journey::milestones(root)?;
    let commands = commands(root)?;
    let proof = (commands.as_slice(), score);
    let files = rust_files(root)?;
    let text = details(
        &milestones,
        outcomes,
        &proof,
        &issues,
        &gh.open_prs()?,
        &files,
    );
    // The roadmap links to the details, so they are written first.
    let number = match titled(&issues, DETAILS_TITLE) {
        Some(found) => {
            put(&gh, found, &text)?;
            found.number
        }
        None => gh.create_issue(DETAILS_TITLE, &text)?,
    };
    let words = (commands.as_slice(), lines(root));
    let text = roadmap(&milestones, outcomes, &words, &issues, Some(number));
    put(&gh, issue, &text)
}

fn put(gh: &Gh, issue: &RoadmapIssue, text: &str) -> Result<(), Problem> {
    // An edit on the website saves its line ends as `\r\n`.
    if issue.body.replace("\r\n", "\n").trim_end() != text.trim_end() {
        gh.edit_issue(issue.number, text)?;
    }
    Ok(())
}

fn titled<'a>(issues: &'a [RoadmapIssue], title: &str) -> Option<&'a RoadmapIssue> {
    let titled = |issue: &&RoadmapIssue| issue.open && issue.title == title;
    issues
        .iter()
        .filter(titled)
        .min_by_key(|issue| issue.number)
}

/// Whether every step of this milestone passes.
fn is_done(outcomes: &[Outcome], milestone: &Milestone) -> bool {
    let mut steps = outcomes
        .iter()
        .filter(|step| step.milestone == milestone.name);
    steps.all(|step| step.state == "pass")
}

/// The roadmap for anyone: what each milestone and command is for, and how
/// far it is. A milestone is done when every one of its steps passes, the
/// first one that isn't done is in progress, and a command is ticked when its
/// milestone is done. The same inputs give the same text.
fn roadmap(
    milestones: &[Milestone],
    outcomes: &[Outcome],
    (commands, lines): &Words,
    all: &[RoadmapIssue],
    details: Option<u64>,
) -> String {
    let done: Vec<bool> = milestones
        .iter()
        .map(|milestone| is_done(outcomes, milestone))
        .collect();
    let current = done.iter().position(|done| !done);
    let status = |index: usize| match done[index] {
        true => "Done",
        false if current == Some(index) => "In progress",
        false => "Planned",
    };
    let mut text = format!(
        "{HEADER}\n## First release (v0.1.0)\n\nMilestones done: {} of {}.\n\n| | Milestone | What you can do | Status |\n|---|---|---|---|\n",
        done.iter().filter(|done| **done).count(),
        milestones.len()
    );
    for (index, milestone) in milestones.iter().enumerate() {
        text.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            index + 1,
            cell(&milestone.name),
            cell(&milestone.summary),
            status(index)
        ));
    }

    text.push_str("\n## Commands\n");
    for (index, milestone) in milestones.iter().enumerate() {
        if milestone.commands.is_empty() {
            continue;
        }
        text.push_str(&format!(
            "\n### {}. {} ({})\n\n",
            index + 1,
            shown(&milestone.name),
            status(index).to_lowercase()
        ));
        let mark = if done[index] { "x" } else { " " };
        for name in &milestone.commands {
            let about = readme::line_for(lines, name)
                .map_or(String::new(), |line| format!(": {}", shown(&line.about)));
            text.push_str(&format!("- [{mark}] `{name}`{about}\n"));
        }
    }

    // One item in the README can describe two commands, and is shown once.
    let mut later: Vec<String> = Vec::new();
    for command in commands.iter().filter(|c| c.arrives != Arrives::V010) {
        let line = match readme::line_for(lines, &command.name) {
            Some(line) => shown(&line.text),
            None => format!("`{}`", command.name),
        };
        if !later.contains(&line) {
            later.push(line);
        }
    }
    later.extend(gaps(all));
    section(&mut text, "## After the first release", later);
    if let Some(number) = details {
        text.push_str(&format!(
            "\nFor each milestone's issue and checks, and for what waits on the owner, see #{number}.\n"
        ));
    }
    text
}

/// The open sub-issues of the roadmap issue, lowest number first.
fn gaps(all: &[RoadmapIssue]) -> Vec<String> {
    let roadmap = titled(all, ISSUE_TITLE).map(|issue| issue.number);
    let mut gaps: Vec<&RoadmapIssue> = all
        .iter()
        .filter(|issue| issue.open && roadmap.is_some() && issue.parent == roadmap)
        .collect();
    gaps.sort_by_key(|issue| issue.number);
    gaps.iter()
        .map(|issue| line(issue.number, &issue.title))
        .collect()
}

/// The details for the owner and the agents. The same inputs give the same
/// text, whatever order GitHub listed them in.
fn details(
    milestones: &[Milestone],
    outcomes: &[Outcome],
    (commands, score): &Proof,
    all: &[RoadmapIssue],
    prs: &[OpenPr],
    files: &[RustFile],
) -> String {
    let mut issues: Vec<&RoadmapIssue> = all.iter().collect();
    issues.sort_by_key(|issue| issue.number);
    let (passing, steps) = journey::tally(outcomes);
    let mut text = format!("{DETAILS_HEADER}\n## v0.1.0\n\njourney {passing} of {steps}\n");
    for (index, milestone) in milestones.iter().enumerate() {
        let in_it = |step: &&Outcome| step.milestone == milestone.name;
        let steps: Vec<&Outcome> = outcomes.iter().filter(in_it).collect();
        let done = is_done(outcomes, milestone);
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
            text.push_str(&step_line(step.id, &step.name, step.state));
        }
    }

    let mut later: Vec<String> = commands
        .iter()
        .filter_map(|command| match command.arrives {
            Arrives::V010 => None,
            Arrives::Later => Some(format!("`{}`: later", command.name)),
            Arrives::Proposal => Some(format!("`{}`: a proposal", command.name)),
        })
        .collect();
    later.extend(gaps(all));
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
    text.push_str(&busy(files, &issues));
    text
}

/// A Rust file at HEAD, how many commits on `main` changed it in the week
/// before HEAD, and how many lines it has.
struct RustFile {
    path: String,
    commits: usize,
    lines: usize,
}

/// The Rust files at HEAD. A workspace with no commit has none.
fn rust_files(root: &Path) -> Result<Vec<RustFile>, Problem> {
    if !base::commit_exists(root, "HEAD")? {
        return Ok(Vec::new());
    }
    let git =
        |args: &[&str]| base::git_text(root, &[&["-c", "core.quotePath=false"], args].concat());
    let time = git(&["log", "-1", "--format=%ct", "HEAD"])?;
    let time: u64 = time
        .trim()
        .parse()
        .map_err(|e| Problem(format!("couldn't read HEAD's commit time {time:?}: {e}")))?;
    let since = format!("--since=@{}", time.saturating_sub(WEEK));
    let changed = git(&[
        "log",
        "--first-parent",
        &since,
        "--format=",
        "--name-only",
        "HEAD",
        "--",
        "*.rs",
    ])?;
    // `git grep` lists no empty file, and exits 1 when it lists none.
    let args = [
        "-c",
        "core.quotePath=false",
        "grep",
        "-c",
        "-e",
        "",
        "HEAD",
        "--",
        "*.rs",
    ];
    let counted = base::git(root, &args)?;
    if !matches!(counted.status.code(), Some(0 | 1)) {
        return Err(Problem(format!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&counted.stderr).trim()
        )));
    }
    let counted = String::from_utf8_lossy(&counted.stdout);
    let counted: Vec<(&str, usize)> = counted
        .lines()
        .filter_map(|line| line.strip_prefix("HEAD:")?.rsplit_once(':'))
        .filter_map(|(path, count)| Some((path, count.parse().ok()?)))
        .collect();
    let lines = |path: &str| {
        let found = counted.iter().find(|(found, _)| *found == path);
        found.map_or(0, |(_, count)| *count)
    };
    let files = git(&["ls-tree", "-r", "--name-only", "HEAD"])?;
    Ok(files
        .lines()
        .filter(|path| path.ends_with(".rs"))
        .map(|path| RustFile {
            path: path.to_string(),
            commits: changed.lines().filter(|line| *line == path).count(),
            lines: lines(path),
        })
        .collect())
}

/// The busiest Rust files, and then the other large ones, with the open
/// issues whose `Touches` overlap each, by the rule the queue uses.
fn busy(files: &[RustFile], issues: &[&RoadmapIssue]) -> String {
    let mut rows: Vec<&RustFile> = files.iter().filter(|file| file.commits > 0).collect();
    rows.sort_by(|a, b| b.commits.cmp(&a.commits).then_with(|| a.path.cmp(&b.path)));
    rows.truncate(BUSIEST);
    let mut large: Vec<&RustFile> = files
        .iter()
        .filter(|file| file.lines >= LARGE && !rows.iter().any(|row| row.path == file.path))
        .collect();
    large.sort_by(|a, b| b.lines.cmp(&a.lines).then_with(|| a.path.cmp(&b.path)));
    rows.extend(large);
    let mut text = "\n## Busy and large files\n\n".to_string();
    if rows.is_empty() {
        text.push_str("Nothing.\n");
        return text;
    }
    let touches: Vec<Vec<String>> = issues
        .iter()
        .filter(|issue| issue.open)
        .map(|issue| work::touches(&issue.body))
        .collect();
    text.push_str(
        "| File | PRs in 7 days | Lines | Open issues that name it |\n|---|---|---|---|\n",
    );
    for row in rows {
        let names =
            |entries: &&Vec<String>| entries.iter().any(|entry| work::overlap(entry, &row.path));
        let named = touches.iter().filter(names).count();
        let path = cell(&format!("`{}`", row.path));
        text.push_str(&format!(
            "| {path} | {} | {} | {named} |\n",
            row.commits, row.lines
        ));
    }
    text
}

/// A step's line in its milestone.
fn step_line(id: i64, name: &str, state: &str) -> String {
    let (mark, note) = match state {
        "pass" => ("x", ""),
        "fail" => (" ", " (fails)"),
        _ => (" ", " (skipped)"),
    };
    format!("- [{mark}] Step {id}: {}{note}\n", shown(name))
}

/// The body of the details that the nightly job wrote last, or nothing when
/// there are none.
pub fn last_details(root: &Path) -> Result<String, Problem> {
    let issues = Gh::new(root).roadmap_issues("open")?;
    Ok(titled(&issues, DETAILS_TITLE).map_or_else(String::new, |issue| issue.body.clone()))
}

/// Whether these details show the step passing, under its id and its name, so
/// a step renamed or renumbered since they were written hasn't passed.
pub fn passed_in(details: &str, step: &Outcome) -> bool {
    let line = step_line(step.id, &step.name, "pass");
    details.lines().any(|found| found == line.trim_end())
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
    title == ISSUE_TITLE || title == DETAILS_TITLE || title == report::ISSUE_TITLE
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

/// The same in a table, where `|` would end the cell.
fn cell(text: &str) -> String {
    shown(text).replace('|', "\\|")
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
