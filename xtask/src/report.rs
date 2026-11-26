//! `cargo xtask report`: the last day on `main`, from the ledger and the open
//! issues.

use crate::bench;
use crate::forge::{Forge, Gh, OpenIssue};
use crate::json::quoted;
use crate::ledger::{self, Record};
use crate::work::PARKED;
use crate::{Problem, Verdict, base, journey, spec, waits, workspace};
use std::collections::HashSet;
use std::path::Path;

/// The report issue's title.
pub const ISSUE_TITLE: &str = "Nightly report";
const HEADING: &str = "# Nightly report for `";
/// How far back to look when no earlier report can say where to start.
const WINDOW: &str = "--since=24.hours";
/// How a report folded its checks before they became one line.
const MUTATION_FAILED: &str = "\n- fail mutation\n";
const SPEC: &str = "SPEC.md";
const RULES: [&str; 2] = ["AGENTS.md", "HARNESS.md"];

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
    /// Its PR: its record's, or the one its subject names when it has none.
    pr: Option<usize>,
    /// Whether it has a record, which was measured on an older `main`.
    late: bool,
}

struct Waiting {
    issue: u64,
    title: String,
}

/// What names a change: its PR, with the PR's title, or its commit when it
/// has no record.
#[derive(Clone, PartialEq)]
enum Source {
    Pr { number: usize, title: String },
    Commit(String),
}

impl Source {
    /// The source in JSON, as `#n` or as its commit in backticks.
    fn label(&self) -> String {
        match self {
            Source::Pr { number, .. } => format!("#{number}"),
            Source::Commit(commit) => format!("`{commit}`"),
        }
    }
}

/// A sentence of SPEC.md, as it was and as it is now. One that was added
/// wasn't, and one that was removed isn't.
struct SpecChange {
    heading: String,
    source: Source,
    was: Option<String>,
    now: Option<String>,
}

/// One change's headings of one file of the rules whose text changed.
struct RuleChange {
    source: Source,
    file: &'static str,
    headings: Vec<String>,
}

pub struct Report {
    commit: String,
    /// The repository's page, which each link starts with.
    url: String,
    scores: Option<Scores>,
    left_out: usize,
    merged: Vec<Merge>,
    regressed: Vec<Regression>,
    unrecorded: Vec<Unrecorded>,
    waiting: Vec<Waiting>,
    spec_changes: Vec<SpecChange>,
    rule_changes: Vec<RuleChange>,
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

/// The `git log` argument that limits it to what no report has covered yet,
/// where a night when the mutation step failed covered nothing.
pub fn mutation_start(root: &Path, gh: &Gh, issues: &[OpenIssue]) -> Result<String, Problem> {
    start_after(root, gh, issues, |report| !mutation_failed(report))
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
        url: String::new(),
        scores,
        left_out: journey::left_out(root)?,
        merged: Vec::new(),
        regressed: Vec::new(),
        unrecorded: Vec::new(),
        waiting: Vec::new(),
        spec_changes: Vec::new(),
        rule_changes: Vec::new(),
    };
    let gh = Gh::new(root);
    let issues = gh.open_issues()?;
    report.url = gh.repo_url()?;
    let start = start_after(root, &gh, &issues, |_| true)?;
    let log = base::git_text(root, &["log", &start, "--format=%H %h %s", "HEAD"])?;
    // Each commit, and what names it: its PR, or the commit when it has no
    // record.
    let mut named: Vec<(String, Source)> = Vec::new();
    for line in log.lines() {
        let mut fields = line.splitn(3, ' ');
        let (Some(full), Some(short), subject) = (fields.next(), fields.next(), fields.next())
        else {
            return Err(Problem(format!("`git log` printed `{line}`")));
        };
        let (commit, subject) = (short.to_string(), subject.unwrap_or_default().to_string());
        let Some(record) = ledger::of_commit(root, full)? else {
            named.push((full.to_string(), Source::Commit(commit.clone())));
            report.unrecorded.push(Unrecorded {
                commit,
                pr: tagged(&subject),
                subject,
                late: false,
            });
            continue;
        };
        let source = Source::Pr {
            number: record.pr,
            title: untagged(&subject, record.pr).to_string(),
        };
        named.push((full.to_string(), source));
        if ledger::merged_after_main_moved(root, full, &record)? {
            report.unrecorded.push(Unrecorded {
                commit,
                subject,
                pr: Some(record.pr),
                late: true,
            });
            continue;
        }
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
        .filter(|issue| {
            issue
                .labels
                .iter()
                .any(|label| PARKED.contains(&label.as_str()))
        })
        .map(|issue| Waiting {
            issue: issue.number,
            title: issue.title,
        })
        .collect();
    // Oldest first, so that each heading lists its changes in order.
    for (commit, source) in named.iter().rev() {
        changes(root, commit, source, &mut report)?;
    }
    Ok(report)
}

/// A squashed commit's subject without the ` (#n)` GitHub adds to the PR's
/// title.
fn untagged(subject: &str, pr: usize) -> &str {
    subject
        .strip_suffix(&format!(" (#{pr})"))
        .unwrap_or(subject)
}

/// The PR a subject names at its end, as a squash names its PR.
fn tagged(subject: &str) -> Option<usize> {
    let number = subject.strip_suffix(')')?.rsplit_once(" (#")?.1;
    match number.bytes().all(|b| b.is_ascii_digit()) {
        true => number.parse().ok(),
        false => None,
    }
}

/// What this commit changed in SPEC.md and in the rules.
fn changes(root: &Path, commit: &str, source: &Source, report: &mut Report) -> Result<(), Problem> {
    let listing = base::git_text(
        root,
        &[
            "diff-tree",
            "--root",
            "--no-commit-id",
            "--name-only",
            "--no-renames",
            "-r",
            "-z",
            commit,
        ],
    )?;
    let touched: Vec<&str> = listing.split('\0').filter(|p| !p.is_empty()).collect();
    let parent = format!("{commit}~1");
    let has_parent = base::commit_exists(root, &parent)?;
    let both = |path: &str| -> Result<(String, String), Problem> {
        let before = match has_parent {
            true => waits::file_at(root, &parent, path)?,
            false => None,
        };
        let after = waits::file_at(root, commit, path)?;
        Ok((before.unwrap_or_default(), after.unwrap_or_default()))
    };
    if touched.contains(&SPEC) {
        let (before, after) = both(SPEC)?;
        // Each heading's changes stay together, in the order it first changed.
        for change in spec_edits(&before, &after, commit, source)? {
            let changes = &mut report.spec_changes;
            let at = match changes.iter().rposition(|c| c.heading == change.heading) {
                Some(last) => last + 1,
                None => changes.len(),
            };
            changes.insert(at, change);
        }
    }
    for file in RULES {
        if !touched.contains(&file) {
            continue;
        }
        let (before, after) = both(file)?;
        let headings = changed_sections(&before, &after);
        if !headings.is_empty() {
            report.rule_changes.push(RuleChange {
                source: source.clone(),
                file,
                headings,
            });
        }
    }
    Ok(())
}

/// The sentences one side has and the other doesn't, under each heading, marked
/// or not. Under
/// one heading, the first removed and the first added are one change, and so
/// on, and what is left over was removed or added. A sentence ignores
/// whitespace, so a reflowed paragraph changes none.
fn spec_edits(
    before: &str,
    after: &str,
    commit: &str,
    source: &Source,
) -> Result<Vec<SpecChange>, Problem> {
    let units = |text: &str| {
        spec::extract_all(text).map_err(|e| Problem(format!("SPEC.md at {commit}: {e}")))
    };
    let (before, after) = (units(before)?, units(after)?);
    let only = |these: &[spec::Unit], those: &[spec::Unit]| -> Vec<spec::Unit> {
        let others: HashSet<(&str, &str)> = those
            .iter()
            .map(|unit| (unit.heading.as_str(), unit.text.as_str()))
            .collect();
        these
            .iter()
            .filter(|unit| !others.contains(&(unit.heading.as_str(), unit.text.as_str())))
            .cloned()
            .collect()
    };
    let (removed, added) = (only(&before, &after), only(&after, &before));
    let mut headings: Vec<&str> = Vec::new();
    for unit in removed.iter().chain(&added) {
        if !headings.contains(&unit.heading.as_str()) {
            headings.push(&unit.heading);
        }
    }
    let mut edits = Vec::new();
    for heading in headings {
        let under = |units: &[spec::Unit]| -> Vec<String> {
            units
                .iter()
                .filter(|unit| unit.heading == heading)
                .map(|unit| unit.text.clone())
                .collect()
        };
        let (mut was, mut now) = (under(&removed).into_iter(), under(&added).into_iter());
        loop {
            match (was.next(), now.next()) {
                (None, None) => break,
                (was, now) => edits.push(SpecChange {
                    heading: heading.to_string(),
                    source: source.clone(),
                    was,
                    now,
                }),
            }
        }
    }
    Ok(edits)
}

/// Each heading whose text differs between the two sides, in the order of the
/// newer side, and then the ones it removed. Text before the first heading
/// belongs to none, and whitespace doesn't count.
fn changed_sections(before: &str, after: &str) -> Vec<String> {
    let (before, after) = (sections(before), sections(after));
    let text = |all: &[(String, String)], heading: &str| -> Option<String> {
        let parts: Vec<&str> = all
            .iter()
            .filter(|(title, _)| title == heading)
            .map(|(_, text)| text.as_str())
            .collect();
        (!parts.is_empty()).then(|| parts.join("\n"))
    };
    let mut changed: Vec<String> = Vec::new();
    for (heading, _) in after.iter().chain(&before) {
        if !changed.contains(heading) && text(&before, heading) != text(&after, heading) {
            changed.push(heading.clone());
        }
    }
    changed
}

/// Each heading, with its text to the next heading of any depth, and the text
/// before the first heading, if any, under no heading.
fn sections(text: &str) -> Vec<(String, String)> {
    let lines: Vec<&str> = text.lines().collect();
    let fenced = spec::fenced(&lines);
    let mut headings = spec::headings_of(&lines, &fenced).into_iter().peekable();
    let mut found: Vec<(String, Vec<&str>)> = vec![(String::new(), Vec::new())];
    for (index, line) in lines.iter().enumerate() {
        match headings.next_if(|(_, _, at)| *at == index) {
            Some((_, title, _)) => found.push((title.to_string(), Vec::new())),
            None => found.last_mut().expect("one is always open").1.push(line),
        }
    }
    let words = |lines: Vec<&str>| {
        lines
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    found
        .into_iter()
        .map(|(title, lines)| (title, words(lines)))
        .filter(|(title, text)| !title.is_empty() || !text.is_empty())
        .collect()
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

/// The items of each source, in the order each source first comes.
fn by_source<'a, T>(
    items: impl Iterator<Item = &'a T>,
    source: fn(&T) -> &Source,
) -> Vec<(&'a Source, Vec<&'a T>)> {
    let mut groups: Vec<(&Source, Vec<&T>)> = Vec::new();
    for item in items {
        match groups.iter_mut().find(|(s, _)| *s == source(item)) {
            Some((_, group)) => group.push(item),
            None => groups.push((source(item), vec![item])),
        }
    }
    groups
}

/// The PR a source names, if it names one.
fn number(source: &Source) -> Option<usize> {
    match source {
        Source::Pr { number, .. } => Some(*number),
        Source::Commit(_) => None,
    }
}

/// An open section, left out when it has nothing in it.
fn section(text: &mut String, heading: &str, lines: Vec<String>) {
    if lines.is_empty() {
        return;
    }
    text.push_str(&format!("\n## {heading}\n\n"));
    for line in lines {
        text.push_str(&format!("- {line}\n"));
    }
}

/// A folded section, left out when it has nothing in it. GitHub renders the
/// list inside only with a blank line on each side of it.
fn fold(text: &mut String, summary: &str, lines: Vec<String>) {
    if lines.is_empty() {
        return;
    }
    text.push_str(&format!("\n<details>\n<summary>{summary}</summary>\n\n"));
    for line in lines {
        text.push_str(&format!("- {line}\n"));
    }
    text.push_str("\n</details>\n");
}

/// What the night's checks came to, or nothing when there were none.
fn verdict(checks: &[(&str, &str)]) -> String {
    let failed: Vec<&str> = checks
        .iter()
        .filter(|(_, state)| *state == "fail")
        .map(|(name, _)| *name)
        .collect();
    match (checks.is_empty(), failed.is_empty()) {
        (true, _) => String::new(),
        (false, true) => "**All checks passed.** ".to_string(),
        (false, false) => format!("**Failed: {}.** ", failed.join(", ")),
    }
}

/// The checks in one line, such as `Checks: 4 passed, 1 failed (`gate`)`,
/// which names only the checks that didn't pass.
fn tally(checks: &[(&str, &str)]) -> String {
    let counts: Vec<String> = [("pass", "passed"), ("fail", "failed"), ("skip", "skipped")]
        .iter()
        .filter_map(|(state, word)| {
            let names: Vec<String> = checks
                .iter()
                .filter(|(_, s)| s == state)
                .map(|(name, _)| format!("`{name}`"))
                .collect();
            match (names.len(), *state) {
                (0, _) => None,
                (n, "pass") => Some(format!("{n} {word}")),
                (n, _) => Some(format!("{n} {word} ({})", names.join(", "))),
            }
        })
        .collect();
    format!("Checks: {}", counts.join(", "))
}

/// Whether this posted report failed its mutation check, in its checks line
/// or in the fold of checks it had before.
fn mutation_failed(report: &str) -> bool {
    report.contains(MUTATION_FAILED)
        || report.lines().any(|line| {
            line.strip_prefix("Checks: ")
                .and_then(|counts| counts.split_once(" failed (")?.1.split_once(')'))
                .is_some_and(|(names, _)| names.split(", ").any(|name| name == "`mutation`"))
        })
}

impl Report {
    /// How many steps that spend were left out of the run, for a caller that
    /// ran the journey with them.
    pub fn with_left_out(self, left_out: usize) -> Report {
        Report { left_out, ..self }
    }

    /// The nightly job passes its checks. A report on its own has none. What
    /// needs the owner stays open, and the rest folds.
    pub fn markdown(&self, checks: &[(&str, &str)]) -> String {
        let mut text = format!("# Nightly report for `{}`\n\n", self.commit);
        text.push_str(&verdict(checks));
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
        let regressed = self
            .regressed
            .iter()
            .map(|r| format!("`{}` {}", r.commit, r.what))
            .collect();
        section(&mut text, "Regressed", regressed);
        let unrecorded = self
            .unrecorded
            .iter()
            .map(|u| {
                let subject = match u.pr {
                    Some(pr) => self.link(untagged(&u.subject, pr), "pull", pr),
                    None => u.subject.clone(),
                };
                match u.late {
                    true => format!("`{}` {subject}, merged after `main` moved", u.commit),
                    false => format!("`{}` {subject}", u.commit),
                }
            })
            .collect();
        section(&mut text, "Not in the ledger", unrecorded);
        let waiting = self
            .waiting
            .iter()
            .map(|w| self.link(&w.title, "issues", w.issue))
            .collect();
        section(&mut text, "Waiting on the owner", waiting);
        let mut above = Vec::new();
        self.spec_section(&mut text, &mut above);
        self.rules_section(&mut text, &mut above);
        let rest: Vec<String> = self
            .merged
            .iter()
            .filter(|m| !above.contains(&m.pr))
            .map(|m| self.link(untagged(&m.subject, m.pr), "pull", m.pr))
            .collect();
        let summary = match self.merged.len() - rest.len() {
            0 => format!("Merged: {}", self.merged.len()),
            named => format!("Merged: {}, {named} of them above", self.merged.len()),
        };
        match (self.merged.is_empty(), rest.is_empty()) {
            (true, _) => {}
            (false, true) => text.push_str(&format!("\n{summary}\n")),
            (false, false) => fold(&mut text, &summary, rest),
        }
        if !checks.is_empty() {
            text.push_str(&format!("\n{}\n", tally(checks)));
        }
        text
    }

    /// A link to an issue or a PR, whose text is its title, so that a reader
    /// sees the title once.
    fn link(&self, title: &str, kind: &str, number: impl std::fmt::Display) -> String {
        format!("[{title}]({}/{kind}/{number})", self.url)
    }

    /// A PR as a link, or a commit with no record.
    fn name(&self, source: &Source) -> String {
        match source {
            Source::Pr { number, title } => self.link(title, "pull", *number),
            Source::Commit(commit) => format!("`{commit}`"),
        }
    }

    /// Each changed sentence of SPEC.md, under its heading and then its
    /// change. A sentence with no mark was added. Each PR it names goes in
    /// `named`.
    fn spec_section(&self, text: &mut String, named: &mut Vec<usize>) {
        if self.spec_changes.is_empty() {
            return;
        }
        text.push_str(&format!("\n## {SPEC}\n"));
        let mut headings: Vec<&str> = Vec::new();
        for change in &self.spec_changes {
            if !headings.contains(&change.heading.as_str()) {
                headings.push(&change.heading);
            }
        }
        for heading in headings {
            text.push_str(&format!("\n### {heading}\n\n"));
            let under = self.spec_changes.iter().filter(|c| c.heading == heading);
            for (source, changes) in by_source(under, |c| &c.source) {
                text.push_str(&format!("- {}\n", self.name(source)));
                named.extend(number(source));
                for change in changes {
                    text.push_str(&match (&change.was, &change.now) {
                        (Some(was), Some(now)) => {
                            format!("  - Changed:\n    - Was: {was}\n    - Now: {now}\n")
                        }
                        (Some(was), None) => format!("  - Removed: {was}\n"),
                        (None, Some(now)) => format!("  - {now}\n"),
                        (None, None) => String::new(),
                    });
                }
            }
        }
    }

    /// Each change to the rules, with a line for each file it changed that
    /// names the headings whose text changed. Each PR it names goes in
    /// `named`.
    fn rules_section(&self, text: &mut String, named: &mut Vec<usize>) {
        let mut lines = Vec::new();
        for (source, changes) in by_source(self.rule_changes.iter(), |c| &c.source) {
            named.extend(number(source));
            let mut line = self.name(source);
            for change in changes {
                let headings: Vec<&str> = change
                    .headings
                    .iter()
                    .map(|heading| match heading.as_str() {
                        "" => "before any heading",
                        heading => heading,
                    })
                    .collect();
                line.push_str(&format!("\n  - `{}`: {}", change.file, headings.join(", ")));
            }
            lines.push(line);
        }
        section(text, "Rules", lines);
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
                let subject = match u.late {
                    true => format!("{}, merged after `main` moved", u.subject),
                    false => u.subject.clone(),
                };
                format!(
                    "{{\"commit\":{},\"subject\":{}}}",
                    quoted(&u.commit),
                    quoted(&subject)
                )
            })
            .collect();
        let waiting: Vec<String> = self
            .waiting
            .iter()
            .map(|w| format!("{{\"issue\":{},\"title\":{}}}", w.issue, quoted(&w.title)))
            .collect();
        let maybe = |text: &Option<String>| text.as_deref().map_or("null".to_string(), quoted);
        let spec_changes: Vec<String> = self
            .spec_changes
            .iter()
            .map(|c| {
                format!(
                    "{{\"heading\":{},\"pr\":{},\"was\":{},\"now\":{}}}",
                    quoted(&c.heading),
                    quoted(&c.source.label()),
                    maybe(&c.was),
                    maybe(&c.now)
                )
            })
            .collect();
        let rule_changes: Vec<String> = self
            .rule_changes
            .iter()
            .map(|r| {
                let headings: Vec<String> = r.headings.iter().map(|h| quoted(h)).collect();
                format!(
                    "{{\"pr\":{},\"file\":{},\"headings\":[{}]}}",
                    quoted(&r.source.label()),
                    quoted(r.file),
                    headings.join(",")
                )
            })
            .collect();
        format!(
            "{{\"commit\":{},\"scores\":{scores},\"left_out\":{},\"merged\":[{}],\"regressed\":[{}],\"unrecorded\":[{}],\"waiting\":[{}],\"spec_changes\":[{}],\"rule_changes\":[{}]}}",
            quoted(&self.commit),
            self.left_out,
            merged.join(","),
            regressed.join(","),
            unrecorded.join(","),
            waiting.join(","),
            spec_changes.join(","),
            rule_changes.join(",")
        )
    }
}
