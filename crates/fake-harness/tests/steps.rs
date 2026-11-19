//! The steps a prompt asks of the fake harness: putting in, changing, and
//! deleting files, and committing. The `git` on the host is the oracle for
//! every commit.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn empty_directory(name: &str) -> PathBuf {
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("steps")
        .join(name);
    if directory.exists() {
        fs::remove_dir_all(&directory).unwrap();
    }
    fs::create_dir_all(&directory).unwrap();
    directory
}

/// Runs the fake harness in `directory` headless, with `prompt` last, as the
/// Claude Code adapter passes it.
fn harness(directory: &Path, prompt: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fake-harness"))
        .args(["-p", "--", prompt])
        .current_dir(directory)
        .env_clear()
        .output()
        .unwrap()
}

fn steps(directory: &Path, prompt: &str) {
    let output = harness(directory, prompt);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Runs the host's `git` in `directory`, away from any config of yours, and
/// returns what it prints.
fn git(directory: &Path, args: &[&str]) -> String {
    let home = directory.parent().unwrap().join(format!(
        "{}.home",
        directory.file_name().unwrap().to_string_lossy()
    ));
    fs::create_dir_all(&home).unwrap();
    let output = Command::new("git")
        .args(["-c", "user.name=You", "-c", "user.email=you@localhost"])
        .args(["-c", "init.defaultBranch=main"])
        .args(args)
        .current_dir(directory)
        .env("HOME", &home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn it_puts_in_changes_and_deletes_files() {
    let directory = empty_directory("files");
    fs::write(directory.join("changed.txt"), "yours\n").unwrap();
    fs::write(directory.join("deleted.txt"), "yours\n").unwrap();
    steps(
        &directory,
        "put new/added.txt one two\nput changed.txt agent's\ndelete deleted.txt",
    );
    assert_eq!(
        fs::read_to_string(directory.join("new/added.txt")).unwrap(),
        "one two\n"
    );
    assert_eq!(
        fs::read_to_string(directory.join("changed.txt")).unwrap(),
        "agent's\n"
    );
    assert!(!directory.join("deleted.txt").exists());
}

#[test]
fn it_takes_no_step_from_a_prompt_without_one() {
    let directory = empty_directory("none");
    steps(&directory, "Say what you can see.\nThen put nothing away.");
    assert_eq!(fs::read_dir(&directory).unwrap().count(), 0);
}

#[test]
fn it_reports_before_it_takes_a_step() {
    let directory = empty_directory("report");
    let output = harness(&directory, "put a.txt a");
    let out = String::from_utf8(output.stdout).unwrap();
    assert!(out.contains(r#""files":[]"#), "{out}");
}

#[test]
fn it_fails_a_step_it_cant_take() {
    let directory = empty_directory("fails");
    for prompt in [
        "delete missing.txt",
        "put a.txt",
        "commit Nothing to commit",
    ] {
        let output = harness(&directory, prompt);
        assert!(!output.status.success(), "{prompt}");
        assert!(
            String::from_utf8_lossy(&output.stderr).starts_with("fake-harness: "),
            "{prompt}"
        );
    }
}

#[test]
fn it_commits_what_it_changed_on_top_of_head() {
    let directory = empty_directory("commit");
    git(&directory, &["init", "-q"]);
    fs::create_dir_all(directory.join("src")).unwrap();
    fs::write(directory.join("src/kept.txt"), "kept\n").unwrap();
    fs::write(directory.join("src/deleted.txt"), "yours\n").unwrap();
    fs::write(directory.join("changed.txt"), "yours\n").unwrap();
    git(&directory, &["add", "."]);
    git(&directory, &["commit", "-qm", "Start"]);
    let start = git(&directory, &["rev-parse", "HEAD"]);

    steps(
        &directory,
        "put src/new/added.txt added\nput changed.txt agent's\n\
         delete src/deleted.txt\ncommit Change some files\n\
         put later.txt not committed",
    );

    assert_eq!(
        git(&directory, &["log", "--format=%s"]),
        "Change some files\nStart\n"
    );
    assert_eq!(git(&directory, &["rev-parse", "HEAD~1"]), start);
    assert_eq!(
        git(&directory, &["ls-tree", "-r", "--name-only", "HEAD"]),
        "changed.txt\nsrc/kept.txt\nsrc/new/added.txt\n"
    );
    assert_eq!(git(&directory, &["show", "HEAD:changed.txt"]), "agent's\n");
    assert_eq!(git(&directory, &["show", "HEAD:src/kept.txt"]), "kept\n");
    // What the prompt puts in after the commit isn't in it.
    assert!(git(&directory, &["status", "--porcelain"]).contains("?? later.txt"));
    git(&directory, &["fsck", "--strict", "--no-dangling"]);
}

#[test]
fn it_commits_again_on_its_own_commit() {
    let directory = empty_directory("twice");
    git(&directory, &["init", "-q"]);
    steps(
        &directory,
        "put a.txt a\ncommit First\nput b.txt b\ndelete a.txt\ncommit Second",
    );
    assert_eq!(git(&directory, &["log", "--format=%s"]), "Second\nFirst\n");
    assert_eq!(
        git(&directory, &["ls-tree", "-r", "--name-only", "HEAD"]),
        "b.txt\n"
    );
    assert_eq!(
        git(&directory, &["ls-tree", "-r", "--name-only", "HEAD~1"]),
        "a.txt\n"
    );
    git(&directory, &["fsck", "--strict", "--no-dangling"]);
}
