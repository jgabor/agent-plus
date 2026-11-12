//! Compares the working tree with the commit its branch started from.

use crate::Problem;
use crate::json::{self, Json};
use crate::score::{self, Score, Sentence};
use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Output};

pub const MAIN: &str = "refs/remotes/origin/main";
pub const SPEC_CHANGE: &str = "spec-change";
const LEDGER: &str = "harness/ledger";
/// A path that ends in `/` covers everything under it.
const PROTECTED: [&str; 6] = [
    "SPEC.md",
    "AGENTS.md",
    "HARNESS.md",
    "xtask/",
    "harness/",
    ".github/",
];
const NOT_PROTECTED: [&str; 2] = ["harness/ledger/", "harness/journey/"];

/// The commit where this branch left `origin/main`, or `None` if there is no
/// `origin/main` to compare with. The workspace must be the top of its own
/// repository, so that a workspace inside another repository's tree isn't
/// compared with that repository. Any other git failure is a problem, because
/// skipping the comparison would pass the gate.
pub fn find(root: &Path) -> Result<Option<String>, Problem> {
    if !root.join(".git").exists() {
        return Ok(None);
    }
    if !commit_exists(root, MAIN)? {
        return Ok(None);
    }
    let commit = git_text(root, &["merge-base", MAIN, "HEAD"])?;
    Ok(Some(commit.trim().to_string()))
}

/// Whether a name resolves to a commit. A git failure is a problem, so that it
/// isn't mistaken for a missing commit.
pub fn commit_exists(root: &Path, name: &str) -> Result<bool, Problem> {
    let name = format!("{name}^{{commit}}");
    let args = ["rev-parse", "--verify", "--quiet", &name];
    let found = git(root, &args)?;
    match found.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(Problem(format!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&found.stderr).trim()
        ))),
    }
}

/// Names each promise that fewer checks quote than in the base. Counting
/// sentences lets a check be renamed or moved.
pub fn removed_checks(root: &Path, base: &str, score: &Score) -> Result<Vec<String>, Problem> {
    let listing = git_text(root, &["ls-tree", "-r", "-z", "--name-only", base])?;
    let mut before: HashMap<Sentence, usize> = HashMap::new();
    for path in listing
        .split('\0')
        .filter(|path| path.ends_with(".rs") && !path.starts_with("xtask/"))
    {
        let source = git_text(root, &["show", &format!("{base}:{path}")])?;
        for sentence in score::quotes_in(&source) {
            *before.entry(sentence).or_default() += 1;
        }
    }
    let mut findings: Vec<String> = before
        .into_iter()
        .filter(|(sentence, count)| {
            score.promises.contains(sentence)
                && score.quoted.get(sentence).copied().unwrap_or(0) < *count
        })
        .map(|((heading, text), _)| {
            format!(
                "a check quoting this sentence under `{heading}` was removed: {text}. Restore the check, or remove the sentence in a `{SPEC_CHANGE}` PR"
            )
        })
        .collect();
    findings.sort();
    Ok(findings)
}

/// Whether HEAD is exactly `base` with one earlier commit of its history undone,
/// going by the `git revert` message and replaying that revert. The ledger stays
/// as `base` has it, because a record says what was measured.
pub fn is_exact_revert(root: &Path, base: &str) -> Result<bool, Problem> {
    let messages = git_text(root, &["log", "--format=%B", &format!("{base}..HEAD")])?;
    let Some(commit) = messages.lines().find_map(reverted_commit) else {
        return Ok(false);
    };
    let parent = format!("{commit}~1");
    let in_history = git(root, &["merge-base", "--is-ancestor", commit, base])?
        .status
        .success();
    if !in_history {
        return Ok(false);
    }
    // A conflict or a commit with no parent gives no tree that HEAD can match.
    let from = format!("--merge-base={commit}");
    let replayed = git(root, &["merge-tree", "--write-tree", &from, base, &parent])?;
    let tree = String::from_utf8_lossy(&replayed.stdout);
    let tree = tree.lines().next().unwrap_or_default();
    let same = |args: &[&str]| -> Result<bool, Problem> {
        Ok(git(root, &[&["diff", "--quiet"], args].concat())?
            .status
            .success())
    };
    Ok(same(&[base, "HEAD", "--", LEDGER])?
        && same(&[tree, "HEAD", "--", ".", &format!(":(exclude){LEDGER}")])?)
}

fn reverted_commit(line: &str) -> Option<&str> {
    let commit = line
        .strip_prefix("This reverts commit ")?
        .strip_suffix('.')?;
    (commit.len() == 40 && commit.chars().all(|c| c.is_ascii_hexdigit())).then_some(commit)
}

/// Names each protected path changed without the label. `None` means a
/// protected path changed, but the branch has no open PR to read a label from.
pub fn missing_label(root: &Path, base: &str) -> Result<Option<Vec<String>>, Problem> {
    let touched = protected_changes(root, base)?;
    if touched.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let branch = git_text(root, &["branch", "--show-current"])?;
    let branch = branch.trim();
    if branch.is_empty() {
        return Err(Problem(format!(
            "{} changed on a detached HEAD, so no PR can be found. Check out the PR's branch",
            touched.join(", ")
        )));
    }
    let prs = open_prs(root, branch)?;
    if prs.is_empty() {
        return Ok(None);
    }
    if prs
        .iter()
        .all(|labels| labels.iter().any(|label| label == SPEC_CHANGE))
    {
        return Ok(Some(Vec::new()));
    }
    Ok(Some(label_findings(&touched)))
}

/// The same check for a PR whose labels the caller has already read.
pub fn missing_label_on(
    root: &Path,
    base: &str,
    labels: &[String],
) -> Result<Vec<String>, Problem> {
    if labels.iter().any(|label| label == SPEC_CHANGE) {
        return Ok(Vec::new());
    }
    Ok(label_findings(&protected_changes(root, base)?))
}

pub fn protected_changes(root: &Path, base: &str) -> Result<Vec<String>, Problem> {
    let changed = git_text(root, &["diff", "--name-only", "--no-renames", "-z", base])?;
    Ok(changed
        .split('\0')
        .filter(|path| is_protected(path))
        .map(str::to_string)
        .collect())
}

fn is_protected(path: &str) -> bool {
    let covered = |entry: &&str| {
        if entry.ends_with('/') {
            path.starts_with(*entry)
        } else {
            path == *entry
        }
    };
    PROTECTED.iter().any(covered) && !NOT_PROTECTED.iter().any(covered)
}

fn label_findings(touched: &[String]) -> Vec<String> {
    touched
        .iter()
        .map(|path| {
            format!(
                "{path} changed, but this branch's PR doesn't have the `{SPEC_CHANGE}` label. Add the label, or undo the change"
            )
        })
        .collect()
}

/// The text of a `Budget reason: <text>` line in the body of this branch's
/// open PR. A reason with no text isn't one.
pub fn budget_reason(root: &Path) -> Result<Option<String>, Problem> {
    let branch = git_text(root, &["branch", "--show-current"])?;
    let branch = branch.trim();
    if branch.is_empty() {
        return Ok(None);
    }
    for pr in pr_list(root, branch, "body")? {
        let body = pr
            .get("body")
            .and_then(Json::as_str)
            .ok_or_else(unexpected)?;
        if let Some(reason) = reason_in(body) {
            return Ok(Some(reason));
        }
    }
    Ok(None)
}

pub fn reason_in(body: &str) -> Option<String> {
    body.lines()
        .filter_map(|line| line.strip_prefix("Budget reason:"))
        .map(str::trim)
        .find(|text| !text.is_empty())
        .map(str::to_string)
}

fn unexpected() -> Problem {
    Problem("`gh pr list` printed an unexpected shape".to_string())
}

/// One field of each open PR from this branch.
fn pr_list(root: &Path, branch: &str, field: &str) -> Result<Vec<Json>, Problem> {
    let output = Command::new("gh")
        .args([
            "pr", "list", "--head", branch, "--state", "open", "--json", field,
        ])
        .current_dir(root)
        .output()
        .map_err(|e| Problem(format!("couldn't run `gh`: {e}. Install the GitHub CLI")))?;
    if !output.status.success() {
        return Err(Problem(format!(
            "`gh pr list` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let value = json::parse(&String::from_utf8_lossy(&output.stdout))
        .map_err(|e| Problem(format!("couldn't read `gh pr list` output: {e}")))?;
    Ok(value.as_arr().ok_or_else(unexpected)?.to_vec())
}

/// The labels of each open PR from this branch.
fn open_prs(root: &Path, branch: &str) -> Result<Vec<Vec<String>>, Problem> {
    pr_list(root, branch, "labels")?
        .iter()
        .map(|pr| {
            pr.get("labels")
                .and_then(Json::as_arr)
                .ok_or_else(unexpected)?
                .iter()
                .map(|label| {
                    label
                        .get("name")
                        .and_then(Json::as_str)
                        .map(str::to_string)
                        .ok_or_else(unexpected)
                })
                .collect()
        })
        .collect()
}

pub fn git(root: &Path, args: &[&str]) -> Result<Output, Problem> {
    Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| Problem(format!("couldn't run `git`: {e}")))
}

pub fn git_text(root: &Path, args: &[&str]) -> Result<String, Problem> {
    let output = git(root, args)?;
    if !output.status.success() {
        return Err(Problem(format!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
