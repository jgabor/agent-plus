//! `remove`: deletes the worktree it runs in, and its local branch, for step 9
//! of the worker brief.
//!
//! `git worktree remove` refuses a worktree with modified or untracked files
//! before it deletes anything, and that refusal stands. Once it starts, it can
//! still fail with `Directory not empty`, because rust-analyzer sees
//! `Cargo.toml` go and starts a check that writes `target/flycheck0/`. Git
//! then forgets the worktree and leaves the directory. This command deletes
//! what is left, at the path git recorded before the removal.

use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use crate::base::git;
use crate::{Problem, Verdict};

/// How many times to delete what is left, because the writer can come back.
const ATTEMPTS: usize = 10;

/// How long the directory must stay gone before it counts as gone.
const WAIT: Duration = Duration::from_millis(200);

#[derive(Debug, PartialEq, Eq)]
struct Worktree {
    path: PathBuf,
    branch: Option<String>,
}

/// Reads `git worktree list --porcelain -z`. The main worktree comes first.
fn parse(listing: &str) -> Vec<Worktree> {
    let mut worktrees = Vec::new();
    for field in listing.split('\0') {
        if let Some(path) = field.strip_prefix("worktree ") {
            worktrees.push(Worktree {
                path: PathBuf::from(path),
                branch: None,
            });
        } else if let Some(branch) = field.strip_prefix("branch refs/heads/")
            && let Some(last) = worktrees.last_mut()
        {
            last.branch = Some(branch.to_string());
        }
    }
    worktrees
}

/// Runs git, and refuses output that isn't UTF-8. A lossy decoding could turn
/// two different paths or branches into the same one.
fn text(dir: &Path, args: &[&str]) -> Result<String, Problem> {
    let output = git(dir, args)?;
    if !output.status.success() {
        return Err(Problem(format!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    String::from_utf8(output.stdout).map_err(|_| {
        Problem(format!(
            "`git {}` printed a path or branch that isn't UTF-8, which \
             `remove` doesn't handle",
            args.join(" ")
        ))
    })
}

fn list(dir: &Path) -> Result<Vec<Worktree>, Problem> {
    Ok(parse(&text(
        dir,
        &["worktree", "list", "--porcelain", "-z"],
    )?))
}

fn same(first: &Path, second: &Path) -> bool {
    match (first.canonicalize(), second.canonicalize()) {
        (Ok(first), Ok(second)) => first == second,
        _ => first == second,
    }
}

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// Deletes `path` until it stays gone for one wait.
fn delete(path: &Path) -> Result<(), String> {
    let mut last = None;
    for _ in 0..ATTEMPTS {
        if exists(path)
            && let Err(e) = fs::remove_dir_all(path)
        {
            last = Some(e);
        }
        thread::sleep(WAIT);
        if !exists(path) {
            return Ok(());
        }
    }
    Err(last.map_or_else(
        || "something kept writing to it".to_string(),
        |e| e.to_string(),
    ))
}

pub fn run() -> Result<Verdict, Problem> {
    let here = std::env::current_dir()
        .map_err(|e| Problem(format!("couldn't read the current directory: {e}")))?;
    let top = text(&here, &["rev-parse", "--show-toplevel"])?;
    let top = PathBuf::from(top.strip_suffix('\n').unwrap_or(&top));
    let worktrees = list(&here)?;
    let Some(main) = worktrees.first() else {
        return Err(Problem("git lists no worktrees".to_string()));
    };
    let Some(this) = worktrees.iter().find(|w| same(&w.path, &top)) else {
        return Err(Problem(format!(
            "git doesn't list {} as a worktree",
            top.display()
        )));
    };
    if std::ptr::eq(this, main) || main.path.starts_with(&this.path) {
        return Err(Problem(format!(
            "{} is the main checkout, which `remove` never deletes",
            this.path.display()
        )));
    }
    let Some(branch) = this.branch.clone() else {
        return Err(Problem(format!(
            "{} has no branch checked out",
            this.path.display()
        )));
    };
    let path = this.path.clone();
    let main = main.path.clone();

    let removal = git(&main, &["worktree", "remove", &path.to_string_lossy()])?;
    if !removal.status.success() {
        let why = String::from_utf8_lossy(&removal.stderr).trim().to_string();
        if list(&main)?.iter().any(|w| same(&w.path, &path)) {
            eprintln!(
                "git kept {} and branch {branch}, which may hold changes that \
                 didn't merge: {why}",
                path.display()
            );
            return Ok(Verdict::Fail);
        }
        eprintln!(
            "git forgot {} but left some of it: {why}. Deleting what is left",
            path.display()
        );
    }
    if let Err(why) = delete(&path) {
        eprintln!(
            "couldn't delete {}, so branch {branch} stays: {why}",
            path.display()
        );
        return Ok(Verdict::Fail);
    }
    text(&main, &["branch", "-D", &branch])?;
    println!("removed {} and branch {branch}", path.display());
    Ok(Verdict::Pass)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reads_paths_and_branches_in_order() {
        let listing = "worktree /r/main\0HEAD 1\0branch refs/heads/main\0\0\
                       worktree /r/a b\0HEAD 2\0detached\0\0\
                       worktree /r/7-x\0HEAD 3\0branch refs/heads/7-x\0prunable gone\0\0";
        assert_eq!(
            parse(listing),
            vec![
                Worktree {
                    path: PathBuf::from("/r/main"),
                    branch: Some("main".to_string()),
                },
                Worktree {
                    path: PathBuf::from("/r/a b"),
                    branch: None,
                },
                Worktree {
                    path: PathBuf::from("/r/7-x"),
                    branch: Some("7-x".to_string()),
                },
            ]
        );
    }
}
