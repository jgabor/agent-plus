//! `ap apply`, from SPEC.md's `SHARE` section, for a share whose root holds
//! no `.git` directory. Nothing here boots: a test writes a run's pending
//! directory the way the share server does, as its `cow` module describes,
//! and makes a run active the way a run process does, by holding the lock on
//! its record.

mod common;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use store::Runs;

/// A test's own directory: a home with a store, and `share`, the directory
/// the agent shared.
struct Scratch {
    root: PathBuf,
    home: PathBuf,
    share: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        Scratch::sharing(name, "share")
    }

    /// A scratch whose shared directory is `share`, beneath its root.
    fn sharing(name: &str, share: &str) -> Scratch {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("apply")
            .join(name);
        writable(&root);
        let _ = fs::remove_dir_all(&root);
        let home = root.join("home");
        let share = root.join(share);
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&share).unwrap();
        common::seed_os(&home.join(".local/share/ap"));
        Scratch {
            root: root.canonicalize().unwrap(),
            home: home.canonicalize().unwrap(),
            share: share.canonicalize().unwrap(),
        }
    }

    fn store(&self) -> PathBuf {
        self.home.join(".local/share/ap")
    }

    fn runs(&self) -> Runs {
        Runs::new(self.store())
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ap"));
        command
            .args(args)
            .current_dir(&self.root)
            .env_clear()
            .env("HOME", &self.home)
            .stdin(Stdio::null());
        command
    }

    /// Runs `ap` with only `HOME` in its environment and no terminal.
    fn ap(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// Builds an agent named `name`, which the store then has.
    fn agent(&self, name: &str) {
        let dir = self.root.join(name);
        fs::create_dir_all(&dir).unwrap();
        let agentfile = "AGENT claude\nOS minimal\nNETWORK -\nPOLICY -\n";
        fs::write(dir.join("Agentfile"), agentfile).unwrap();
        common::pin_harness(&self.home, &dir);
        let grants = self.root.join("grants");
        fs::write(&grants, "NETWORK -\nPOLICY -\n").unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ap"))
            .args(["build", &format!("--approve={}", grants.display())])
            .current_dir(&dir)
            .env_clear()
            .env("HOME", &self.home)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    }

    /// Records a run of `agent` with a copy-on-write share of `root`, and
    /// returns its name and the share's pending directory.
    fn run_sharing(&self, agent: &str, started: u128, root: &Path) -> (String, PathBuf) {
        let runs = self.runs();
        let run = runs.create(agent, started).unwrap();
        runs.create_pending(&run.name).unwrap();
        let pending = runs.create_pending_share(&run.name, root).unwrap();
        fs::create_dir(pending.join("tree")).unwrap();
        fs::write(pending.join("records"), "").unwrap();
        (run.name, pending)
    }

    /// Records a run of `agent` that shares `share`.
    fn run(&self, agent: &str, started: u128) -> (String, PathBuf) {
        self.run_sharing(agent, started, &self.share)
    }

    fn read(&self, path: &str) -> String {
        fs::read_to_string(self.share.join(path)).unwrap()
    }

    fn exists(&self, path: &str) -> bool {
        fs::symlink_metadata(self.share.join(path)).is_ok()
    }

    /// The names in the share's directory `path`, in order.
    fn names(&self, path: &str) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.share.join(path))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }
}

/// Makes every directory at or beneath `path` one you can write in, so an
/// earlier run of a test that left a read-only one can be removed.
fn writable(path: &Path) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if metadata.is_dir() {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
        for entry in fs::read_dir(path).into_iter().flatten().flatten() {
            writable(&entry.path());
        }
    }
}

/// Puts the agent's copy of `path` in the tree, and records it.
fn write(pending: &Path, path: &str, mode: u32, data: &[u8]) {
    let file = pending.join("tree").join(path);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, data).unwrap();
    record(pending, &format!("FILE \"{path}\" {mode:o}"));
}

/// Records that the agent changed your file at `path`, as the share server
/// does before it first changes it, and puts the agent's copy in the tree.
fn change(share: &Path, pending: &Path, path: &str, mode: u32, data: &[u8]) {
    record(pending, &base(share, path));
    write(pending, path, mode, data);
}

/// Records that the agent deleted your file at `path`.
fn delete(share: &Path, pending: &Path, path: &str) {
    record(pending, &base(share, path));
    record(pending, &format!("DELETE \"{path}\""));
}

/// Records a directory the agent made, and puts it in the tree.
fn dir(pending: &Path, path: &str, mode: u32) {
    fs::create_dir_all(pending.join("tree").join(path)).unwrap();
    record(pending, &format!("DIR \"{path}\" {mode:o}"));
}

/// Records a symlink the agent made, with the empty file the share server
/// keeps for it in the tree.
fn link(pending: &Path, path: &str, target: &str) {
    fs::write(pending.join("tree").join(path), "").unwrap();
    record(pending, &format!("SYMLINK \"{path}\" \"{target}\""));
}

/// Adds a line to a share's records, in one append, as the share server
/// does.
fn record(pending: &Path, line: &str) {
    use std::io::Write;
    let mut records = fs::OpenOptions::new()
        .append(true)
        .open(pending.join("records"))
        .unwrap();
    records.write_all(format!("{line}\n").as_bytes()).unwrap();
}

/// The `BASE` line the share server writes for your file at `path`, as it
/// is now.
fn base(share: &Path, path: &str) -> String {
    use std::os::unix::fs::MetadataExt;
    match fs::symlink_metadata(share.join(path)) {
        Err(_) => format!("BASE \"{path}\" none"),
        Ok(m) => format!(
            "BASE \"{path}\" {} {} {}.{:09} {}.{:09}",
            m.ino(),
            m.size(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec()
        ),
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
}

// spec(SHARE): `ap apply <agent | run>` brings them back.
// spec(SHARE): Otherwise, changed, added, and deleted files are applied to your files, even if a repository lies above or below the share.
#[test]
fn apply_lands_changed_added_and_deleted_files() {
    // A repository lies above the share.
    let scratch = Scratch::sharing("files", "repository/share");
    fs::create_dir(scratch.root.join("repository/.git")).unwrap();
    let share = &scratch.share;
    fs::write(share.join("changed.txt"), "one\n").unwrap();
    fs::write(share.join("deleted.txt"), "gone\n").unwrap();
    fs::write(share.join("same.txt"), "same\n").unwrap();
    fs::write(share.join("script"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(share.join("script"), fs::Permissions::from_mode(0o644)).unwrap();
    fs::create_dir_all(share.join("docs/deep")).unwrap();
    fs::write(share.join("docs/a.md"), "a\n").unwrap();
    fs::write(share.join("docs/deep/b.md"), "b\n").unwrap();
    // And one lies below it.
    fs::create_dir_all(share.join("vendor/lib/.git")).unwrap();
    fs::write(share.join("vendor/lib/file"), "old\n").unwrap();
    fs::write(share.join("vendor/lib/.git/HEAD"), "ref: refs/heads/main\n").unwrap();
    let before = fs::metadata(share.join("same.txt"))
        .unwrap()
        .modified()
        .unwrap();
    let (run, pending) = scratch.run("notes", 1);
    change(share, &pending, "changed.txt", 0o644, b"two\n");
    delete(share, &pending, "deleted.txt");
    change(share, &pending, "same.txt", 0o644, b"same\n");
    change(share, &pending, "script", 0o755, b"#!/bin/sh\n");
    record(&pending, &base(share, "new"));
    dir(&pending, "new", 0o750);
    write(&pending, "new/added.txt", 0o600, b"added\n");
    record(&pending, &base(share, "latest"));
    link(&pending, "latest", "new/added.txt");
    for path in ["docs/a.md", "docs/deep/b.md", "docs/deep", "docs"] {
        delete(share, &pending, path);
    }
    change(share, &pending, "vendor/lib/file", 0o644, b"new\n");

    let output = scratch.ap(&["apply", &run]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");
    assert_eq!(stderr(&output), "");
    assert_eq!(scratch.read("changed.txt"), "two\n");
    assert!(!scratch.exists("deleted.txt"));
    assert!(!scratch.exists("docs"));
    assert_eq!(scratch.read("new/added.txt"), "added\n");
    assert_eq!(mode(&share.join("new/added.txt")), 0o600);
    assert_eq!(mode(&share.join("script")), 0o755);
    assert_eq!(
        fs::read_link(share.join("latest")).unwrap(),
        Path::new("new/added.txt")
    );
    assert_eq!(scratch.read("vendor/lib/file"), "new\n");
    // A file the agent left as it was isn't written again.
    let after = fs::metadata(share.join("same.txt"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(before, after);
    // Nothing is left beside your files, and nothing stays pending.
    assert_eq!(
        scratch.names(""),
        [
            "changed.txt",
            "latest",
            "new",
            "same.txt",
            "script",
            "vendor"
        ]
    );
    assert_eq!(scratch.names("new"), ["added.txt"]);
    assert!(!scratch.store().join("pending").join(&run).exists());
    let output = scratch.ap(&["diff", &run]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");
}

// spec(SHARE): A file that changed on your side after the agent first wrote it is not applied, and `ap apply` reports it.
// spec(SHARE): After `ap apply`, the only changes still pending are files it couldn't apply, because they changed on your side too or a write failed, and `ap discard` removes them.
// spec(SHARE): `ap apply` exits 1 if anything stays pending.
#[test]
fn a_file_you_changed_too_is_reported_and_stays_pending() {
    let scratch = Scratch::new("conflict");
    let share = &scratch.share;
    fs::write(share.join("mine.txt"), "mine\n").unwrap();
    fs::create_dir(share.join("docs")).unwrap();
    fs::write(share.join("docs/old"), "old\n").unwrap();
    let (run, pending) = scratch.run("notes", 1);
    change(share, &pending, "mine.txt", 0o644, b"the agent's\n");
    write(&pending, "other.txt", 0o644, b"other\n");
    delete(share, &pending, "docs/old");
    delete(share, &pending, "docs");
    // You change your file, and add one to the directory the agent deleted.
    fs::write(share.join("mine.txt"), "yours, again\n").unwrap();
    fs::write(share.join("docs/new"), "new\n").unwrap();

    let output = scratch.ap(&["apply", &run]);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stdout(&output),
        concat!(
            "skipped docs/new: it changed on your side after the agent first changed it\n",
            "skipped mine.txt: it changed on your side after the agent first changed it\n",
        )
    );
    assert_eq!(
        stderr(&output),
        format!("ap: 2 files stay pending in run {run}, which `ap diff {run}` shows\n")
    );
    assert_eq!(scratch.read("mine.txt"), "yours, again\n");
    assert_eq!(scratch.read("other.txt"), "other\n");
    assert_eq!(scratch.read("docs/new"), "new\n");
    assert!(!scratch.exists("docs/old"));
    // Only those two stay, and stay as they were.
    let output = scratch.ap(&["diff", &run]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        concat!(
            "skipped docs/new: it changed on your side after the agent first changed it\n",
            "skipped mine.txt: it changed on your side after the agent first changed it\n",
        )
    );
    let output = scratch.ap(&["apply", &run]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(scratch.read("mine.txt"), "yours, again\n");

    let output = scratch.ap(&["discard", &run]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!scratch.store().join("pending").join(&run).exists());
    assert_eq!(scratch.read("mine.txt"), "yours, again\n");
}

/// Only a file `ap apply` itself landed is exempt from the check, not one
/// you changed to match the agent's, and not one it can't read.
#[test]
fn a_file_you_changed_to_the_agent_s_or_can_t_read_is_still_reported() {
    let scratch = Scratch::new("conflict-same");
    let share = &scratch.share;
    fs::write(share.join("same"), "one\n").unwrap();
    fs::write(share.join("locked"), "one\n").unwrap();
    let (run, pending) = scratch.run("notes", 1);
    change(share, &pending, "same", 0o644, b"two\n");
    change(share, &pending, "locked", 0o644, b"two\n");
    write(&pending, "other", 0o644, b"other\n");
    fs::write(share.join("same"), "two\n").unwrap();
    fs::write(share.join("locked"), "yours\n").unwrap();
    fs::set_permissions(share.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();

    let output = scratch.ap(&["apply", &run]);

    fs::set_permissions(share.join("locked"), fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        concat!(
            "skipped locked: it changed on your side after the agent first changed it\n",
            "skipped same: it changed on your side after the agent first changed it\n",
        )
    );
    assert_eq!(scratch.read("locked"), "yours\n");
    assert_eq!(scratch.read("other"), "other\n");
}

// spec(SHARE): Nothing is ever applied under a path component that matches `.git` case-insensitively, in any repository, including nested ones.
#[test]
fn nothing_is_applied_under_git() {
    let scratch = Scratch::new("git");
    let share = &scratch.share;
    fs::create_dir_all(share.join("vendor/lib/.git")).unwrap();
    fs::write(share.join("vendor/lib/.git/HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::create_dir_all(share.join("old/lib/.git/refs/heads")).unwrap();
    fs::write(share.join("old/lib/.git/HEAD"), "ref: refs/heads/main\n").unwrap();
    let (run, pending) = scratch.run("notes", 1);
    change(share, &pending, "vendor/lib/.git/HEAD", 0o644, b"0000\n");
    // The agent deletes a directory that holds a repository.
    delete(share, &pending, "old");
    write(
        &pending,
        "vendor/lib/.git/hooks/pre-commit",
        0o755,
        b"#!/bin/sh\n",
    );
    write(&pending, "vendor/lib/.GIT/config", 0o644, b"[core]\n");
    record(&pending, &base(share, "sub"));
    dir(&pending, "sub", 0o755);
    write(&pending, "sub/.Git", 0o644, b"gitdir: /etc\n");

    let output = scratch.ap(&["apply", &run]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        concat!(
            "skipped old/lib/.git/HEAD: it is under .git\n",
            "skipped sub/.Git: it is under .git\n",
            "skipped vendor/lib/.GIT/config: it is under .git\n",
            "skipped vendor/lib/.git/HEAD: it is under .git\n",
            "skipped vendor/lib/.git/hooks/pre-commit: it is under .git\n",
        )
    );
    assert_eq!(
        scratch.read("vendor/lib/.git/HEAD"),
        "ref: refs/heads/main\n"
    );
    assert_eq!(scratch.names("vendor/lib"), [".git"]);
    assert_eq!(scratch.names("vendor/lib/.git"), ["HEAD"]);
    assert!(!scratch.exists("sub/.Git"));
    assert_eq!(scratch.names("old/lib/.git"), ["HEAD", "refs"]);
    assert_eq!(scratch.names("old/lib/.git/refs"), ["heads"]);
    // Left out on purpose, so none of it stays pending.
    assert!(!scratch.store().join("pending").join(&run).exists());
}

// spec(SHARE): Symlinks are never followed.
// spec(SHARE): A symlink is applied only if its target stays inside the share.
// spec(SHARE): Special files are never applied.
#[test]
fn symlinks_are_never_followed_and_special_files_never_applied() {
    let scratch = Scratch::new("symlinks");
    let share = &scratch.share;
    let outside = scratch.root.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::create_dir(share.join("docs")).unwrap();
    symlink(&outside, share.join("hop")).unwrap();
    let (run, pending) = scratch.run("notes", 1);
    link(&pending, "inside", "a/../b");
    link(&pending, "escape", "../outside");
    link(&pending, "absolute", "/etc/passwd");
    link(&pending, "through", "hop/file");
    fs::write(pending.join("tree/pipe"), "").unwrap();
    record(&pending, "FIFO \"pipe\" 644");
    fs::write(pending.join("tree/null"), "").unwrap();
    record(&pending, "CHAR \"null\" 666 1 3");
    // The agent writes beneath `hop`, which the agent saw as your symlink,
    // and beneath `docs`, which becomes a symlink after the run.
    write(&pending, "hop/file", 0o644, b"not for outside\n");
    write(&pending, "docs/file", 0o644, b"not for outside\n");
    fs::remove_dir(share.join("docs")).unwrap();
    symlink(&outside, share.join("docs")).unwrap();

    let output = scratch.ap(&["apply", &run]);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stdout(&output),
        concat!(
            "skipped absolute: it is a symlink whose target leaves the share\n",
            "failed docs/file: a directory on its way is a symlink on your side\n",
            "skipped escape: it is a symlink whose target leaves the share\n",
            "failed hop/file: a directory on its way is a symlink on your side\n",
            "skipped null: it is a char, and special files are never applied\n",
            "skipped pipe: it is a fifo, and special files are never applied\n",
            "skipped through: it is a symlink whose target goes through a symlink\n",
        )
    );
    assert_eq!(
        stderr(&output),
        format!("ap: 2 files stay pending in run {run}, which `ap diff {run}` shows\n")
    );
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    assert_eq!(
        fs::read_link(share.join("inside")).unwrap(),
        Path::new("a/../b")
    );
    assert_eq!(scratch.names(""), ["docs", "hop", "inside"]);
    // The writes that failed stay pending, and nothing else does.
    let output = scratch.ap(&["diff", &run]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("+++ b/docs/file\n")
            && stdout(&output).contains("+++ b/hop/file\n"),
        "{}",
        stdout(&output)
    );
    assert!(!stdout(&output).contains("skipped"), "{}", stdout(&output));
    // Once `docs` is a directory again, the write lands.
    fs::remove_file(share.join("docs")).unwrap();
    fs::create_dir(share.join("docs")).unwrap();
    let output = scratch.ap(&["apply", &run]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stdout(&output),
        "failed hop/file: a directory on its way is a symlink on your side\n"
    );
    assert_eq!(scratch.read("docs/file"), "not for outside\n");
}

/// A symlink to a file the agent replaced goes by the file you have, because
/// that replacement may not land.
#[test]
fn a_symlink_to_a_change_that_doesn_t_land_goes_by_yours() {
    let scratch = Scratch::new("symlink-skipped");
    let share = &scratch.share;
    let outside = scratch.root.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(share.join("plain"), "plain\n").unwrap();
    let (run, pending) = scratch.run("notes", 1);
    change(share, &pending, "plain", 0o644, b"the agent's\n");
    link(&pending, "link", "plain");
    // You make `plain` a symlink that leaves the share.
    fs::remove_file(share.join("plain")).unwrap();
    symlink(&outside, share.join("plain")).unwrap();

    let output = scratch.ap(&["apply", &run]);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stdout(&output),
        concat!(
            "skipped link: it is a symlink whose target goes through a symlink\n",
            "skipped plain: it changed on your side after the agent first changed it\n",
        )
    );
    assert_eq!(scratch.names(""), ["plain"]);
}

/// The agent made a directory, wrote in it, and then made it read-only.
// spec(SHARE): `ap apply` makes a directory only to hold what it applies, and gives each file it writes and each directory it makes the permission bits the agent gave it.
#[test]
fn a_directory_the_agent_made_read_only_still_gets_its_files() {
    let scratch = Scratch::new("sealed");
    let share = &scratch.share;
    let (run, pending) = scratch.run("notes", 1);
    record(&pending, &base(share, "sealed"));
    dir(&pending, "sealed", 0o555);
    write(&pending, "sealed/file", 0o640, b"in\n");
    record(&pending, &base(share, "empty"));
    dir(&pending, "empty", 0o755);

    let output = scratch.ap(&["apply", &run]);

    let sealed = mode(&share.join("sealed"));
    fs::set_permissions(share.join("sealed"), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");
    assert_eq!(sealed, 0o555);
    assert_eq!(scratch.read("sealed/file"), "in\n");
    assert_eq!(mode(&share.join("sealed/file")), 0o640);
    assert_eq!(scratch.names(""), ["sealed"]);
}

// spec(SHARE): Git-ignored files are neither shown nor applied.
// spec(SHARE): `ap diff` and `ap apply` report how many files they skipped, and why.
// spec(SHARE): Your `.gitignore` files in the share decide which files are git-ignored, and ones the agent wrote don't.
// spec(SHARE): Files it leaves out on purpose, such as git-ignored ones, don't stay pending.
#[test]
fn git_ignored_files_are_counted_and_not_applied() {
    let scratch = Scratch::new("ignored");
    let share = &scratch.share;
    fs::write(share.join(".gitignore"), "*.log\nbuild/\n").unwrap();
    fs::write(share.join("old.log"), "old\n").unwrap();
    let (run, pending) = scratch.run("notes", 1);
    write(&pending, "out.log", 0o644, b"log\n");
    write(&pending, "build/a.o", 0o644, b"\0");
    delete(share, &pending, "old.log");
    // The agent's own `.gitignore` hides nothing.
    write(&pending, "src/.gitignore", 0o644, b"*.rs\n");
    write(&pending, "src/main.rs", 0o644, b"fn main() {}\n");

    let output = scratch.ap(&["apply", &run]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "skipped 3 git-ignored files\n");
    assert_eq!(scratch.read("old.log"), "old\n");
    assert!(!scratch.exists("out.log"));
    assert!(!scratch.exists("build"));
    assert_eq!(scratch.read("src/main.rs"), "fn main() {}\n");
    assert_eq!(scratch.read("src/.gitignore"), "*.rs\n");
    assert!(!scratch.store().join("pending").join(&run).exists());
}

// spec(SHARE): `ap apply` and `ap discard` fail while the run is active.
#[test]
fn apply_fails_while_the_run_is_active() {
    let scratch = Scratch::new("active");
    fs::write(scratch.share.join("kept.txt"), "yours\n").unwrap();
    let (run, pending) = scratch.run("notes", 1);
    change(
        &scratch.share,
        &pending,
        "kept.txt",
        0o644,
        b"the agent's\n",
    );
    let held = scratch.runs().hold(&run).unwrap();

    let output = scratch.ap(&["apply", &run]);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        format!(
            "ap: run {run} is active, so its changes stay. Stop it with `ap stop {run}` first\n"
        )
    );
    assert_eq!(scratch.read("kept.txt"), "yours\n");
    assert!(scratch.runs().has_pending(&run).unwrap());
    drop(held);
    let output = scratch.ap(&["apply", &run]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(scratch.read("kept.txt"), "the agent's\n");
}

// spec(SHARE): `ap diff`, `ap apply`, and `ap discard` take an agent or a run.
// spec(SHARE): If several runs have pending changes, each command lists them and exits 1, without a prompt.
#[test]
fn apply_takes_an_agent_or_a_run_and_lists_several() {
    let scratch = Scratch::new("names");
    scratch.agent("notes");
    let (older, pending) = scratch.run("notes", 1);
    write(&pending, "a", 0o644, b"a\n");
    let (newer, pending) = scratch.run("notes", 2);
    write(&pending, "b", 0o644, b"b\n");

    let output = scratch.ap(&["apply", "notes"]);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert_eq!(
        stderr(&output),
        format!(
            "ap: agent notes has pending changes in 2 runs, so name one of them:\n{newer}\n{older}\n"
        )
    );
    assert!(!scratch.exists("a") && !scratch.exists("b"));
    let output = scratch.ap(&["apply", &older]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(scratch.read("a"), "a\n");
    let output = scratch.ap(&["apply", "notes"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(scratch.read("b"), "b\n");
    // With nothing pending, there is nothing to do.
    let output = scratch.ap(&["apply", "notes"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");
    let output = scratch.ap(&["apply", "nobody"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "ap: no agent or run is named nobody\n");
}

/// The branch import isn't built yet, so a share whose root holds `.git`
/// keeps everything pending, and so does every other share of the run.
#[test]
fn a_repository_s_share_keeps_everything_pending() {
    let scratch = Scratch::new("repository");
    let plain = scratch.root.join("plain");
    fs::create_dir(&plain).unwrap();
    fs::create_dir(scratch.share.join(".git")).unwrap();
    let runs = scratch.runs();
    let run = runs.create("notes", 1).unwrap().name;
    runs.create_pending(&run).unwrap();
    for root in [&plain, &scratch.share] {
        let pending = runs.create_pending_share(&run, root).unwrap();
        fs::create_dir(pending.join("tree")).unwrap();
        fs::write(pending.join("records"), "").unwrap();
        write(&pending, "file", 0o644, b"x\n");
    }
    let records = fs::read(runs.pending_shares(&run).unwrap()[1].dir.join("records")).unwrap();

    let output = scratch.ap(&["apply", &run]);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert_eq!(
        stderr(&output),
        format!(
            "ap: run {run}: {}: the share's directory holds a git repository, and `ap apply` can't import a branch yet, so everything stays pending\n",
            scratch.share.display()
        )
    );
    assert!(!plain.join("file").exists());
    assert!(!scratch.exists("file"));
    let after = fs::read(runs.pending_shares(&run).unwrap()[1].dir.join("records")).unwrap();
    assert_eq!(after, records);
}

#[test]
fn each_share_s_report_starts_with_a_line_that_names_it() {
    let scratch = Scratch::new("shares");
    let other = scratch.root.join("other");
    fs::create_dir(&other).unwrap();
    let runs = scratch.runs();
    let run = runs.create("notes", 1).unwrap().name;
    runs.create_pending(&run).unwrap();
    for root in [&scratch.share, &other] {
        fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        let pending = runs.create_pending_share(&run, root).unwrap();
        fs::create_dir(pending.join("tree")).unwrap();
        fs::write(pending.join("records"), "").unwrap();
        write(&pending, "out.log", 0o644, b"log\n");
        write(&pending, "kept", 0o644, b"x\n");
    }
    let pending = runs.pending_shares(&run).unwrap()[1].dir.clone();
    fs::write(pending.join("tree/pipe"), "").unwrap();
    record(&pending, "FIFO \"pipe\" 644");

    let output = scratch.ap(&["apply", &run]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!(
            "share {}\nskipped pipe: it is a fifo, and special files are never applied\nskipped 2 git-ignored files\n",
            other.display()
        )
    );
    assert_eq!(fs::read_to_string(other.join("kept")).unwrap(), "x\n");
    assert_eq!(scratch.read("kept"), "x\n");
}

/// Sets a limit on the size of a file `ap` writes, and what `SIGXFSZ` does
/// once it writes past it.
fn limit_file_size(command: &mut Command, on_signal: libc::sighandler_t) {
    // SAFETY: `signal` and `setrlimit` are async-signal-safe, and touch
    // nothing but the child.
    unsafe {
        command.pre_exec(move || {
            let limit = libc::rlimit {
                rlim_cur: 1 << 16,
                rlim_max: 1 << 16,
            };
            libc::signal(libc::SIGXFSZ, on_signal);
            match libc::setrlimit(libc::RLIMIT_FSIZE, &limit) {
                0 => Ok(()),
                _ => Err(std::io::Error::last_os_error()),
            }
        });
    }
}

/// From #304: `ap apply` writes each file beside its target and renames it
/// into place. A write that fails, here because the file is larger than
/// `ap` may write, leaves your file as it was, and nothing beside it.
#[test]
fn a_failed_write_leaves_your_file_as_it_was() {
    let scratch = Scratch::new("failed-write");
    let share = &scratch.share;
    fs::write(share.join("big"), "yours\n").unwrap();
    let (run, pending) = scratch.run("notes", 1);
    let large = vec![b'x'; 1 << 20];
    change(share, &pending, "big", 0o644, &large);
    write(&pending, "small", 0o644, b"s\n");

    let mut command = scratch.command(&["apply", &run]);
    limit_file_size(&mut command, libc::SIG_IGN);
    let output = command.output().unwrap();

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "failed big: File too large (os error 27)\n"
    );
    assert_eq!(scratch.read("big"), "yours\n");
    assert_eq!(scratch.read("small"), "s\n");
    assert_eq!(scratch.names(""), ["big", "small"]);
    let output = scratch.ap(&["apply", &run]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(fs::read(share.join("big")).unwrap(), large);
    assert!(!scratch.store().join("pending").join(&run).exists());
}

/// A write that fails in a directory the agent made read-only leaves the
/// directory one you can write in, so a later `ap apply` lands the file and
/// then gives the directory the agent's mode.
#[test]
fn a_failed_write_in_a_read_only_directory_lands_later() {
    let scratch = Scratch::new("failed-sealed");
    let share = &scratch.share;
    let (run, pending) = scratch.run("notes", 1);
    record(&pending, &base(share, "sealed"));
    dir(&pending, "sealed", 0o555);
    let large = vec![b'x'; 1 << 20];
    write(&pending, "sealed/big", 0o644, &large);

    let mut command = scratch.command(&["apply", &run]);
    limit_file_size(&mut command, libc::SIG_IGN);
    let output = command.output().unwrap();

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "failed sealed/big: File too large (os error 27)\n"
    );
    assert_eq!(mode(&share.join("sealed")) & 0o200, 0o200);
    let output = scratch.ap(&["apply", &run]);
    let sealed = mode(&share.join("sealed"));
    writable(share);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(fs::read(share.join("sealed/big")).unwrap(), large);
    assert_eq!(sealed, 0o555);
    assert!(!scratch.store().join("pending").join(&run).exists());
}

/// An `ap apply` killed while it writes leaves your file as it was, and
/// everything pending, so the next one lands it, and finds the file the
/// first one landed already the agent's.
#[test]
fn an_interrupted_write_leaves_your_file_as_it_was() {
    use std::os::unix::process::ExitStatusExt;
    let scratch = Scratch::new("interrupted-write");
    let share = &scratch.share;
    fs::write(share.join("big"), "yours\n").unwrap();
    fs::write(share.join("a"), "yours\n").unwrap();
    let (run, pending) = scratch.run("notes", 1);
    let large = vec![b'x'; 1 << 20];
    change(share, &pending, "a", 0o644, b"the agent's\n");
    change(share, &pending, "big", 0o644, &large);
    // `b/file` lands before `big`, and `b` gets its mode only once the
    // records are rewritten.
    record(&pending, &base(share, "b"));
    dir(&pending, "b", 0o555);
    write(&pending, "b/file", 0o644, b"b\n");

    let mut command = scratch.command(&["apply", &run]);
    limit_file_size(&mut command, libc::SIG_DFL);
    let output = command.output().unwrap();

    assert_eq!(output.status.signal(), Some(libc::SIGXFSZ));
    assert_eq!(scratch.read("a"), "the agent's\n");
    assert_eq!(scratch.read("b/file"), "b\n");
    assert_eq!(scratch.read("big"), "yours\n");
    assert!(scratch.runs().has_pending(&run).unwrap());
    // What the killed write left beside your file was never your file.
    for name in scratch.names("") {
        if name != "a" && name != "b" && name != "big" {
            fs::remove_file(share.join(name)).unwrap();
        }
    }
    let output = scratch.ap(&["apply", &run]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let sealed = mode(&share.join("b"));
    writable(share);
    assert_eq!(stdout(&output), "");
    assert_eq!(fs::read(share.join("big")).unwrap(), large);
    assert_eq!(scratch.read("b/file"), "b\n");
    assert_eq!(sealed, 0o555);
    assert!(!scratch.store().join("pending").join(&run).exists());
}
