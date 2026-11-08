//! What the harness reads from GitHub and writes to it.

use crate::Problem;
use crate::json::{self, Json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const FIELDS: &str = "number,state,body,labels,comments";
const ISSUE_LIMIT: usize = 2000;
/// `gh issue list` gives at most this many comments per issue.
pub const LISTED_COMMENTS: usize = 100;

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

/// A PR, as far as the merge sequence needs one.
pub struct PullRequest {
    pub state: String,
    pub head_ref: String,
    pub base_ref: String,
    pub body: String,
    pub labels: Vec<String>,
    /// Empty when no review has decided anything.
    pub review_decision: String,
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

    fn output(&self, args: &[&str], input: Option<&str>) -> Result<std::process::Output, Problem> {
        let couldnt = |e: std::io::Error| Problem(format!("couldn't run `gh`: {e}"));
        let mut child = Command::new("gh")
            .args(args)
            .current_dir(&self.root)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(couldnt)?;
        if let Some(input) = input {
            let mut stdin = child.stdin.take().expect("stdin is piped");
            stdin.write_all(input.as_bytes()).map_err(couldnt)?;
        }
        child.wait_with_output().map_err(couldnt)
    }

    fn text(&self, args: &[&str], input: Option<&str>) -> Result<String, Problem> {
        let output = self.output(args, input)?;
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
            "state,headRefName,baseRefName,body,labels,reviewDecision",
        ])?;
        read_pull_request(&value).ok_or_else(|| unexpected("gh pr view"))
    }

    /// Sets the one check this repository has on a commit.
    pub fn post_status(&self, commit: &str, state: &str) -> Result<(), Problem> {
        let path = format!("repos/{{owner}}/{{repo}}/statuses/{commit}");
        let state = format!("state={state}");
        self.text(
            &[
                "api",
                "-X",
                "POST",
                &path,
                "-f",
                &state,
                "-f",
                "context=xtask/gate",
            ],
            None,
        )?;
        Ok(())
    }

    /// Squash-merges only if the branch is still at `head`. Naming the
    /// repository keeps `gh` away from the local branches.
    pub fn merge_pr(&self, number: u64, head: &str) -> Result<(), Problem> {
        let value = self.json(&["repo", "view", "--json", "nameWithOwner"])?;
        let name = value
            .get("nameWithOwner")
            .and_then(Json::as_str)
            .ok_or_else(|| unexpected("gh repo view"))?;
        self.text(
            &[
                "pr",
                "merge",
                &number.to_string(),
                "-R",
                name,
                "--squash",
                "--delete-branch",
                "--match-head-commit",
                head,
            ],
            None,
        )?;
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
        let limit = ISSUE_LIMIT.to_string();
        let value = self.json(&[
            "issue", "list", "--state", "all", "--limit", &limit, "--json", FIELDS,
        ])?;
        let items = value.as_arr().ok_or_else(|| unexpected("gh issue list"))?;
        if items.len() >= ISSUE_LIMIT {
            return Err(Problem(format!(
                "this repository has {ISSUE_LIMIT} issues or more, so an open blocker could be missing. Raise `ISSUE_LIMIT`"
            )));
        }
        items
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

fn read_issue(value: &Json) -> Option<Issue> {
    let labels = value.get("labels")?.as_arr()?;
    let comments = value.get("comments")?.as_arr()?;
    Some(Issue {
        number: value.get("number")?.as_u64()?,
        open: value.get("state")?.as_str()? == "OPEN",
        body: value.get("body")?.as_str()?.to_string(),
        labels: labels
            .iter()
            .map(|label| Some(label.get("name")?.as_str()?.to_string()))
            .collect::<Option<_>>()?,
        comments: comments.iter().map(read_comment).collect::<Option<_>>()?,
    })
}

fn read_pull_request(value: &Json) -> Option<PullRequest> {
    let text = |key: &str| Some(value.get(key)?.as_str()?.to_string());
    Some(PullRequest {
        state: text("state")?,
        head_ref: text("headRefName")?,
        base_ref: text("baseRefName")?,
        body: text("body")?,
        labels: value
            .get("labels")?
            .as_arr()?
            .iter()
            .map(|label| Some(label.get("name")?.as_str()?.to_string()))
            .collect::<Option<_>>()?,
        review_decision: text("reviewDecision")?,
    })
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
