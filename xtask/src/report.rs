//! `cargo xtask report`: the last day on `main`, from the ledger and the open
//! issues.

use crate::bench;
use crate::forge::{Comment, Forge, Gh, OpenIssue};
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
const MUTATION_FAILED: &str = "\n- fail mutation\n";
/// The last line of a posted report with a change to SPEC.md or the rules.
/// It also says, to anyone reading the comment later, that this one is read.
pub const MARK: &str = "Mark this report read by reacting with :eyes:.";
/// The reaction that marks a report read.
const READ: &str = "eyes";
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
}

struct Waiting {
    issue: u64,
    title: String,
}

/// A sentence of SPEC.md, as it was and as it is now. One that was added
/// wasn't, and one that was removed isn't.
struct SpecChange {
    heading: String,
    pr: String,
    was: Option<String>,
    now: Option<String>,
}

/// A heading of the rules whose text changed, and each change that did it.
struct RuleChange {
    file: &'static str,
    heading: String,
    prs: Vec<String>,
}

/// A posted report that asks to be read, and that the owner hasn't marked.
pub struct Unread {
    pub commit: String,
    pub url: String,
}

pub struct Report {
    commit: String,
    scores: Option<Scores>,
    left_out: usize,
    merged: Vec<Merge>,
    regressed: Vec<Regression>,
    unrecorded: Vec<Unrecorded>,
    waiting: Vec<Waiting>,
    spec_changes: Vec<SpecChange>,
    rule_changes: Vec<RuleChange>,
    unread: Vec<Unread>,
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
    start_after(root, gh, issues, |report| !report.contains(MUTATION_FAILED))
}

fn start_after(
    root: &Path,
    gh: &Gh,
    issues: &[OpenIssue],
    counts: fn(&str) -> bool,
) -> Result<String, Problem> {
    let number = issues
        .iter()
        .find(|issue| issue.title == ISSUE_TITLE)
        .map(|issue| issue.number);
    start_from(root, &reports(gh, number)?, counts)
}

/// The `git log` argument that limits it to what no report has covered yet.
fn start_from(
    root: &Path,
    reports: &Option<Reports>,
    counts: fn(&str) -> bool,
) -> Result<String, Problem> {
    Ok(match last_reported(root, reports, counts)? {
        Some(commit) => format!("^{commit}"),
        None => WINDOW.to_string(),
    })
}

/// The report issue's comments, and the owner, whose comments are the reports.
struct Reports {
    comments: Vec<Comment>,
    owner: String,
}

/// Nothing without a report issue.
fn reports(gh: &Gh, number: Option<u64>) -> Result<Option<Reports>, Problem> {
    let Some(number) = number else {
        return Ok(None);
    };
    Ok(Some(Reports {
        comments: gh.issue(number)?.comments,
        owner: gh.owner()?,
    }))
}

/// The reports on this issue that ask to be read, and that have no :eyes:
/// from the owner's account. A comment is only data, so only the owner's
/// count, and so does only the owner's reaction.
pub fn unread_on(gh: &Gh, number: Option<u64>) -> Result<Vec<Unread>, Problem> {
    unread(gh, &reports(gh, number)?)
}

fn unread(gh: &Gh, reports: &Option<Reports>) -> Result<Vec<Unread>, Problem> {
    let Some(reports) = reports else {
        return Ok(Vec::new());
    };
    let mut found = Vec::new();
    for comment in &reports.comments {
        let asks = comment.body.lines().any(|line| line == MARK);
        let Some(commit) = comment.body.strip_prefix(HEADING) else {
            continue;
        };
        if comment.author != reports.owner
            || !asks
            || gh.reacted(comment.id, READ)?.contains(&reports.owner)
        {
            continue;
        }
        found.push(Unread {
            commit: commit.split('`').next().unwrap_or_default().to_string(),
            url: comment.url.clone(),
        });
    }
    Ok(found)
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
        spec_changes: Vec::new(),
        rule_changes: Vec::new(),
        unread: Vec::new(),
    };
    let gh = Gh::new(root);
    let issues = gh.open_issues()?;
    let number = issues
        .iter()
        .find(|issue| issue.title == ISSUE_TITLE)
        .map(|issue| issue.number);
    let reports = reports(&gh, number)?;
    let start = start_from(root, &reports, |_| true)?;
    report.unread = unread(&gh, &reports)?;
    let log = base::git_text(root, &["log", &start, "--format=%H %h %s", "HEAD"])?;
    // Each commit, and what names it: its PR, or the commit when it has no
    // record.
    let mut named: Vec<(String, String)> = Vec::new();
    for line in log.lines() {
        let mut fields = line.splitn(3, ' ');
        let (Some(full), Some(short), subject) = (fields.next(), fields.next(), fields.next())
        else {
            return Err(Problem(format!("`git log` printed `{line}`")));
        };
        let (commit, subject) = (short.to_string(), subject.unwrap_or_default().to_string());
        let Some(record) = ledger::of_commit(root, full)? else {
            named.push((full.to_string(), format!("`{commit}`")));
            report.unrecorded.push(Unrecorded { commit, subject });
            continue;
        };
        named.push((full.to_string(), format!("#{}", record.pr)));
        if ledger::merged_after_main_moved(root, full, &record)? {
            let subject = format!("{subject}, merged after `main` moved");
            report.unrecorded.push(Unrecorded { commit, subject });
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
    for (commit, pr) in named.iter().rev() {
        changes(root, commit, pr, &mut report)?;
    }
    Ok(report)
}

/// What this commit changed in SPEC.md and in the rules.
fn changes(root: &Path, commit: &str, pr: &str, report: &mut Report) -> Result<(), Problem> {
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
        for change in spec_edits(&before, &after, commit, pr)? {
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
        for heading in changed_sections(&before, &after) {
            let same = |change: &&mut RuleChange| change.file == file && change.heading == heading;
            match report.rule_changes.iter_mut().find(same) {
                Some(change) => change.prs.push(pr.to_string()),
                None => report.rule_changes.push(RuleChange {
                    file,
                    heading,
                    prs: vec![pr.to_string()],
                }),
            }
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
    pr: &str,
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
                    pr: pr.to_string(),
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
    reports: &Option<Reports>,
    counts: fn(&str) -> bool,
) -> Result<Option<String>, Problem> {
    let Some(Reports { comments, owner }) = reports else {
        return Ok(None);
    };
    let newest = comments
        .iter()
        .rev()
        .filter(|comment| &comment.author == owner && counts(&comment.body))
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

/// The checks fold's summary, such as `Checks: 4 passed, 1 failed`.
fn tally(checks: &[(&str, &str)]) -> String {
    let counts: Vec<String> = [("pass", "passed"), ("fail", "failed"), ("skip", "skipped")]
        .iter()
        .filter_map(|(state, word)| {
            let n = checks.iter().filter(|(_, s)| s == state).count();
            (n > 0).then(|| format!("{n} {word}"))
        })
        .collect();
    format!("Checks: {}", counts.join(", "))
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
            .map(|u| format!("`{}` {}", u.commit, u.subject))
            .collect();
        section(&mut text, "Not in the ledger", unrecorded);
        let waiting = self
            .waiting
            .iter()
            .map(|w| format!("#{} {}", w.issue, w.title))
            .collect();
        section(&mut text, "Waiting on the owner", waiting);
        self.spec_section(&mut text);
        let rules = self
            .rule_changes
            .iter()
            .map(|r| match r.heading.as_str() {
                "" => format!("`{}`: {}", r.file, r.prs.join(", ")),
                heading => format!("`{}`, {heading}: {}", r.file, r.prs.join(", ")),
            })
            .collect();
        section(&mut text, "Rules", rules);
        let unread = self
            .unread
            .iter()
            .map(|u| format!("[Nightly report for `{}`]({})", u.commit, u.url))
            .collect();
        section(&mut text, "Unread reports", unread);
        let merged = self
            .merged
            .iter()
            .map(|m| format!("`{}` {}, issue #{}", m.commit, m.subject, m.issue))
            .collect();
        fold(&mut text, &format!("Merged: {}", self.merged.len()), merged);
        let lines = checks
            .iter()
            .map(|(name, state)| format!("{state} {name}"))
            .collect();
        fold(&mut text, &tally(checks), lines);
        text
    }

    /// The report as the nightly job posts it. One with a change to SPEC.md
    /// or the rules ends by asking the owner to mark it read.
    pub fn comment(&self, checks: &[(&str, &str)]) -> String {
        let mut text = self.markdown(checks);
        if !self.spec_changes.is_empty() || !self.rule_changes.is_empty() {
            text.push_str(&format!("\n{MARK}\n"));
        }
        text
    }

    /// Each changed sentence of SPEC.md, under its heading.
    fn spec_section(&self, text: &mut String) {
        if self.spec_changes.is_empty() {
            return;
        }
        text.push_str(&format!("\n## {SPEC}\n"));
        let mut heading = None;
        for change in &self.spec_changes {
            if heading != Some(&change.heading) {
                heading = Some(&change.heading);
                text.push_str(&format!("\n### {}\n\n", change.heading));
            }
            let pr = &change.pr;
            text.push_str(&match (&change.was, &change.now) {
                (Some(was), Some(now)) => {
                    format!("- Changed in {pr}:\n  - Was: {was}\n  - Now: {now}\n")
                }
                (Some(was), None) => format!("- Removed in {pr}: {was}\n"),
                (None, Some(now)) => format!("- Added in {pr}: {now}\n"),
                (None, None) => String::new(),
            });
        }
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
        let maybe = |text: &Option<String>| text.as_deref().map_or("null".to_string(), quoted);
        let spec_changes: Vec<String> = self
            .spec_changes
            .iter()
            .map(|c| {
                format!(
                    "{{\"heading\":{},\"pr\":{},\"was\":{},\"now\":{}}}",
                    quoted(&c.heading),
                    quoted(&c.pr),
                    maybe(&c.was),
                    maybe(&c.now)
                )
            })
            .collect();
        let rule_changes: Vec<String> = self
            .rule_changes
            .iter()
            .map(|r| {
                let prs: Vec<String> = r.prs.iter().map(|pr| quoted(pr)).collect();
                format!(
                    "{{\"file\":{},\"heading\":{},\"prs\":[{}]}}",
                    quoted(r.file),
                    quoted(&r.heading),
                    prs.join(",")
                )
            })
            .collect();
        let unread: Vec<String> = self
            .unread
            .iter()
            .map(|u| {
                format!(
                    "{{\"commit\":{},\"url\":{}}}",
                    quoted(&u.commit),
                    quoted(&u.url)
                )
            })
            .collect();
        format!(
            "{{\"commit\":{},\"scores\":{scores},\"left_out\":{},\"merged\":[{}],\"regressed\":[{}],\"unrecorded\":[{}],\"waiting\":[{}],\"spec_changes\":[{}],\"rule_changes\":[{}],\"unread\":[{}]}}",
            quoted(&self.commit),
            self.left_out,
            merged.join(","),
            regressed.join(","),
            unrecorded.join(","),
            waiting.join(","),
            spec_changes.join(","),
            rule_changes.join(","),
            unread.join(",")
        )
    }
}
