//! What the harness reads from GitHub and writes to it.

use crate::Problem;
use crate::json::{self, Json};
use crate::lock::Hold;
use std::collections::HashMap;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const FIELDS: &str = "number,state,body,labels,comments";
const ISSUE_LIMIT: usize = 2000;
/// `gh issue list` gives at most this many comments per issue.
pub const LISTED_COMMENTS: usize = 100;
const PR_LIMIT: usize = 1000;
const ISSUES: &str = "repos/{owner}/{repo}/issues";

#[derive(Clone, Debug)]
pub struct Comment {
    pub id: u64,
    pub author: String,
    pub body: String,
}

#[derive(Clone, Debug)]
pub struct Issue {
    pub number: u64,
    pub open: bool,
    pub body: String,
    pub labels: Vec<String>,
    pub comments: Vec<Comment>,
}

/// An open issue, as far as the nightly report needs one.
pub struct OpenIssue {
    pub number: u64,
    pub title: String,
    pub labels: Vec<String>,
    pub pinned: bool,
}

/// A PR, as far as the merge sequence needs one.
pub struct PullRequest {
    pub state: String,
    /// A branch in another repository, whose name means nothing in this one.
    pub from_fork: bool,
    pub head_ref: String,
    pub base_ref: String,
    pub body: String,
    pub labels: Vec<String>,
    /// The issues in this repository that GitHub will close when this merges,
    /// from any closing word.
    pub closing_issues: Vec<u64>,
    pub changes_requested: bool,
}

/// An open PR, as far as `refresh` needs one.
pub struct OpenPr {
    pub number: u64,
    pub from_fork: bool,
    pub head_ref: String,
    pub head: String,
    pub closing_issues: Vec<u64>,
}

pub trait Forge {
    /// The login of the repository's owner.
    fn owner(&self) -> Result<String, Problem>;
    fn issues(&self) -> Result<Vec<Issue>, Problem>;
    fn issue(&self, number: u64) -> Result<Issue, Problem>;
    /// The date of the branch's newest commit, or `None` if there's no branch.
    fn branch_date(&self, branch: &str) -> Result<Option<String>, Problem>;
    fn add_label(&self, number: u64, label: &str) -> Result<(), Problem>;
    /// Returns the new comment's id.
    fn post_comment(&self, number: u64, body: &str) -> Result<u64, Problem>;
    fn edit_comment(&self, id: u64, body: &str) -> Result<(), Problem>;
}

/// The real forge, through the `gh` command.
pub struct Gh {
    root: PathBuf,
}

impl Gh {
    pub fn new(root: &Path) -> Gh {
        Gh {
            root: root.to_path_buf(),
        }
    }

    fn output(&self, args: &[&str], input: Option<&str>) -> Result<Output, Problem> {
        let stdin = if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        };
        self.spawned(args, stdin, input)
    }

    fn spawned(&self, args: &[&str], stdin: Stdio, input: Option<&str>) -> Result<Output, Problem> {
        let couldnt = |e: std::io::Error| Problem(format!("couldn't run `gh`: {e}"));
        let mut child = Command::new("gh")
            .args(args)
            .current_dir(&self.root)
            .stdin(stdin)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(couldnt)?;
        if let Some(input) = input {
            let mut stdin = child.stdin.take().expect("stdin is piped");
            // `gh` can exit before reading its input. Its exit status and its
            // error message say what happened.
            match stdin.write_all(input.as_bytes()) {
                Err(e) if e.kind() != ErrorKind::BrokenPipe => return Err(couldnt(e)),
                _ => {}
            }
        }
        child.wait_with_output().map_err(couldnt)
    }

    fn text(&self, args: &[&str], input: Option<&str>) -> Result<String, Problem> {
        self.checked(args, self.output(args, input)?)
    }

    fn checked(&self, args: &[&str], output: Output) -> Result<String, Problem> {
        if !output.status.success() {
            return Err(Problem(format!(
                "`gh {}` failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn json(&self, args: &[&str]) -> Result<Json, Problem> {
        json::parse(&self.text(args, None)?)
            .map_err(|e| Problem(format!("couldn't read `gh {}` output: {e}", args.join(" "))))
    }

    pub fn pull_request(&self, number: u64) -> Result<PullRequest, Problem> {
        let value = self.json(&[
            "pr",
            "view",
            &number.to_string(),
            "--json",
            "url,isCrossRepository,state,headRefName,baseRefName,body,labels,closingIssuesReferences,reviews",
        ])?;
        read_pull_request(&value).ok_or_else(|| unexpected("gh pr view"))
    }

    /// Issues and PRs share one sequence of numbers.
    pub fn is_pr(&self, number: u64) -> Result<bool, Problem> {
        let value = self.json(&["api", &format!("{ISSUES}/{number}")])?;
        Ok(value.get("pull_request").is_some())
    }

    /// The open PRs that GitHub will close this issue with, from any closing
    /// word.
    pub fn open_prs_closing(&self, issue: u64) -> Result<Vec<u64>, Problem> {
        let limit = PR_LIMIT.to_string();
        let args = [
            "pr",
            "list",
            "--state",
            "open",
            "--limit",
            &limit,
            "--json",
            "number,url,closingIssuesReferences",
        ];
        let value = self.json(&args)?;
        let prs = value.as_arr().ok_or_else(|| unexpected("gh pr list"))?;
        if prs.len() >= PR_LIMIT {
            return Err(Problem(format!(
                "this repository has {PR_LIMIT} open PRs or more, so the one that closes issue {issue} could be missing. Raise `PR_LIMIT`"
            )));
        }
        let mut closing = Vec::new();
        for pr in prs {
            let (number, closes) = read_closing(pr).ok_or_else(|| unexpected("gh pr list"))?;
            if closes.contains(&issue) {
                closing.push(number);
            }
        }
        Ok(closing)
    }

    pub fn open_prs(&self) -> Result<Vec<OpenPr>, Problem> {
        let limit = PR_LIMIT.to_string();
        let args = [
            "pr",
            "list",
            "--state",
            "open",
            "--limit",
            &limit,
            "--json",
            "number,url,isCrossRepository,headRefName,headRefOid,closingIssuesReferences",
        ];
        let value = self.json(&args)?;
        let prs = value.as_arr().ok_or_else(|| unexpected("gh pr list"))?;
        if prs.len() >= PR_LIMIT {
            return Err(Problem(format!(
                "this repository has {PR_LIMIT} open PRs or more, so one could be missing. Raise `PR_LIMIT`"
            )));
        }
        prs.iter()
            .map(|pr| read_open_pr(pr).ok_or_else(|| unexpected("gh pr list")))
            .collect()
    }

    /// Works on an issue or a PR, and on one that already has the label.
    pub fn apply_label(&self, number: u64, label: &str) -> Result<(), Problem> {
        let path = format!("{ISSUES}/{number}/labels");
        let field = format!("labels[]={label}");
        self.text(&["api", "-X", "POST", &path, "-f", &field], None)?;
        Ok(())
    }

    /// Works on an issue or a PR, and on one that doesn't have the label.
    pub fn remove_label(&self, number: u64, label: &str) -> Result<(), Problem> {
        let path = format!("{ISSUES}/{number}/labels/{label}");
        let args = ["api", "-X", "DELETE", &path];
        let output = self.output(&args, None)?;
        if !output.status.success() && String::from_utf8_lossy(&output.stderr).contains("HTTP 404")
        {
            return Ok(());
        }
        self.checked(&args, output)?;
        Ok(())
    }

    /// Sets a status on a commit.
    pub fn post_status(&self, commit: &str, context: &str, state: &str) -> Result<(), Problem> {
        let path = format!("repos/{{owner}}/{{repo}}/statuses/{commit}");
        let state = format!("state={state}");
        let context = format!("context={context}");
        self.text(
            &["api", "-X", "POST", &path, "-f", &state, "-f", &context],
            None,
        )?;
        Ok(())
    }

    pub fn comment_on_pr(&self, number: u64, body: &str) -> Result<(), Problem> {
        let number = number.to_string();
        self.text(&["pr", "comment", &number, "--body-file", "-"], Some(body))?;
        Ok(())
    }

    pub fn open_issues(&self) -> Result<Vec<OpenIssue>, Problem> {
        self.listed("open", "number,title,labels,isPinned")?
            .iter()
            .map(|item| read_open_issue(item).ok_or_else(|| unexpected("gh issue list")))
            .collect()
    }

    /// Returns the new issue's number.
    pub fn create_issue(&self, title: &str, body: &str) -> Result<u64, Problem> {
        let url = self.text(&["issue", "create", "--title", title, "--body", body], None)?;
        issue_number(url.trim()).ok_or_else(|| unexpected("gh issue create"))
    }

    /// Returns the new PR's number. It targets `main`, and the branch must
    /// already be pushed.
    pub fn create_pr(
        &self,
        branch: &str,
        title: &str,
        body: &str,
        label: Option<&str>,
    ) -> Result<u64, Problem> {
        let mut args = vec![
            "pr", "create", "--base", "main", "--head", branch, "--title", title, "--body", body,
        ];
        if let Some(label) = label {
            args.extend(["--label", label]);
        }
        let url = self.text(&args, None)?;
        url.trim()
            .rsplit_once("/pull/")
            .and_then(|(_, number)| number.parse().ok())
            .ok_or_else(|| unexpected("gh pr create"))
    }

    /// Whether this status is a success on this commit.
    pub fn status_passed(&self, commit: &str, context: &str) -> Result<bool, Problem> {
        let value = self.json(&[
            "api",
            &format!("repos/{{owner}}/{{repo}}/commits/{commit}/status"),
        ])?;
        let statuses = value
            .get("statuses")
            .and_then(Json::as_arr)
            .ok_or_else(|| unexpected("gh api"))?;
        Ok(statuses.iter().any(|status| {
            status.get("context").and_then(Json::as_str) == Some(context)
                && status.get("state").and_then(Json::as_str) == Some("success")
        }))
    }

    pub fn pin_issue(&self, number: u64) -> Result<(), Problem> {
        self.text(&["issue", "pin", &number.to_string()], None)?;
        Ok(())
    }

    fn listed(&self, state: &str, fields: &str) -> Result<Vec<Json>, Problem> {
        let limit = ISSUE_LIMIT.to_string();
        let args = [
            "issue", "list", "--state", state, "--limit", &limit, "--json", fields,
        ];
        let value = self.json(&args)?;
        let items = value.as_arr().ok_or_else(|| unexpected("gh issue list"))?;
        if items.len() >= ISSUE_LIMIT {
            return Err(Problem(format!(
                "this repository has {ISSUE_LIMIT} issues or more, so an open blocker could be missing. Raise `ISSUE_LIMIT`"
            )));
        }
        Ok(items.to_vec())
    }

    /// Squash-merges only if the branch is still at `head`. Naming the
    /// repository keeps `gh` away from the local branches. `gh` keeps the merge
    /// slot until it exits, because a squash that lands after the next merge
    /// took the slot would put that merge on a `main` it wasn't tested on.
    pub fn merge_pr(&self, number: u64, head: &str, slot: &Hold) -> Result<(), Problem> {
        let value = self.json(&["repo", "view", "--json", "nameWithOwner"])?;
        let name = value
            .get("nameWithOwner")
            .and_then(Json::as_str)
            .ok_or_else(|| unexpected("gh repo view"))?;
        let args = [
            "pr",
            "merge",
            &number.to_string(),
            "-R",
            name,
            "--squash",
            "--delete-branch",
            "--match-head-commit",
            head,
        ];
        self.checked(&args, self.spawned(&args, slot.as_stdin()?, None)?)?;
        Ok(())
    }
}

impl Forge for Gh {
    fn owner(&self) -> Result<String, Problem> {
        let value = self.json(&["repo", "view", "--json", "owner"])?;
        value
            .get("owner")
            .and_then(|owner| owner.get("login"))
            .and_then(Json::as_str)
            .map(str::to_string)
            .ok_or_else(|| unexpected("gh repo view"))
    }

    fn issues(&self) -> Result<Vec<Issue>, Problem> {
        self.listed("all", FIELDS)?
            .iter()
            .map(|item| read_issue(item).ok_or_else(|| unexpected("gh issue list")))
            .collect()
    }

    fn issue(&self, number: u64) -> Result<Issue, Problem> {
        let value = self.json(&["issue", "view", &number.to_string(), "--json", FIELDS])?;
        read_issue(&value).ok_or_else(|| unexpected("gh issue view"))
    }

    fn branch_date(&self, branch: &str) -> Result<Option<String>, Problem> {
        let path = format!("repos/{{owner}}/{{repo}}/branches/{branch}");
        let output = self.output(&["api", &path], None)?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() {
            if stderr.contains("HTTP 404") {
                return Ok(None);
            }
            return Err(Problem(format!(
                "`gh api {path}` failed: {}",
                stderr.trim()
            )));
        }
        let value = json::parse(&String::from_utf8_lossy(&output.stdout))
            .map_err(|e| Problem(format!("couldn't read `gh api {path}` output: {e}")))?;
        let date = ["commit", "commit", "committer", "date"]
            .iter()
            .try_fold(&value, |value, key| value.get(key))
            .and_then(Json::as_str)
            .ok_or_else(|| unexpected("gh api"))?;
        Ok(Some(date.to_string()))
    }

    fn add_label(&self, number: u64, label: &str) -> Result<(), Problem> {
        let number = number.to_string();
        self.text(&["issue", "edit", &number, "--add-label", label], None)?;
        Ok(())
    }

    fn post_comment(&self, number: u64, body: &str) -> Result<u64, Problem> {
        let number = number.to_string();
        let url = self.text(
            &["issue", "comment", &number, "--body-file", "-"],
            Some(body),
        )?;
        comment_id(url.trim()).ok_or_else(|| unexpected("gh issue comment"))
    }

    fn edit_comment(&self, id: u64, body: &str) -> Result<(), Problem> {
        let path = format!("repos/{{owner}}/{{repo}}/issues/comments/{id}");
        self.text(&["api", "-X", "PATCH", &path, "-F", "body=@-"], Some(body))?;
        Ok(())
    }
}

fn unexpected(command: &str) -> Problem {
    Problem(format!("`{command}` printed an unexpected shape"))
}

/// A comment's number is only in its URL, because `--json` gives a node id.
fn comment_id(url: &str) -> Option<u64> {
    url.rsplit_once("#issuecomment-")?.1.parse().ok()
}

/// An issue's number is the end of its URL.
fn issue_number(url: &str) -> Option<u64> {
    url.rsplit_once("/issues/")?.1.parse().ok()
}

fn read_labels(value: &Json) -> Option<Vec<String>> {
    value
        .get("labels")?
        .as_arr()?
        .iter()
        .map(|label| Some(label.get("name")?.as_str()?.to_string()))
        .collect()
}

fn read_open_issue(value: &Json) -> Option<OpenIssue> {
    Some(OpenIssue {
        number: value.get("number")?.as_u64()?,
        title: value.get("title")?.as_str()?.to_string(),
        labels: read_labels(value)?,
        pinned: value.get("isPinned")?.as_bool()?,
    })
}

fn read_issue(value: &Json) -> Option<Issue> {
    let comments = value.get("comments")?.as_arr()?;
    Some(Issue {
        number: value.get("number")?.as_u64()?,
        open: value.get("state")?.as_str()? == "OPEN",
        body: value.get("body")?.as_str()?.to_string(),
        labels: read_labels(value)?,
        comments: comments.iter().map(read_comment).collect::<Option<_>>()?,
    })
}

/// The issues in this repository that a PR closes. GitHub also lists issues
/// in other repositories, whose numbers mean nothing here.
fn closing_here(value: &Json) -> Option<Vec<u64>> {
    let url = value.get("url")?.as_str()?;
    let here = format!("{}/issues/", url.rsplit_once("/pull/")?.0);
    let is_here = |issue: &&Json| {
        let url = issue.get("url").and_then(Json::as_str);
        url.is_some_and(|url| url.starts_with(&here))
    };
    value
        .get("closingIssuesReferences")?
        .as_arr()?
        .iter()
        .filter(is_here)
        .map(|issue| issue.get("number")?.as_u64())
        .collect()
}

fn read_closing(value: &Json) -> Option<(u64, Vec<u64>)> {
    Some((value.get("number")?.as_u64()?, closing_here(value)?))
}

fn read_open_pr(value: &Json) -> Option<OpenPr> {
    Some(OpenPr {
        number: value.get("number")?.as_u64()?,
        from_fork: value.get("isCrossRepository")?.as_bool()?,
        head_ref: value.get("headRefName")?.as_str()?.to_string(),
        head: value.get("headRefOid")?.as_str()?.to_string(),
        closing_issues: closing_here(value)?,
    })
}

fn read_pull_request(value: &Json) -> Option<PullRequest> {
    let text = |key: &str| Some(value.get(key)?.as_str()?.to_string());
    Some(PullRequest {
        state: text("state")?,
        from_fork: value.get("isCrossRepository")?.as_bool()?,
        head_ref: text("headRefName")?,
        base_ref: text("baseRefName")?,
        body: text("body")?,
        labels: read_labels(value)?,
        closing_issues: closing_here(value)?,
        changes_requested: has_standing_request(value.get("reviews")?.as_arr()?)?,
    })
}

/// Whether a reviewer's latest decision is a change request. GitHub's own
/// summary of this is empty on a repository with no required reviews. A comment
/// decides nothing, so it leaves an earlier request standing.
fn has_standing_request(reviews: &[Json]) -> Option<bool> {
    let mut latest = HashMap::new();
    for review in reviews {
        // A deleted account has no author.
        let author = review
            .get("author")
            .and_then(|author| author.get("login"))
            .and_then(Json::as_str)
            .unwrap_or_default();
        let state = review.get("state")?.as_str()?;
        if state != "COMMENTED" && state != "PENDING" {
            latest.insert(author, state);
        }
    }
    Some(latest.values().any(|state| *state == "CHANGES_REQUESTED"))
}

fn read_comment(value: &Json) -> Option<Comment> {
    Some(Comment {
        id: comment_id(value.get("url")?.as_str()?)?,
        // A deleted account has no author.
        author: value
            .get("author")
            .and_then(|author| author.get("login"))
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_string(),
        body: value.get("body")?.as_str()?.to_string(),
    })
}
