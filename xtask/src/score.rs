//! `cargo xtask score`: which promises in SPEC.md have a passing check.

use crate::testrun::{self, Results};
use crate::{Problem, Verdict, spec, workspace};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub struct Score {
    pub proven: usize,
    pub promises: usize,
    pub findings: Vec<String>,
}

pub fn run(json: bool) -> Result<Verdict, Problem> {
    let root = workspace::root()?;
    let results = testrun::run(&root)?;
    let score = compute(&root, &results)?;
    report_findings(&score);
    if json {
        println!(
            "{{\"spec\":{{\"proven\":{},\"promises\":{}}}}}",
            score.proven, score.promises
        );
    } else {
        println!("spec {} of {}", score.proven, score.promises);
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
    let known: HashSet<(String, String)> = units
        .iter()
        .map(|u| (u.heading.clone(), u.text.clone()))
        .collect();

    let mut proven = HashSet::new();
    let mut findings = Vec::new();
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
                if !known.contains(&key) {
                    findings.push(format!(
                        "{shown}:{}: no such sentence under `{}` in SPEC.md: {}. Match the quote to SPEC.md, or remove the check",
                        quote.line, key.0, key.1
                    ));
                } else if passed {
                    proven.insert(key);
                }
            }
        }
    }
    Ok(Score {
        proven: proven.len(),
        promises: units.len(),
        findings,
    })
}

fn rust_files(directory: &Path) -> Result<Vec<PathBuf>, Problem> {
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
fn test_functions(source: &str) -> Vec<String> {
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
