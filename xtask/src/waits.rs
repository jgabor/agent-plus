//! What makes a PR wait for the owner, read from its diff with its base.
//!
//! The owner's merge is for awareness. A PR waits when it lowers the bar that
//! later PRs are held to, or changes what this file decides. Everything else
//! merges under `auto:merge`. A count that fell is compared in `merge.rs`.

use crate::toml::{self, Table, Value};
use crate::{Problem, base, spec};
use std::collections::HashSet;
use std::path::Path;

const SPEC: &str = "SPEC.md";
const WAIVERS: &str = "harness/waived.txt";
const BUDGETS: &str = "harness/budgets.toml";
const BUILD_SCRIPTS: &str = "harness/build-scripts.txt";
const GATE_TESTS: &str = "xtask/";
/// A workflow runs with a token that can write, and a leaked token can't be
/// changed afterwards.
const WORKFLOWS: &str = ".github/";
/// The code that decides what waits, and what a SPEC.md unit is. This file
/// reads the budgets itself, so that a change to how they are measured doesn't
/// wait.
const DEFINES: [&str; 4] = [
    "xtask/src/waits.rs",
    "xtask/src/merge.rs",
    "xtask/src/gate.rs",
    "xtask/src/spec.rs",
];

/// Each reason that HEAD waits for the owner, compared with `commit`.
pub fn reasons(root: &Path, commit: &str) -> Result<Vec<String>, Problem> {
    let listing = base::git_text(
        root,
        &["diff", "--name-only", "--no-renames", "-z", commit, "HEAD"],
    )?;
    let changed: Vec<&str> = listing.split('\0').filter(|p| !p.is_empty()).collect();
    let touched = |path: &str| changed.contains(&path);
    let mut reasons = Vec::new();
    if touched(SPEC) {
        reasons.extend(rewritten_units(root, commit)?);
    }
    if touched(WAIVERS) {
        let fields = |line: &str| line.splitn(3, '\t').take(2).collect::<Vec<_>>().join("\t");
        for added in added_entries(root, commit, WAIVERS, fields)? {
            let (heading, sentence) = added.split_once('\t').unwrap_or((&added, ""));
            reasons.push(format!(
                "it adds a waiver to {WAIVERS} under `{heading}`: {sentence}"
            ));
        }
    }
    if touched(BUDGETS) {
        let before = budgets_at(root, commit)?;
        let after = budgets_at(root, "HEAD")?;
        reasons.extend(loosened(&before, &after));
    }
    if touched(BUILD_SCRIPTS) {
        let fields = |line: &str| {
            line.split_whitespace()
                .take(2)
                .collect::<Vec<_>>()
                .join(" ")
        };
        for added in added_entries(root, commit, BUILD_SCRIPTS, fields)? {
            reasons.push(format!("it adds `{added}` to {BUILD_SCRIPTS}"));
        }
    }
    if changed.iter().any(|path| path.starts_with(GATE_TESTS)) {
        let (before, after) = (tests_at(root, commit)?, tests_at(root, "HEAD")?);
        if after < before {
            reasons.push(format!(
                "the number of tests under `{GATE_TESTS}` fell from {before} to {after}"
            ));
        }
    }
    for path in &changed {
        if path.starts_with(WORKFLOWS) {
            reasons.push(format!("it changes `{path}`"));
        } else if DEFINES.contains(path) {
            reasons.push(format!(
                "it changes `{path}`, which decides what waits for the owner"
            ));
        }
    }
    Ok(reasons)
}

/// A file as a commit has it, or `None` if it isn't there.
fn file_at(root: &Path, commit: &str, path: &str) -> Result<Option<String>, Problem> {
    let listed = base::git_text(root, &["ls-tree", "--name-only", commit, "--", path])?;
    if listed.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(base::git_text(
        root,
        &["show", &format!("{commit}:{path}")],
    )?))
}

/// The units of SPEC.md in the base that HEAD doesn't have. A unit ignores
/// whitespace, so a reflowed paragraph keeps its units.
fn rewritten_units(root: &Path, commit: &str) -> Result<Vec<String>, Problem> {
    let units = |at: &str| -> Result<Vec<spec::Unit>, Problem> {
        let text = file_at(root, at, SPEC)?.unwrap_or_default();
        spec::extract(&text).map_err(|e| Problem(format!("SPEC.md at {at}: {e}")))
    };
    let after: HashSet<(String, String)> = units("HEAD")?
        .into_iter()
        .map(|unit| (unit.heading, unit.text))
        .collect();
    Ok(units(commit)?
        .into_iter()
        .filter(|unit| !after.contains(&(unit.heading.clone(), unit.text.clone())))
        .map(|unit| {
            format!(
                "it changes or removes this sentence of SPEC.md under `{}`: {}",
                unit.heading, unit.text
            )
        })
        .collect())
}

/// The entries of a list file that HEAD has and the base doesn't, each as
/// `key` reads it from a line. Comments and blank lines aren't entries.
fn added_entries(
    root: &Path,
    commit: &str,
    path: &str,
    key: impl Fn(&str) -> String,
) -> Result<Vec<String>, Problem> {
    let entries = |at: &str| -> Result<Vec<String>, Problem> {
        let text = file_at(root, at, path)?.unwrap_or_default();
        Ok(text
            .lines()
            .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
            .map(&key)
            .collect())
    };
    let before: HashSet<String> = entries(commit)?.into_iter().collect();
    Ok(entries("HEAD")?
        .into_iter()
        .filter(|entry| !before.contains(entry))
        .collect())
}

/// What a budget holds a value to. A value is better lower, so raising a
/// target or a band loosens it. A budget with no band can't fail.
struct Budget {
    key: String,
    target: Option<f64>,
    /// The percentage and the absolute amount, either of which may be missing.
    band: Option<(f64, f64)>,
}

/// The budgets as a commit has them, as `bench.rs` reads them.
fn budgets_at(root: &Path, commit: &str) -> Result<Vec<Budget>, Problem> {
    let text = file_at(root, commit, BUDGETS)?.unwrap_or_default();
    let bad = |e: String| Problem(format!("{BUDGETS} at {commit}: {e}"));
    let number = |table: &Table, field: &str| match table.get(field) {
        Some(Value::Int(number)) => Some(*number as f64),
        Some(Value::Float(number)) => Some(*number),
        _ => None,
    };
    toml::array_of_tables(&text, "budget")
        .map_err(bad)?
        .iter()
        .map(|table| {
            let key = table
                .str("key")
                .ok_or_else(|| bad("a budget has no `key`".to_string()))?;
            let (pct, abs) = (number(table, "band_pct"), number(table, "band_abs"));
            Ok(Budget {
                key: key.to_string(),
                target: number(table, "target"),
                band: (pct.is_some() || abs.is_some())
                    .then(|| (pct.unwrap_or(0.0), abs.unwrap_or(0.0))),
            })
        })
        .collect()
}

/// Each budget that HEAD removes, or whose target it raises or band it widens.
fn loosened(before: &[Budget], after: &[Budget]) -> Vec<String> {
    let mut reasons = Vec::new();
    for old in before {
        let key = &old.key;
        let Some(new) = after.iter().find(|budget| &budget.key == key) else {
            reasons.push(format!("it removes the budget `{key}`"));
            continue;
        };
        match (old.target, new.target) {
            (Some(_), None) => reasons.push(format!("it removes the target of `{key}`")),
            (Some(from), Some(to)) if to > from => reasons.push(format!(
                "it raises the target of `{key}` from {from} to {to}"
            )),
            _ => {}
        }
        match (old.band, new.band) {
            (Some(_), None) => reasons.push(format!("it removes the band of `{key}`")),
            (Some((pct, abs)), Some((to_pct, to_abs))) if to_pct > pct || to_abs > abs => {
                reasons.push(format!(
                    "it widens the band of `{key}` from {pct}% and {abs} to {to_pct}% and {to_abs}"
                ));
            }
            _ => {}
        }
    }
    reasons
}

/// How many `#[test]` functions the Rust files under `xtask/` hold at a
/// commit. Counting them lets a test be renamed or moved.
fn tests_at(root: &Path, commit: &str) -> Result<usize, Problem> {
    let pattern = r"^[[:space:]]*#\[test\][[:space:]]*$";
    let pathspec = format!("{GATE_TESTS}*.rs");
    let args = ["grep", "-c", "-E", pattern, commit, "--", &pathspec];
    let output = base::git(root, &args)?;
    match output.status.code() {
        Some(0) => {}
        Some(1) => return Ok(0),
        _ => {
            return Err(Problem(format!(
                "`git {}` failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.rsplit_once(':')?.1.parse::<usize>().ok())
        .sum())
}
