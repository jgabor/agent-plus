//! Runs `remove` in a worktree of a scratch repository, the way step 9 of the
//! worker brief does.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const BRANCH: &str = "7-x";

struct Repo {
    main: PathBuf,
    worktree: PathBuf,
}

impl Repo {
    /// A main checkout with one commit, and a worktree of it on `BRANCH`, with
    /// a tracked `Cargo.toml` and an ignored `target/`.
    fn new(name: &str) -> Self {
        let top = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("remove-{name}"));
        let _ = fs::remove_dir_all(&top);
        let main = top.join("main");
        fs::create_dir_all(&main).unwrap();
        let repo = Self {
            worktree: top.join(BRANCH),
            main,
        };
        repo.git(&repo.main, &["init", "-q", "-b", "main"]);
        fs::write(repo.main.join("Cargo.toml"), "[workspace]\n").unwrap();
        fs::write(repo.main.join(".gitignore"), "/target\n").unwrap();
        repo.git(&repo.main, &["add", "."]);
        repo.git(&repo.main, &["commit", "-q", "-m", "x"]);
        repo.git(
            &repo.main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                BRANCH,
                &format!("../{BRANCH}"),
            ],
        );
        let target = repo.worktree.join("target/debug");
        fs::create_dir_all(&target).unwrap();
        for n in 0..2000 {
            fs::write(target.join(n.to_string()), "").unwrap();
        }
        repo
    }

    fn command(&self, program: &str, dir: &Path) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(dir)
            .env("HOME", self.main.parent().unwrap())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "x")
            .env("GIT_AUTHOR_EMAIL", "x@x")
            .env("GIT_COMMITTER_NAME", "x")
            .env("GIT_COMMITTER_EMAIL", "x@x");
        command
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let output = self.command("git", dir).args(args).output().unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn remove(&self) -> Output {
        self.command(env!("CARGO_BIN_EXE_xtask"), &self.worktree)
            .arg("remove")
            .output()
            .unwrap()
    }

    fn has_branch(&self) -> bool {
        !self
            .git(&self.main, &["branch", "--list", BRANCH])
            .trim()
            .is_empty()
    }

    fn is_listed(&self) -> bool {
        self.git(&self.main, &["worktree", "list", "--porcelain"])
            .lines()
            .any(|line| line == format!("branch refs/heads/{BRANCH}"))
    }
}

fn describe(output: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Writes `target/flycheck0/` as rust-analyzer's check does, while git still
/// records the worktree, and for a moment after, or until told to stop.
fn writer(repo: &Repo) -> (Arc<AtomicBool>, thread::JoinHandle<u64>) {
    let started = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let (begun, halt) = (Arc::clone(&started), Arc::clone(&stop));
    let record = repo.main.join(".git/worktrees").join(BRANCH);
    let check = repo.worktree.join("target/flycheck0");
    let handle = thread::spawn(move || {
        let mut writes = 0;
        let mut gone: Option<Instant> = None;
        while !halt.load(Ordering::SeqCst)
            && gone.is_none_or(|at| at.elapsed() < Duration::from_millis(100))
        {
            if fs::create_dir_all(&check).is_ok()
                && fs::write(check.join("stderr"), "error").is_ok()
            {
                writes += 1;
            }
            begun.store(true, Ordering::SeqCst);
            if gone.is_none() && !record.exists() {
                gone = Some(Instant::now());
            }
        }
        writes
    });
    while !started.load(Ordering::SeqCst) {
        thread::yield_now();
    }
    (stop, handle)
}

#[test]
fn remove_deletes_the_worktree_and_branch_while_something_writes_to_target() {
    let repo = Repo::new("writer");
    let (stop, handle) = writer(&repo);
    let output = repo.remove();
    stop.store(true, Ordering::SeqCst);
    let writes = handle.join().unwrap();
    assert!(writes > 0);
    assert!(output.status.success(), "{}", describe(&output));
    assert!(!repo.worktree.exists(), "{}", describe(&output));
    assert!(!repo.is_listed());
    assert!(!repo.has_branch(), "{}", describe(&output));
}

#[test]
fn remove_deletes_a_clean_worktree_and_its_branch() {
    let repo = Repo::new("clean");
    let output = repo.remove();
    assert!(output.status.success(), "{}", describe(&output));
    assert!(!repo.worktree.exists());
    assert!(!repo.is_listed());
    assert!(!repo.has_branch());
}

#[test]
fn remove_keeps_a_worktree_with_modified_files_and_its_branch() {
    let repo = Repo::new("modified");
    fs::write(repo.worktree.join("Cargo.toml"), "[workspace]\n# x\n").unwrap();
    let output = repo.remove();
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    assert!(repo.worktree.join("Cargo.toml").exists());
    assert!(repo.is_listed());
    assert!(repo.has_branch());
}

#[test]
fn remove_keeps_a_worktree_with_untracked_files_and_its_branch() {
    let repo = Repo::new("untracked");
    fs::write(repo.worktree.join("new.rs"), "").unwrap();
    let output = repo.remove();
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    assert!(repo.worktree.join("new.rs").exists());
    assert!(repo.is_listed());
    assert!(repo.has_branch());
}

#[test]
fn remove_refuses_the_main_checkout() {
    let repo = Repo::new("main");
    let output = repo
        .command(env!("CARGO_BIN_EXE_xtask"), &repo.main)
        .arg("remove")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{}", describe(&output));
    assert!(repo.main.join(".git").exists());
    assert!(repo.worktree.exists());
}

#[test]
fn remove_deletes_only_its_own_worktree_when_a_name_differs_by_a_space() {
    let repo = Repo::new("space");
    let spaced = repo.worktree.with_file_name(format!("{BRANCH} "));
    repo.git(
        &repo.main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "8-y",
            &spaced.to_string_lossy(),
        ],
    );
    let output = repo
        .command(env!("CARGO_BIN_EXE_xtask"), &spaced)
        .arg("remove")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", describe(&output));
    assert!(!spaced.exists());
    assert!(repo.worktree.join("Cargo.toml").exists());
    assert!(repo.is_listed());
    assert!(repo.has_branch());
}
