//! What makes a PR wait for the owner, read from its diff with its base and
//! from its body.
//!
//! The owner's merge is for awareness. A PR waits when it lowers the bar that
//! later PRs are held to, changes what this file decides, or leaves the owner
//! an action. Everything else merges under `auto:merge`. A count that fell is
//! compared in `accepts.rs`.

use crate::forge::OpenIssue;
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
/// The code that decides what a merge accepts. A file decides when it says
/// what waits for the owner, what a count or a floor is and whether one fell,
/// which checks a proof runs, or what makes a check pass. This file reads the
/// budgets itself, so that a change to how they are measured doesn't wait.
const DECIDES: [&str; 11] = [
    "xtask/src/accepts.rs",
    "xtask/src/base.rs",
    "xtask/src/floors.rs",
    "xtask/src/gate.rs",
    "xtask/src/journey.rs",
    "xtask/src/ledger.rs",
    "xtask/src/readme.rs",
    "xtask/src/score.rs",
    "xtask/src/spec.rs",
    "xtask/src/testrun.rs",
    "xtask/src/waits.rs",
];
/// The rest of `xtask/src/`, which runs what those decide, measures, or
/// reports. A test fails while a file is on neither list.
#[cfg(test)]
const RUNS: [&str; 21] = [
    "xtask/src/bench.rs",
    "xtask/src/bisect.rs",
    "xtask/src/forge.rs",
    "xtask/src/fuzz.rs",
    "xtask/src/gaps.rs",
    "xtask/src/group.rs",
    "xtask/src/json.rs",
    "xtask/src/limit.rs",
    "xtask/src/lock.rs",
    "xtask/src/main.rs",
    "xtask/src/merge.rs",
    "xtask/src/mirror.rs",
    "xtask/src/mutation.rs",
    "xtask/src/nightly.rs",
    "xtask/src/refresh.rs",
    "xtask/src/remove.rs",
    "xtask/src/report.rs",
    "xtask/src/roadmap.rs",
    "xtask/src/toml.rs",
    "xtask/src/work.rs",
    "xtask/src/workspace.rs",
];

/// What starts a line of a PR's body that names an action only the owner can
/// take after the merge, as in `Owner action: #123`. The issue says what to do.
const OWNER_ACTION: &str = "Owner action:";
const ACTION_NEEDED: &str = "action-needed";

/// Each reason that `head` waits for the owner, compared with `commit`, after
/// the owner actions that its PR's body names, which come first.
pub fn reasons(
    root: &Path,
    commit: &str,
    head: &str,
    actions: &[String],
) -> Result<Vec<String>, Problem> {
    let listing = base::git_text(
        root,
        &["diff", "--name-only", "--no-renames", "-z", commit, head],
    )?;
    let changed: Vec<&str> = listing.split('\0').filter(|p| !p.is_empty()).collect();
    let touched = |path: &str| changed.contains(&path);
    let mut reasons = actions.to_vec();
    if touched(SPEC) {
        reasons.extend(rewritten_units(root, commit, head)?);
    }
    if touched(WAIVERS) {
        let fields = |line: &str| line.splitn(3, '\t').take(2).collect::<Vec<_>>().join("\t");
        for added in added_entries(root, commit, head, WAIVERS, fields)? {
            let (heading, sentence) = added.split_once('\t').unwrap_or((&added, ""));
            reasons.push(format!(
                "it adds a waiver to {WAIVERS} under `{heading}`: {sentence}"
            ));
        }
    }
    if touched(BUDGETS) {
        let before = budgets_at(root, commit)?;
        let after = budgets_at(root, head)?;
        reasons.extend(loosened(&before, &after));
    }
    if touched(BUILD_SCRIPTS) {
        let fields = |line: &str| {
            line.split_whitespace()
                .take(2)
                .collect::<Vec<_>>()
                .join(" ")
        };
        for added in added_entries(root, commit, head, BUILD_SCRIPTS, fields)? {
            reasons.push(format!("it adds `{added}` to {BUILD_SCRIPTS}"));
        }
    }
    if changed.iter().any(|path| path.starts_with(GATE_TESTS)) {
        let (before, after) = (tests_at(root, commit)?, tests_at(root, head)?);
        if after < before {
            reasons.push(format!(
                "the number of tests under `{GATE_TESTS}` fell from {before} to {after}"
            ));
        }
    }
    for path in &changed {
        if path.starts_with(WORKFLOWS) {
            reasons.push(format!("it changes `{path}`"));
        } else if DECIDES.contains(path) {
            reasons.push(format!(
                "it changes `{path}`, which decides what waits for the owner"
            ));
        }
    }
    Ok(reasons)
}

/// Each owner action that the `Owner action:` lines of PR `number`'s body
/// name, as a reason that it waits for the owner. It reads the open issues
/// from `open`, and only when a line names one. The error says why a line
/// names no issue that waits for the owner.
pub fn owner_actions(
    number: u64,
    body: &str,
    open: impl FnOnce() -> Result<Vec<OpenIssue>, Problem>,
) -> Result<Result<Vec<String>, String>, Problem> {
    let line = format!("the `{OWNER_ACTION}` line of PR {number}");
    let mut named = Vec::new();
    for value in body
        .lines()
        .filter_map(|text| text.trim().strip_prefix(OWNER_ACTION))
    {
        // As in `Closes #123.`
        let value = value.trim();
        let issue = value.strip_suffix('.').unwrap_or(value).strip_prefix('#');
        match issue.and_then(|issue| issue.parse::<u64>().ok()) {
            Some(issue) => named.push(issue),
            None => {
                return Ok(Err(format!(
                    "{line} reads `{value}`, and it needs to read `#<issue>`"
                )));
            }
        }
    }
    if named.is_empty() {
        return Ok(Ok(Vec::new()));
    }
    let open = open()?;
    let mut reasons = Vec::new();
    for issue in named {
        let Some(found) = open.iter().find(|found| found.number == issue) else {
            return Ok(Err(format!(
                "{line} names issue {issue}, which isn't an open issue"
            )));
        };
        if !found.labels.iter().any(|label| label == ACTION_NEEDED) {
            return Ok(Err(format!(
                "{line} names issue {issue}, which doesn't have the label `{ACTION_NEEDED}`"
            )));
        }
        reasons.push(format!(
            "it leaves the owner an action, #{issue}: {}",
            found.title
        ));
    }
    Ok(Ok(reasons))
}

/// A file as a commit has it, or `None` if it isn't there.
pub fn file_at(root: &Path, commit: &str, path: &str) -> Result<Option<String>, Problem> {
    let listed = base::git_text(root, &["ls-tree", "--name-only", commit, "--", path])?;
    if listed.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(base::git_text(
        root,
        &["show", &format!("{commit}:{path}")],
    )?))
}

/// The units of SPEC.md in the base that `head` doesn't have. A unit ignores
/// whitespace, so a reflowed paragraph keeps its units.
fn rewritten_units(root: &Path, commit: &str, head: &str) -> Result<Vec<String>, Problem> {
    let units = |at: &str| -> Result<Vec<spec::Unit>, Problem> {
        let text = file_at(root, at, SPEC)?.unwrap_or_default();
        spec::extract(&text).map_err(|e| Problem(format!("SPEC.md at {at}: {e}")))
    };
    let after: HashSet<(String, String)> = units(head)?
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

/// The entries of a list file that `head` has and the base doesn't, each as
/// `key` reads it from a line. Comments and blank lines aren't entries.
fn added_entries(
    root: &Path,
    commit: &str,
    head: &str,
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
    Ok(entries(head)?
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

/// Each budget that the head removes, or whose target it raises or band it
/// widens.
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

#[cfg(test)]
mod tests {
    use super::{DECIDES, RUNS};
    use std::collections::BTreeSet;
    use std::fs;

    /// A new file under `xtask/src/` has to be put on one side of the rule.
    #[test]
    fn every_file_under_xtask_src_decides_or_not() {
        let root = crate::workspace::root().unwrap();
        let mut named = 0;
        for entry in fs::read_dir(root.join("xtask/src")).unwrap() {
            let name = entry.unwrap().file_name().into_string().unwrap();
            let path = format!("xtask/src/{name}");
            let sides = [DECIDES.contains(&&*path), RUNS.contains(&&*path)];
            assert_eq!(
                sides.iter().filter(|side| **side).count(),
                1,
                "`{path}` must be in exactly one of `DECIDES` and `RUNS`"
            );
            named += 1;
        }
        assert_eq!(named, DECIDES.len() + RUNS.len());
    }

    /// HARNESS.md names the files that decide, in the item that says so.
    #[test]
    fn harness_md_lists_the_files_that_decide() {
        let root = crate::workspace::root().unwrap();
        let text = fs::read_to_string(root.join("HARNESS.md")).unwrap();
        let (_, section) = text.split_once("### What waits for the owner").unwrap();
        let item: String = section
            .lines()
            .skip_while(|line| !line.starts_with("- changes a file under `xtask/src/`"))
            .enumerate()
            .take_while(|(at, line)| *at == 0 || line.starts_with("  "))
            .map(|(_, line)| format!("{line} "))
            .collect();
        let listed: BTreeSet<String> = item
            .split('`')
            .skip(1)
            .step_by(2)
            .filter(|name| name.ends_with(".rs"))
            .map(|name| format!("xtask/src/{name}"))
            .collect();
        let decides: BTreeSet<String> = DECIDES.iter().map(|path| path.to_string()).collect();
        assert_eq!(listed, decides);
    }
}
