//! `cargo xtask score`: which promises in SPEC.md have a passing check.

use crate::journey::{self, JOURNEY_FILE};
use crate::readme::{self, README_FILE};
use crate::spec::Arrives;
use crate::testrun::{self, Results};
use crate::{Problem, Verdict, bench, ledger, lock, spec, workspace};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

const WAIVERS: &str = "harness/waived.txt";

/// A SPEC.md heading, and a unit's text under it.
pub type Sentence = (String, String);

pub struct Score {
    pub proven: usize,
    pub promises: HashSet<Sentence>,
    /// How many checks quote each sentence.
    pub quoted: HashMap<Sentence, usize>,
    pub findings: Vec<String>,
    /// The promises no passing check proves, in SPEC order.
    pub unproven: Vec<Sentence>,
}

pub fn run(json: bool) -> Result<Verdict, Problem> {
    let root = workspace::root()?;
    let hold = lock::shared(&root)?;
    let results = testrun::run(&root, &hold)?;
    let score = compute(&root, &results)?;
    report_findings(&score);
    let (proven, promises) = (score.proven, score.promises.len());
    let record = ledger::at_head(&root)?;
    let journey = record.as_ref().map(|record| (record.passing, record.steps));
    let budgets = match &record {
        Some(record) if root.join(bench::BUDGETS_FILE).exists() => {
            Some(bench::met(&bench::read_budgets(&root)?, &record.budgets))
        }
        _ => None,
    };
    if json {
        let journey = journey.map_or(String::new(), |(passing, steps)| {
            format!(",\"journey\":{{\"passing\":{passing},\"steps\":{steps}}}")
        });
        let budgets = budgets.map_or(String::new(), |(met, targets)| {
            format!(",\"budgets\":{{\"met\":{met},\"targets\":{targets}}}")
        });
        println!("{{\"spec\":{{\"proven\":{proven},\"promises\":{promises}}}{journey}{budgets}}}");
    } else {
        println!("spec {proven} of {promises}");
        if let Some((passing, steps)) = journey {
            println!("journey {passing} of {steps}");
        }
        if let Some((met, targets)) = budgets {
            println!("budgets {met} of {targets}");
        }
    }
    Ok(if score.findings.is_empty() {
        Verdict::Pass
    } else {
        Verdict::Fail
    })
}

pub fn report_findings(score: &Score) {
    for finding in &score.findings {
        eprintln!("spec: {finding}");
    }
}

pub fn compute(root: &Path, results: &Results) -> Result<Score, Problem> {
    let text = fs::read_to_string(root.join("SPEC.md"))
        .map_err(|e| Problem(format!("couldn't read SPEC.md: {e}")))?;
    let units = spec::extract(&text).map_err(|e| Problem(format!("SPEC.md: {e}")))?;
    let ordered: Vec<Sentence> = units.into_iter().map(|u| (u.heading, u.text)).collect();
    let units: HashSet<Sentence> = ordered.iter().cloned().collect();

    let mut proven = HashSet::new();
    let mut quoted = HashMap::new();
    let mut findings = Vec::new();
    let mut promises = units.clone();
    for (line, sentence) in read_waivers(root)? {
        if !units.contains(&sentence) {
            findings.push(format!(
                "{WAIVERS}:{line}: no such sentence under `{}` in SPEC.md: {}. Match the waiver to SPEC.md, or remove it",
                sentence.0, sentence.1
            ));
        }
        promises.remove(&sentence);
    }
    for package in workspace::packages(root)? {
        if package.name == "xtask" {
            continue;
        }
        let mut definitions: HashMap<String, usize> = HashMap::new();
        let mut checks = Vec::new();
        for file in rust_files(&package.directory)? {
            let source = fs::read_to_string(&file)
                .map_err(|e| Problem(format!("couldn't read {}: {e}", file.display())))?;
            let shown = file
                .strip_prefix(root)
                .unwrap_or(&file)
                .display()
                .to_string();
            for sentence in quotes_in(&source) {
                *quoted.entry(sentence).or_default() += 1;
            }
            for name in test_functions(&source) {
                *definitions.entry(name).or_default() += 1;
            }
            for item in scan(&source) {
                match item {
                    Scanned::Orphan { line } => findings.push(format!(
                        "{shown}:{line}: a spec comment must be `// spec(<heading>): <sentence>`, directly above a #[test] function"
                    )),
                    Scanned::Check {
                        line,
                        quotes,
                        function,
                    } => checks.push((shown.clone(), line, quotes, function)),
                }
            }
        }
        for (shown, line, quotes, function) in checks {
            if definitions.get(&function).copied().unwrap_or(0) > 1 {
                findings.push(format!(
                    "{shown}:{line}: `{function}` names more than one test in package {}. A check's name must be unique within its package",
                    package.name
                ));
                continue;
            }
            let passed = results.passed(&package.name, &function);
            for quote in quotes {
                let key = (quote.heading, quote.text);
                if promises.contains(&key) {
                    if passed && !package.fake {
                        proven.insert(key);
                    }
                } else if units.contains(&key) {
                    findings.push(format!(
                        "{shown}:{}: this sentence under `{}` is waived, so it isn't a promise: {}. Remove the check or the waiver",
                        quote.line, key.0, key.1
                    ));
                } else {
                    findings.push(format!(
                        "{shown}:{}: no such sentence under `{}` in SPEC.md: {}. Match the quote to SPEC.md, or remove the check",
                        quote.line, key.0, key.1
                    ));
                }
            }
        }
    }
    findings.extend(command_findings(root, &text)?);
    let unproven = ordered
        .into_iter()
        .filter(|sentence| promises.contains(sentence) && !proven.contains(sentence))
        .collect();
    Ok(Score {
        proven: proven.len(),
        promises,
        quoted,
        findings,
        unproven,
    })
}

/// The milestones must list exactly the commands that SPEC.md has for v0.1.0,
/// so that neither file names a command the other doesn't know. The README
/// must describe every command, so that the roadmap can.
fn command_findings(root: &Path, spec: &str) -> Result<Vec<String>, Problem> {
    let commands = spec::commands(spec);
    let headings = spec::headings(spec);
    let mut findings = Vec::new();
    for command in &commands {
        for title in &command.specified_by {
            if !headings.contains(title) {
                findings.push(format!(
                    "SPEC.md: `{}` says that `{title}` specifies it, and SPEC.md has no `{title}` heading. Name a heading as it is written",
                    command.name
                ));
            }
        }
    }
    // A journey file from before milestones lists no commands, and one from
    // before summaries has none. Today's reader refuses both. The nightly
    // bisect gates such trees, so they are left alone here, as is a workspace
    // with no journey file.
    let listing = fs::read_to_string(root.join(JOURNEY_FILE)).unwrap_or_default();
    let has_milestones = listing.lines().any(|line| line.trim() == "[[milestone]]");
    let key = |line: &str| line.split_once('=').map(|(key, _)| key.trim() == "summary");
    let has_summaries = listing.lines().any(|line| key(line) == Some(true));
    if !has_milestones || !has_summaries {
        return Ok(findings);
    }
    let milestones = journey::milestones(root)?;
    for milestone in &milestones {
        for name in &milestone.commands {
            let what = match commands.iter().find(|command| command.name == *name) {
                None => {
                    "which has no heading under Commands in SPEC.md. Add the heading, or fix the name"
                }
                Some(command) if command.arrives != Arrives::V010 => {
                    "which SPEC.md marks as planned or proposed. Remove the mark, or take the command out of the milestone"
                }
                Some(_) => continue,
            };
            findings.push(format!(
                "{JOURNEY_FILE}: milestone `{}` lists `{name}`, {what}",
                milestone.name
            ));
        }
    }
    let listed = |name: &String| milestones.iter().any(|m| m.commands.contains(name));
    for command in &commands {
        if command.arrives == Arrives::V010 && !listed(&command.name) {
            findings.push(format!(
                "SPEC.md: `{}` is a v0.1.0 command, and no milestone in {JOURNEY_FILE} lists it. Add it to a milestone, or mark it `**Planned.**`",
                command.name
            ));
        }
    }
    // The roadmap describes each command with its line in the README. A
    // workspace with no README describes none.
    if let Ok(text) = fs::read_to_string(root.join(README_FILE)) {
        let lines = readme::command_lines(&text);
        for command in &commands {
            if readme::line_for(&lines, &command.name).is_none() {
                findings.push(format!(
                    "{README_FILE}: `{}` is a command in SPEC.md, and no item under Core commands describes it. Add an item that starts with the command in backticks, then a colon and what it does",
                    command.name
                ));
            }
        }
    }
    Ok(findings)
}

/// Each waived sentence, with its line number. A missing file waives nothing.
fn read_waivers(root: &Path) -> Result<Vec<(usize, Sentence)>, Problem> {
    let text = match fs::read_to_string(root.join(WAIVERS)) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Problem(format!("couldn't read {WAIVERS}: {e}"))),
    };
    let mut waivers = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.splitn(3, '\t');
        match (fields.next(), fields.next(), fields.next()) {
            (Some(heading), Some(unit), Some(reason)) if !reason.trim().is_empty() => {
                waivers.push((index + 1, (heading.to_string(), unit.to_string())));
            }
            _ => {
                return Err(Problem(format!(
                    "{WAIVERS}:{}: expected `<heading><TAB><unit><TAB><reason>`, got `{line}`",
                    index + 1
                )));
            }
        }
    }
    Ok(waivers)
}

/// Each sentence quoted by a check in this source, once per check.
pub fn quotes_in(source: &str) -> Vec<Sentence> {
    let mut sentences = Vec::new();
    for item in scan(source) {
        if let Scanned::Check { quotes, .. } = item {
            let distinct: HashSet<Sentence> =
                quotes.into_iter().map(|q| (q.heading, q.text)).collect();
            sentences.extend(distinct);
        }
    }
    sentences
}

pub fn rust_files(directory: &Path) -> Result<Vec<PathBuf>, Problem> {
    let mut files = Vec::new();
    let mut pending = vec![directory.to_path_buf()];
    while let Some(current) = pending.pop() {
        let entries = fs::read_dir(&current)
            .map_err(|e| Problem(format!("couldn't read {}: {e}", current.display())))?;
        for entry in entries {
            let entry = entry.map_err(|e| Problem(format!("couldn't read a directory: {e}")))?;
            let path = entry.path();
            let kind = entry
                .file_type()
                .map_err(|e| Problem(format!("couldn't read {}: {e}", path.display())))?;
            if kind.is_dir() {
                if entry.file_name() != "target" {
                    pending.push(path);
                }
            } else if path.extension().is_some_and(|e| e == "rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

struct Quote {
    line: usize,
    heading: String,
    text: String,
}

enum Scanned {
    Check {
        line: usize,
        quotes: Vec<Quote>,
        function: String,
    },
    Orphan {
        line: usize,
    },
}

/// Finds runs of `// spec(...)` comments and the test each run sits above.
fn scan(source: &str) -> Vec<Scanned> {
    let lines: Vec<&str> = source.lines().map(str::trim).collect();
    let mut found = Vec::new();
    let mut at = 0;
    while at < lines.len() {
        if !lines[at].starts_with("// spec(") {
            at += 1;
            continue;
        }
        let first_line = at + 1;
        let mut quotes = Vec::new();
        let mut malformed = false;
        while at < lines.len() && lines[at].starts_with("// spec(") {
            match parse_quote(lines[at]) {
                Some((heading, text)) => quotes.push(Quote {
                    line: at + 1,
                    heading: heading.to_string(),
                    text: text.to_string(),
                }),
                None => malformed = true,
            }
            at += 1;
        }
        let (after, is_test) = skip_attributes(&lines, at);
        at = after;
        let function = lines.get(at).copied().and_then(function_name);
        match function {
            Some(function) if is_test && !malformed => found.push(Scanned::Check {
                line: first_line,
                quotes,
                function: function.to_string(),
            }),
            _ => found.push(Scanned::Orphan { line: first_line }),
        }
    }
    found
}

/// Moves past any attributes, including ones spread over several lines, and
/// says whether one of them was `#[test]`.
fn skip_attributes(lines: &[&str], mut at: usize) -> (usize, bool) {
    let mut is_test = false;
    while at < lines.len() && lines[at].starts_with("#[") {
        is_test |= lines[at] == "#[test]";
        let mut depth = 0i32;
        while at < lines.len() {
            depth += lines[at].matches('[').count() as i32;
            depth -= lines[at].matches(']').count() as i32;
            at += 1;
            if depth <= 0 {
                break;
            }
        }
    }
    (at, is_test)
}

/// The name of every `#[test]` function, including disabled ones.
pub fn test_functions(source: &str) -> Vec<String> {
    let lines: Vec<&str> = source.lines().map(str::trim).collect();
    let mut names = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if *line == "#[test]" {
            let (after, _) = skip_attributes(&lines, index);
            if let Some(name) = lines.get(after).copied().and_then(function_name) {
                names.push(name.to_string());
            }
        }
    }
    names
}

fn parse_quote(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix("// spec(")?;
    let (heading, text) = rest.split_once("): ")?;
    (!heading.is_empty() && !text.trim().is_empty()).then(|| (heading, text.trim()))
}

fn function_name(line: &str) -> Option<&str> {
    let mut rest = line;
    for prefix in ["pub ", "async "] {
        rest = rest.strip_prefix(prefix).unwrap_or(rest);
    }
    let rest = rest.strip_prefix("fn ")?;
    let end = rest.find(['(', '<'])?;
    Some(rest[..end].trim())
}
