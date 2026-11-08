//! Compares the working tree with the commit its branch started from.

use crate::Problem;
use crate::json::{self, Json};
use crate::score::{self, Score, Sentence};
use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Output};

const MAIN: &str = "refs/remotes/origin/main";
const SPEC_CHANGE: &str = "spec-change";
const PROTECTED: &[&str] = &[
    "SPEC.md",
    "harness/waived.txt",
    "harness/journey.toml",
    "harness/budgets.toml",
];

/// The commit where this branch left `origin/main`, or `None` if there is no
/// `origin/main` to compare with. The workspace must be the top of its own
/// repository, so that a workspace inside another repository's tree isn't
/// compared with that repository. Any other git failure is a problem, because
/// skipping the comparison would pass the gate.
pub fn find(root: &Path) -> Result<Option<String>, Problem> {
    if !root.join(".git").exists() {
        return Ok(None);
    }
    let main = format!("{MAIN}^{{commit}}");
    let args = ["rev-parse", "--verify", "--quiet", &main];
    let found = git(root, &args)?;
    match found.status.code() {
        Some(0) => {}
        Some(1) => return Ok(None),
        _ => {
            return Err(Problem(format!(
                "`git {}` failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&found.stderr).trim()
            )));
        }
    }
    let commit = git_text(root, &["merge-base", MAIN, "HEAD"])?;
    Ok(Some(commit.trim().to_string()))
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

/// Names each protected path changed without the label. `None` means a
/// protected path changed, but the branch has no open PR to read a label from.
pub fn missing_label(root: &Path, base: &str) -> Result<Option<Vec<String>>, Problem> {
    let changed = git_text(root, &["diff", "--name-only", "--no-renames", "-z", base])?;
    let touched: Vec<&str> = changed
        .split('\0')
        .filter(|path| PROTECTED.contains(path))
        .collect();
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
    Ok(Some(
        touched
            .iter()
            .map(|path| {
                format!(
                    "{path} changed, but this branch's PR doesn't have the `{SPEC_CHANGE}` label. Add the label, or undo the change"
                )
            })
            .collect(),
    ))
}

/// The labels of each open PR from this branch.
fn open_prs(root: &Path, branch: &str) -> Result<Vec<Vec<String>>, Problem> {
    let output = Command::new("gh")
        .args([
            "pr", "list", "--head", branch, "--state", "open", "--json", "labels",
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
    let unexpected = || Problem("`gh pr list` printed an unexpected shape".to_string());
    let prs = value.as_arr().ok_or_else(unexpected)?;
    prs.iter()
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

fn git(root: &Path, args: &[&str]) -> Result<Output, Problem> {
    Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| Problem(format!("couldn't run `git`: {e}")))
}

fn git_text(root: &Path, args: &[&str]) -> Result<String, Problem> {
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
