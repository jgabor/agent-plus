//! `ap apply`, from SPEC.md's `SHARE` section, for a share whose root holds
//! a `.git` directory or doesn't. Nothing here boots: a test writes a run's pending
//! directory the way the share server does, as its `cow` module describes,
//! and makes a run active the way a run process does, by holding the lock on
//! its record.

mod common;

use common::repo;

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
        fs::write(&grants, "AGENT claude\nNETWORK -\nPOLICY -\n").unwrap();
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

// spec(SHARE): Nothing is applied to your files at or beneath a directory that holds `HEAD`, and either `commondir` or both `objects` and `refs`, because that is how git finds a bare repository.
// spec(SHARE): That counts what you have there and what the agent made there, with the agent's names matched case-insensitively, so nothing the agent wrote becomes a bare repository.
#[test]
fn nothing_is_applied_in_a_bare_repository() {
    let scratch = Scratch::new("bare");
    let share = &scratch.share;
    // Your bare repository, with an empty directory git needs.
    fs::create_dir_all(share.join("repo.git/objects")).unwrap();
    fs::create_dir_all(share.join("repo.git/refs/heads")).unwrap();
    fs::write(share.join("repo.git/HEAD"), "ref: refs/heads/main\n").unwrap();
    // A directory of yours that the agent turns into one.
    fs::create_dir(share.join("src")).unwrap();
    fs::write(share.join("src/main.rs"), "fn main() {}\n").unwrap();
    // A directory with only a `HEAD` isn't one.
    fs::create_dir(share.join("notes")).unwrap();
    let (run, pending) = scratch.run("notes", 1);
    change(share, &pending, "repo.git/HEAD", 0o644, b"0000\n");
    write(&pending, "repo.git/config", 0o644, b"[core]\n");
    write(&pending, "repo.git/hooks/pre-commit", 0o755, b"#!/bin/sh\n");
    record(&pending, &base(share, "repo.git/refs"));
    record(&pending, "DELETE \"repo.git/refs\"");
    write(&pending, "src/HEAD", 0o644, b"ref: refs/heads/main\n");
    dir(&pending, "src/objects", 0o755);
    write(&pending, "src/refs/heads/main", 0o644, b"0000\n");
    write(&pending, "src/config", 0o644, b"[core]\n");
    change(
        share,
        &pending,
        "src/main.rs",
        0o644,
        b"fn main() { run() }\n",
    );
    // A worktree's git directory, named in another case.
    dir(&pending, "wt", 0o755);
    write(&pending, "wt/Head", 0o644, b"0000\n");
    write(&pending, "wt/CommonDir", 0o644, b"..\n");
    write(&pending, "wt/config", 0o644, b"[core]\n");
    write(&pending, "notes/HEAD", 0o644, b"a note\n");

    let output = scratch.ap(&["apply", &run]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        concat!(
            "skipped repo.git/HEAD: it is in a bare repository\n",
            "skipped repo.git/config: it is in a bare repository\n",
            "skipped repo.git/hooks/pre-commit: it is in a bare repository\n",
            "skipped src/HEAD: it is in a bare repository\n",
            "skipped src/config: it is in a bare repository\n",
            "skipped src/main.rs: it is in a bare repository\n",
            "skipped src/refs/heads/main: it is in a bare repository\n",
            "skipped wt/CommonDir: it is in a bare repository\n",
            "skipped wt/Head: it is in a bare repository\n",
            "skipped wt/config: it is in a bare repository\n",
        )
    );
    assert_eq!(scratch.names("repo.git"), ["HEAD", "objects", "refs"]);
    assert_eq!(scratch.read("repo.git/HEAD"), "ref: refs/heads/main\n");
    assert_eq!(scratch.names("repo.git/refs"), ["heads"]);
    assert_eq!(scratch.names("src"), ["main.rs"]);
    assert_eq!(scratch.read("src/main.rs"), "fn main() {}\n");
    assert!(!scratch.exists("wt"));
    assert_eq!(scratch.read("notes/HEAD"), "a note\n");
    // Left out on purpose, so none of it stays pending.
    assert!(!scratch.store().join("pending").join(&run).exists());
}

// spec(SHARE): That counts the names along the share's own path on your host, so nothing is applied to your files in a share at or beneath a `.git` directory.
#[test]
fn nothing_is_applied_in_a_share_that_is_git() {
    let scratch = Scratch::sharing("git-root", ".Git");
    fs::write(scratch.share.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    let (run, pending) = scratch.run("notes", 1);
    write(&pending, "config", 0o644, b"[core]\n");

    let output = scratch.ap(&["apply", &run]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "skipped config: it is under .git\n");
    assert!(!scratch.exists("config"));
}

// spec(SHARE): That counts the names along the share's own path on your host, so nothing is applied to your files in a share at or beneath a `.git` directory.
#[test]
fn nothing_is_applied_in_a_share_beneath_git() {
    let scratch = Scratch::sharing("git-above", ".GIT/hooks");
    let share = &scratch.share;
    // Not what git finds a bare repository by, so only its name keeps it.
    fs::write(scratch.root.join(".GIT/HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(share.join("pre-push"), "#!/bin/sh\n").unwrap();
    fs::write(share.join("sample"), "#!/bin/sh\n").unwrap();
    fs::create_dir(share.join("old")).unwrap();
    let (run, pending) = scratch.run("notes", 1);
    write(&pending, "pre-commit", 0o755, b"#!/bin/sh\nrun\n");
    change(share, &pending, "pre-push", 0o755, b"#!/bin/sh\nrun\n");
    delete(share, &pending, "sample");
    delete(share, &pending, "old");

    let output = scratch.ap(&["apply", &run]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        concat!(
            "skipped pre-commit: it is under .git\n",
            "skipped pre-push: it is under .git\n",
            "skipped sample: it is under .git\n",
        )
    );
    assert_eq!(scratch.names(""), ["old", "pre-push", "sample"]);
    assert_eq!(scratch.read("pre-push"), "#!/bin/sh\n");
    // Left out on purpose, so none of it stays pending.
    assert!(!scratch.store().join("pending").join(&run).exists());
}

// spec(SHARE): Nothing is applied to your files at or beneath a directory that holds `HEAD`, and either `commondir` or both `objects` and `refs`, because that is how git finds a bare repository.
#[test]
fn nothing_is_applied_in_a_share_that_is_a_bare_repository() {
    let scratch = Scratch::sharing("bare-root", "repo.git");
    let share = &scratch.share;
    fs::create_dir(share.join("objects")).unwrap();
    fs::create_dir(share.join("refs")).unwrap();
    fs::write(share.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    let (run, pending) = scratch.run("notes", 1);
    write(&pending, "config", 0o644, b"[core]\n");

    let output = scratch.ap(&["apply", &run]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "skipped config: it is in a bare repository\n"
    );
    assert!(!scratch.exists("config"));
}

// spec(SHARE): Nothing is applied to your files at or beneath a directory that holds `HEAD`, and either `commondir` or both `objects` and `refs`, because that is how git finds a bare repository.
#[test]
fn nothing_is_applied_in_a_share_beneath_a_bare_repository() {
    let scratch = Scratch::sharing("bare-above", "repo.git/hooks");
    let repo = scratch.root.join("repo.git");
    fs::create_dir(repo.join("objects")).unwrap();
    fs::create_dir(repo.join("refs")).unwrap();
    fs::write(repo.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    let (run, pending) = scratch.run("notes", 1);
    write(&pending, "pre-commit", 0o755, b"#!/bin/sh\n");

    let output = scratch.ap(&["apply", &run]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "skipped pre-commit: it is in a bare repository\n"
    );
    assert!(!scratch.exists("pre-commit"));
}

// spec(SHARE): Nothing is applied to your files at or beneath a directory that holds `HEAD`, and either `commondir` or both `objects` and `refs`, because that is how git finds a bare repository.
// spec(SHARE): That counts what you have there and what the agent made there, with the agent's names matched case-insensitively, so nothing the agent wrote becomes a bare repository.
#[test]
fn a_bare_repository_and_a_file_don_t_replace_each_other() {
    let scratch = Scratch::new("bare-replace");
    let share = &scratch.share;
    fs::create_dir_all(share.join("repo.git/objects")).unwrap();
    fs::create_dir_all(share.join("repo.git/refs")).unwrap();
    fs::write(share.join("repo.git/HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(share.join("notes"), "yours\n").unwrap();
    let (run, pending) = scratch.run("notes", 1);
    // The agent puts a file in place of your bare repository.
    change(share, &pending, "repo.git", 0o644, b"a file\n");
    // And a bare repository in place of your file.
    record(&pending, &base(share, "notes"));
    dir(&pending, "notes", 0o755);
    write(&pending, "notes/HEAD", 0o644, b"0000\n");
    write(&pending, "notes/commondir", 0o644, b"..\n");

    let output = scratch.ap(&["apply", &run]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        concat!(
            "skipped notes: it is in a bare repository\n",
            "skipped notes/HEAD: it is in a bare repository\n",
            "skipped notes/commondir: it is in a bare repository\n",
            "skipped repo.git: it is in a bare repository\n",
            "skipped repo.git/HEAD: it is in a bare repository\n",
        )
    );
    assert_eq!(scratch.names("repo.git"), ["HEAD", "objects", "refs"]);
    assert_eq!(scratch.read("notes"), "yours\n");
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

/// Only a directory `ap apply` made gets the agent's mode, so a directory
/// you made since keeps yours, and one that replaces your file gets it.
#[test]
fn only_a_directory_apply_makes_gets_the_agent_s_mode() {
    let scratch = Scratch::new("sealed-yours");
    let share = &scratch.share;
    fs::write(share.join("was-a-file"), "yours\n").unwrap();
    let (run, pending) = scratch.run("notes", 1);
    record(&pending, &base(share, "yours"));
    dir(&pending, "yours", 0o555);
    delete(share, &pending, "was-a-file");
    dir(&pending, "was-a-file", 0o555);
    write(&pending, "was-a-file/file", 0o644, b"b\n");
    // You make `yours` after the run.
    fs::create_dir(share.join("yours")).unwrap();
    fs::set_permissions(share.join("yours"), fs::Permissions::from_mode(0o700)).unwrap();

    let output = scratch.ap(&["apply", &run]);

    let modes = (mode(&share.join("yours")), mode(&share.join("was-a-file")));
    writable(share);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(scratch.read("was-a-file/file"), "b\n");
    assert_eq!(modes, (0o700, 0o555));
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

/// A file and a symlink the agent wrote in place of your directories that
/// hold a git-ignored file, at any depth, are skipped, and each directory
/// stays with its git-ignored files, as if the agent had deleted it. A file
/// in place of a directory with no ignored file in it lands.
// spec(SHARE): Where your directory holds a git-ignored file, and the agent wrote a file or symlink in its place, `ap apply` skips that file and says why, unless it changed on your side.
// spec(SHARE): The directory stays, with its git-ignored files, as if the agent had deleted it.
#[test]
fn a_file_in_place_of_your_directory_with_an_ignored_file_is_skipped() {
    let scratch = Scratch::new("ignored-replaced");
    let share = &scratch.share;
    fs::write(share.join(".gitignore"), "*.log\n").unwrap();
    for (path, data) in [
        ("logs/old.log", "old\n"),
        ("logs/notes.txt", "notes\n"),
        ("cache/deep/run.log", "run\n"),
        ("plain/a.txt", "a\n"),
    ] {
        fs::create_dir_all(share.join(path).parent().unwrap()).unwrap();
        fs::write(share.join(path), data).unwrap();
    }
    let (run, pending) = scratch.run("notes", 1);
    change(share, &pending, "logs", 0o644, b"a file\n");
    record(&pending, &base(share, "cache"));
    link(&pending, "cache", ".gitignore");
    change(share, &pending, "plain", 0o644, b"plain\n");

    let diff = scratch.ap(&["diff", &run]);
    let output = scratch.ap(&["apply", &run]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let why = "it would replace a directory that holds a git-ignored file";
    let skipped = format!("skipped cache: {why}\nskipped logs: {why}\n");
    assert!(stdout(&diff).starts_with(&skipped), "{}", stdout(&diff));
    assert_eq!(
        stdout(&output),
        format!("{skipped}skipped 2 git-ignored files\n")
    );
    assert_eq!(scratch.names("logs"), ["old.log"]);
    assert_eq!(scratch.names("cache"), ["deep"]);
    assert_eq!(scratch.read("cache/deep/run.log"), "run\n");
    assert_eq!(scratch.read("plain"), "plain\n");
    assert!(!scratch.store().join("pending").join(&run).exists());
}

/// A git-ignored file you put in your directory after the agent replaced
/// it is a change on your side, so the agent's file stays pending.
// spec(SHARE): Where your directory holds a git-ignored file, and the agent wrote a file or symlink in its place, `ap apply` skips that file and says why, unless it changed on your side.
#[test]
fn an_ignored_file_you_add_where_the_agent_wrote_a_file_is_a_change_on_your_side() {
    let scratch = Scratch::new("ignored-added");
    let share = &scratch.share;
    fs::write(share.join(".gitignore"), "*.log\n").unwrap();
    fs::create_dir(share.join("logs")).unwrap();
    let (run, pending) = scratch.run("notes", 1);
    change(share, &pending, "logs", 0o644, b"a file\n");
    fs::write(share.join("logs/today.log"), "today\n").unwrap();

    let output = scratch.ap(&["apply", &run]);

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        concat!(
            "skipped logs: it changed on your side after the agent first changed it\n",
            "skipped 1 git-ignored file\n",
        )
    );
    assert_eq!(scratch.names("logs"), ["today.log"]);
    assert!(scratch.runs().has_pending(&run).unwrap());
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

/// A run of `notes` on a repository: your repository in the share, with a
/// first commit unless it is to have none, the run's copy of its `.git`,
/// and the agent's own clone of it, `guest`.
struct Repository {
    scratch: Scratch,
    run: String,
    pending: PathBuf,
    guest: PathBuf,
    /// The commit your `HEAD` was at when the run started, if any.
    start: Option<String>,
}

/// How a test's run on a repository differs from the plain one.
#[derive(Default)]
struct Setup<'a> {
    /// Arguments for `git init`.
    init: &'a [&'a str],
    /// The directory of a share without `.git`, which the run shares
    /// first, with one file the agent wrote.
    plain: Option<&'a str>,
    /// What happens to your repository before the run starts.
    before: Option<&'a dyn Fn(&Path)>,
}

impl Repository {
    fn new(name: &str, first: bool, init: &[&str]) -> Repository {
        Repository::with(
            name,
            first,
            Setup {
                init,
                ..Setup::default()
            },
        )
    }

    fn with(name: &str, first: bool, setup: Setup) -> Repository {
        let scratch = Scratch::new(name);
        let mut args = vec!["init", "-q"];
        args.extend(setup.init);
        repo::run(&scratch.share, &args);
        let start = first.then(|| repo::commit(&scratch.share, "README", "hello\n", "start"));
        if let Some(before) = setup.before {
            before(&scratch.share);
        }
        let runs = scratch.runs();
        let run = runs.create("notes", 1).unwrap().name;
        runs.create_pending(&run).unwrap();
        let mut roots = Vec::new();
        if let Some(plain) = setup.plain {
            let plain = scratch.root.join(plain);
            fs::create_dir(&plain).unwrap();
            roots.push(plain);
        }
        roots.push(scratch.share.clone());
        let mut pending = PathBuf::new();
        for root in &roots {
            pending = runs.create_pending_share(&run, root).unwrap();
            fs::create_dir(pending.join("tree")).unwrap();
            fs::write(pending.join("records"), "").unwrap();
            if *root != scratch.share {
                write(&pending, "file", 0o644, b"x\n");
            }
        }
        repo::capture(&scratch.share, &pending);
        let guest = scratch.root.join("guest");
        repo::guest(&scratch.share, &guest);
        Repository {
            scratch,
            run,
            pending,
            guest,
            start,
        }
    }

    fn share(&self) -> &Path {
        &self.scratch.share
    }

    /// The branch an import makes.
    fn branch(&self) -> String {
        format!("ap/notes/{}", self.run)
    }

    /// The agent commits `data` at `path`, which it writes in the share too.
    fn commit(&self, path: &str, data: &str) -> String {
        if self.scratch.exists(path) {
            record(&self.pending, &base(self.share(), path));
        }
        write(&self.pending, path, 0o644, data.as_bytes());
        repo::commit(&self.guest, path, data, &format!("write {path}"))
    }

    /// Puts what the agent's git wrote in the pending directory.
    fn land(&self) {
        repo::land(&self.guest, self.share(), &self.pending);
    }

    /// Runs `ap apply` with the `PATH` of the tests, so it finds `git`.
    fn apply(&self) -> Output {
        let path = std::env::var_os("PATH").unwrap();
        let mut command = self.scratch.command(&["apply", &self.run]);
        command.env("PATH", path).output().unwrap()
    }

    /// Runs `ap apply` as `apply` does, with at most `seconds` of CPU time.
    fn apply_within(&self, seconds: u64) -> Output {
        let path = std::env::var_os("PATH").unwrap();
        let mut command = self.scratch.command(&["apply", &self.run]);
        repo::limit_cpu(command.env("PATH", path), seconds);
        command.output().unwrap()
    }

    fn records(&self) -> Vec<u8> {
        fs::read(self.pending.join("records")).unwrap()
    }

    /// Your refs, and the name of each object your repository holds.
    fn objects_and_refs(&self) -> (String, Vec<PathBuf>) {
        fn files(dir: &Path, out: &mut Vec<PathBuf>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                match path.is_dir() {
                    true => files(&path, out),
                    false => out.push(path),
                }
            }
        }
        let mut objects = Vec::new();
        files(&self.share().join(".git/objects"), &mut objects);
        objects.sort();
        let refs = repo::run(self.share(), &["for-each-ref"]);
        (refs, objects)
    }

    fn has_branch(&self) -> bool {
        let name = format!("refs/heads/{}", self.branch());
        (repo::git(self.share()).args(["show-ref", "-q", "--verify", &name]))
            .status()
            .unwrap()
            .success()
    }

    /// Checks that everything stays pending, after an `ap apply` that
    /// printed `error`, with your repository as `before` says.
    fn stays_pending(&self, output: &Output, error: &str, before: &(String, Vec<PathBuf>)) {
        assert_eq!(output.status.code(), Some(1), "{}", stderr(output));
        assert_eq!(stdout(output), "");
        let share = self.share().display();
        assert_eq!(
            stderr(output),
            format!("ap: run {}: {share}: {error}\n", self.run)
        );
        assert_eq!(&self.objects_and_refs(), before);
        assert!(self.scratch.runs().has_pending(&self.run).unwrap());
    }

    /// The tree the agent's git would commit from its index, once `change`
    /// has changed it, which leaves the agent's history as it was.
    fn tree_after(&self, change: &[&[&str]]) -> String {
        for args in change {
            repo::run(&self.guest, args);
        }
        let tree = repo::run(&self.guest, &["write-tree"]);
        repo::run(&self.guest, &["reset", "-q"]);
        tree
    }

    /// Checks that the branch ends in the one last commit of the files the
    /// agent didn't commit, which holds `tree`, comes after the agent's
    /// `HEAD`, if it names a commit, and was made between `from` and `to`,
    /// in seconds since the epoch.
    fn assert_last_commit(&self, tree: &str, (from, to): (u64, u64)) {
        let share = self.share();
        let branch = self.branch();
        let format = "%an <%ae>%n%cn <%ce>%n%s%n%b%n%at%n%ct%n%P";
        let log = repo::run(
            share,
            &["log", "-1", &format!("--format={format}"), &branch],
        );
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(
            lines[..3],
            [
                "Agent+ <ap@localhost>",
                "Agent+ <ap@localhost>",
                &*format!("Uncommitted changes from notes/{}", self.run)
            ]
        );
        assert_eq!(lines[3], "", "a commit with a body: {log}");
        let raw = repo::run(share, &["cat-file", "commit", &branch]);
        let zones = (raw.lines())
            .filter(|line| line.starts_with("author ") || line.starts_with("committer "))
            .filter(|line| line.ends_with(" +0000"));
        assert_eq!(zones.count(), 2, "{raw}");
        for time in &lines[4..6] {
            let time: u64 = time.parse().unwrap();
            assert!((from..=to).contains(&time), "{time} isn't in {from}..={to}");
        }
        let head = repo::git(&self.guest)
            .args(["rev-parse", "-q", "--verify", "HEAD"])
            .output()
            .unwrap();
        let head = String::from_utf8(head.stdout).unwrap();
        assert_eq!(lines.get(6).copied().unwrap_or(""), head.trim());
        assert_eq!(
            repo::run(share, &["rev-parse", &format!("{branch}^{{tree}}")]),
            tree
        );
        if !head.is_empty() {
            let parent = format!("{branch}~1");
            assert_eq!(
                repo::history(share, &parent),
                repo::history(&self.guest, "HEAD")
            );
        }
        repo::run(share, &["fsck", "--strict", "--no-dangling"]);
        assert!(self.scratch.runs().list_pending().unwrap().is_empty());
    }

    /// Runs `ap apply`, and returns what it printed, and the seconds since
    /// the epoch before and after it ran.
    fn apply_timed(&self) -> (Output, (u64, u64)) {
        let now = || {
            (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH))
                .unwrap()
                .as_secs()
        };
        let from = now();
        let output = self.apply();
        (output, (from, now()))
    }
}

// spec(SHARE): When the share's root on your host holds a `.git` directory, the agent's commits, and the files it wrote but didn't commit, land as a new branch, `ap/<agent>/<run>`, imported as git objects and checked with `git fsck`.
// spec(SHARE): The branch starts from the commit your `HEAD` was at when the run started.
// spec(SHARE): Your working tree, hooks, and config are not touched.
// spec(SHARE): Neither are your `HEAD` and your index.
// spec(SHARE): When `ap` reads git state that an agent wrote, here or in a persisted repository, it reads refs, objects, the index, and ignore files with its own code.
// spec(SHARE): It never runs that repository's hooks or uses its config or attributes, because those can run commands.
// spec(SHARE): When nothing is uncommitted, there is no such commit.
#[test]
fn the_agent_s_commits_land_as_a_branch_from_the_start_commit() {
    let repository = Repository::new("branch", true, &[]);
    let share = repository.share();
    repository.commit("README", "hello, world\n");
    // A pack holds the first commit, and the second is loose.
    repo::run(&repository.guest, &["repack", "-q", "-d"]);
    repository.commit("docs/guide.md", "read me\n");
    let history = repo::history(&repository.guest, "HEAD");
    // The agent's config and hooks would leave a mark if git used them.
    let mark = repository.scratch.root.join("mark");
    let config = repository.guest.join(".git/config");
    let mut text = fs::read_to_string(&config).unwrap();
    text.push_str(&format!("[core]\n\tfsmonitor = touch {}\n", mark.display()));
    fs::write(&config, text).unwrap();
    for hook in ["reference-transaction", "post-checkout", "pre-auto-gc"] {
        let hook = repository.guest.join(".git/hooks").join(hook);
        fs::write(&hook, format!("#!/bin/sh\ntouch {}\n", mark.display())).unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    }
    repository.land();
    let yours =
        || ["HEAD", "index", "config"].map(|path| fs::read(share.join(".git").join(path)).unwrap());
    let hooks = || repository.scratch.names(".git/hooks");
    let before = (yours(), hooks());

    let output = repository.apply();

    assert!(!mark.exists());
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");
    assert_eq!(stderr(&output), "");
    let branch = repository.branch();
    assert_eq!(repo::history(share, &branch), history);
    let start = repository.start.as_deref().unwrap();
    assert_eq!(
        repo::run(share, &["rev-parse", &format!("{branch}~2")]),
        start
    );
    repo::run(share, &["fsck", "--strict", "--no-dangling"]);
    assert_eq!((yours(), hooks()), before);
    assert_eq!(
        repo::run(share, &["symbolic-ref", "HEAD"]),
        "refs/heads/main"
    );
    assert_eq!(repository.scratch.read("README"), "hello\n");
    assert!(!repository.scratch.exists("docs"));
    assert!(repository.scratch.runs().list_pending().unwrap().is_empty());
}

// spec(SHARE): If the agent's history no longer descends from that commit, or the repository had no commits, the branch is the agent's history as it stands.
#[test]
fn a_history_that_no_longer_descends_lands_as_it_stands() {
    let repository = Repository::new("orphan", true, &[]);
    repo::run(&repository.guest, &["checkout", "-q", "--orphan", "fresh"]);
    repository.commit("README", "anew\n");
    repository.land();

    let output = repository.apply();

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let history = repo::history(repository.share(), &repository.branch());
    assert_eq!(history, repo::history(&repository.guest, "HEAD"));
    assert_eq!(history.len(), 1);
}

// spec(SHARE): If the agent's history no longer descends from that commit, or the repository had no commits, the branch is the agent's history as it stands.
#[test]
fn a_repository_with_no_commits_gets_the_agent_s_history() {
    let repository = Repository::new("empty", false, &[]);
    assert_eq!(
        fs::read_to_string(repository.pending.join("head")).unwrap(),
        "none\n"
    );
    repository.commit("README", "one\n");
    repository.commit("README", "two\n");
    repository.land();

    let output = repository.apply();

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let history = repo::history(repository.share(), &repository.branch());
    assert_eq!(history, repo::history(&repository.guest, "HEAD"));
    assert_eq!(history.len(), 2);
    assert!(
        repo::git(repository.share())
            .args(["rev-parse", "-q", "--verify", "HEAD"])
            .output()
            .unwrap()
            .stdout
            .is_empty()
    );
}

// spec(SHARE): When the agent's `HEAD` names no commit, or the commit your `HEAD` was at when the run started, and the agent left no files uncommitted, there is no branch.
#[test]
fn a_run_that_made_no_commits_makes_no_branch() {
    for (name, first) in [("no-commits", true), ("no-commits-empty", false)] {
        let repository = Repository::new(name, first, &[]);
        fs::write(repository.share().join(".gitignore"), "*.log\n").unwrap();
        write(&repository.pending, "debug.log", 0o644, b"noise\n");

        let output = repository.apply();

        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        assert_eq!(stdout(&output), "skipped 1 git-ignored file\n");
        assert!(!repository.has_branch());
        assert!(repository.scratch.runs().list_pending().unwrap().is_empty());
    }
}

// spec(SHARE): Before the agent's objects or the branch move in, `ap apply` writes the objects to a quarantine directory and runs your host's `git fsck --strict` on them, against your repository.
// spec(SHARE): It never runs `git` on anything else the agent wrote.
// spec(SHARE): If the branch import fails, everything stays pending.
#[test]
fn an_object_fsck_refuses_keeps_everything_pending() {
    let repository = Repository::new("corrupt", true, &[]);
    let good = repository.commit("README", "hello, world\n");
    let guest = &repository.guest;
    let tree = repo::run(guest, &["rev-parse", "HEAD^{tree}"]);
    let mut hash = repo::git(guest);
    hash.args([
        "hash-object",
        "-t",
        "commit",
        "-w",
        "--literally",
        "--stdin",
    ]);
    let mut child = (hash.stdin(Stdio::piped()).stdout(Stdio::piped()))
        .spawn()
        .unwrap();
    let bad = format!(
        "tree {tree}\nparent {good}\nauthor nobody\ncommitter C <c@example.com> 1 +0000\n\nbad\n"
    );
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), bad.as_bytes()).unwrap();
    let bad = String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap();
    repo::run(guest, &["update-ref", "refs/heads/main", bad.trim()]);
    repository.land();
    let records = repository.records();
    let before = repository.objects_and_refs();

    let output = repository.apply();

    assert_eq!(output.status.code(), Some(1));
    let error = stderr(&output);
    let share = repository.share().display();
    let expected = format!(
        "ap: run {}: {share}: git fsck --strict refused the agent's objects, so everything stays pending:\n",
        repository.run
    );
    assert!(error.starts_with(&expected), "{error}");
    assert!(error.contains("missingEmail"), "{error}");
    assert_eq!(repository.objects_and_refs(), before);
    assert_eq!(repository.records(), records);
    assert!(!repository.has_branch());
    assert_eq!(repository.scratch.read("README"), "hello\n");
}

// spec(SHARE): With no `git` on your host, `ap apply` fails the branch import, says so, and everything stays pending.
// spec(Host requirements): On every host, `ap apply` needs `git` to import a branch (see [SHARE](#share)).
#[test]
fn with_no_git_everything_stays_pending() {
    let repository = Repository::new("no-git", true, &[]);
    repository.commit("README", "hello, world\n");
    repository.land();
    let records = repository.records();
    let before = repository.objects_and_refs();
    let empty = repository.scratch.root.join("empty");
    fs::create_dir(&empty).unwrap();

    let output = (repository.scratch.command(&["apply", &repository.run]))
        .env("PATH", &empty)
        .output()
        .unwrap();

    repository.stays_pending(
        &output,
        "`ap apply` needs git on your host to import a branch, and there is none, so everything stays pending",
        &before,
    );
    assert_eq!(repository.records(), records);
}

// spec(SHARE): `ap apply` refuses a branch import into a repository whose object format isn't SHA-1, says so, and everything stays pending.
#[test]
fn a_repository_that_isn_t_sha_1_keeps_everything_pending() {
    let repository = Repository::new("sha256", true, &["--object-format=sha256"]);
    repository.commit("README", "hello, world\n");
    repository.land();
    let records = repository.records();
    let before = repository.objects_and_refs();

    let output = repository.apply();

    repository.stays_pending(
        &output,
        "the repository's object format is sha256, and `ap apply` imports a branch only into a SHA-1 repository, so everything stays pending",
        &before,
    );
    assert_eq!(repository.records(), records);
}

// spec(SHARE): If the branch already exists, `ap apply` refuses the import, says so, and everything stays pending.
#[test]
fn a_branch_that_exists_is_refused() {
    let repository = Repository::new("exists", true, &[]);
    repository.commit("README", "hello, world\n");
    repository.land();
    repo::run(repository.share(), &["branch", &repository.branch()]);
    let records = repository.records();
    let before = repository.objects_and_refs();

    let output = repository.apply();

    let error = format!(
        "the branch {} already exists, so everything stays pending",
        repository.branch()
    );
    repository.stays_pending(&output, &error, &before);
    assert_eq!(repository.records(), records);
}

// spec(SHARE): When the share's root on your host holds a `.git` directory, the agent's commits, and the files it wrote but didn't commit, land as a new branch, `ap/<agent>/<run>`, imported as git objects and checked with `git fsck`.
// spec(SHARE): The files the agent wrote but didn't commit land as one last commit on the branch.
// spec(SHARE): Its author and committer are `Agent+ <ap@localhost>`, its subject is `Uncommitted changes from <agent>/<run>`, and its time is when `ap apply` ran.
// spec(SHARE): The last commit's time is in UTC.
#[test]
fn uncommitted_files_land_as_one_last_commit() {
    // Your repository has commits or none, and the agent commits or doesn't.
    for (name, first, commits) in [
        ("uncommitted", true, true),
        ("uncommitted-only", true, false),
        ("uncommitted-empty", false, false),
        ("uncommitted-empty-commits", false, true),
    ] {
        let old = |share: &Path| {
            repo::commit(share, "old.txt", "old\n", "old");
        };
        let repository = Repository::with(
            name,
            first,
            Setup {
                plain: Some("plain"),
                before: first.then_some(&old as &dyn Fn(&Path)),
                ..Setup::default()
            },
        );
        let (share, pending, guest) = (repository.share(), &repository.pending, &repository.guest);
        if commits {
            repository.commit("docs/guide.md", "read me\n");
            repository.commit("docs/old.md", "gone soon\n");
        }
        repository.land();
        // A file put in, in a directory the agent made, an executable, and
        // a symlink to the file.
        dir(pending, "notes", 0o755);
        write(pending, "notes/new.txt", 0o644, b"new\n");
        fs::create_dir(guest.join("notes")).unwrap();
        fs::write(guest.join("notes/new.txt"), "new\n").unwrap();
        write(pending, "run.sh", 0o755, b"#!/bin/sh\n");
        fs::write(guest.join("run.sh"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(guest.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        link(pending, "latest", "notes/new.txt");
        symlink("notes/new.txt", guest.join("latest")).unwrap();
        // A file changed, and one removed, of yours and of the agent's.
        let mut changed = Vec::new();
        if first {
            changed.push(("README", "old.txt"));
        }
        if commits {
            changed.push(("docs/guide.md", "docs/old.md"));
        }
        for (change_it, remove_it) in changed {
            change(share, pending, change_it, 0o644, b"changed\n");
            fs::write(guest.join(change_it), "changed\n").unwrap();
            delete(share, pending, remove_it);
            fs::remove_file(guest.join(remove_it)).unwrap();
        }
        let tree = repository.tree_after(&[&["add", "-A"]]);

        let (output, during) = repository.apply_timed();

        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        assert_eq!(stdout(&output), "");
        repository.assert_last_commit(&tree, during);
        let files = repo::run(
            share,
            &[
                "ls-tree",
                "-r",
                &repository.branch(),
                "--",
                "run.sh",
                "latest",
            ],
        );
        let blob = repo::run(
            share,
            &["rev-parse", &format!("{}:latest", repository.branch())],
        );
        assert_eq!(
            repo::run(share, &["cat-file", "blob", &blob]),
            "notes/new.txt"
        );
        assert!(files.starts_with("120000 blob "), "{files}");
        assert!(files.contains("\n100755 blob "), "{files}");
        // Your working tree is as it was, and the run's other share lands.
        assert!(!repository.scratch.exists("notes"));
        assert!(repository.scratch.root.join("plain/file").exists());
        if first {
            assert_eq!(repository.scratch.read("README"), "hello\n");
            assert_eq!(repository.scratch.read("old.txt"), "old\n");
        }
    }
}

/// A change on your side to a file the agent never wrote doesn't land in
/// the last commit, which holds the file as the agent's `HEAD` does, but
/// it lands on the branch once the agent commits it.
// spec(SHARE): If the agent commits them, they land on its branch.
// spec(SHARE): A file the agent never changed or removed is in the last commit as the agent's `HEAD` holds it, so a change you made to it during the run is left out.
#[test]
fn your_change_lands_only_if_the_agent_commits_it() {
    for commits in [false, true] {
        let name = ["yours-uncommitted", "yours-committed"][usize::from(commits)];
        let repository = Repository::new(name, true, &[]);
        let share = repository.share();
        fs::write(share.join("README"), "yours\n").unwrap();
        write(&repository.pending, "notes.txt", 0o644, b"mine\n");
        fs::write(repository.guest.join("notes.txt"), "mine\n").unwrap();
        if commits {
            fs::write(repository.guest.join("README"), "yours\n").unwrap();
            repo::run(&repository.guest, &["commit", "-q", "-am", "yours"]);
            repository.land();
        }
        let tree = repository.tree_after(&[&["add", "notes.txt"]]);

        let (output, during) = repository.apply_timed();

        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        repository.assert_last_commit(&tree, during);
        let readme = repo::run(share, &["show", &format!("{}:README", repository.branch())]);
        assert_eq!(readme, ["hello", "yours"][usize::from(commits)]);
    }
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

/// A journal whose last record an `ap apply` was stopped while writing is
/// read without it, and added to after its last whole record, so that the
/// next `ap apply`, stopped too, leaves one the one after can read.
#[test]
fn a_journal_cut_short_is_read_and_added_to() {
    use std::os::unix::process::ExitStatusExt;
    let scratch = Scratch::new("journal-torn");
    let share = &scratch.share;
    let (run, pending) = scratch.run("notes", 1);
    let large = vec![b'x'; 1 << 20];
    write(&pending, "a", 0o644, b"a\n");
    write(&pending, "big", 0o644, &large);
    fs::write(pending.join("landed"), "BASE \"gone\" none\nBASE \"a").unwrap();

    let mut command = scratch.command(&["apply", &run]);
    limit_file_size(&mut command, libc::SIG_DFL);
    let output = command.output().unwrap();

    assert_eq!(output.status.signal(), Some(libc::SIGXFSZ));
    for name in scratch.names("") {
        if name != "a" {
            fs::remove_file(share.join(name)).unwrap();
        }
    }
    let output = scratch.ap(&["apply", &run]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");
    assert_eq!(scratch.read("a"), "a\n");
    assert_eq!(fs::read(share.join("big")).unwrap(), large);
    assert!(!scratch.store().join("pending").join(&run).exists());
}

/// A file a stopped `ap apply` landed is neither read nor written again,
/// even one the agent left so that you can't read it.
#[test]
fn a_file_a_stopped_apply_landed_is_left_as_it_is() {
    let scratch = Scratch::new("journal-unreadable");
    let share = &scratch.share;
    fs::write(share.join("a"), "old\n").unwrap();
    let (run, pending) = scratch.run("notes", 1);
    change(share, &pending, "a", 0o000, b"the agent's\n");
    // What the stopped `ap apply` did, and the journal it left.
    fs::write(share.join("a"), "the agent's\n").unwrap();
    fs::set_permissions(share.join("a"), fs::Permissions::from_mode(0o000)).unwrap();
    fs::write(pending.join("landed"), format!("{}\n", base(share, "a"))).unwrap();

    let output = scratch.ap(&["apply", &run]);

    fs::set_permissions(share.join("a"), fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");
    assert_eq!(scratch.read("a"), "the agent's\n");
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

/// Writes `text` as a commit object in the agent's repository, without the
/// checks git makes, and points its `main` at it.
fn plant_commit(guest: &Path, text: &str) -> String {
    let mut hash = repo::git(guest);
    hash.args([
        "hash-object",
        "-t",
        "commit",
        "-w",
        "--literally",
        "--stdin",
    ]);
    let mut child = (hash.stdin(Stdio::piped()).stdout(Stdio::piped()))
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), text.as_bytes()).unwrap();
    let id = String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap();
    let id = id.trim().to_owned();
    repo::run(guest, &["update-ref", "refs/heads/main", &id]);
    id
}

/// An object of yours that the agent's history names as another type, which
/// `git fsck` doesn't look at, loose or packed.
// spec(SHARE): Before the agent's objects or the branch move in, `ap apply` writes the objects to a quarantine directory and runs your host's `git fsck --strict` on them, against your repository.
// spec(SHARE): If the branch import fails, everything stays pending.
#[test]
fn an_object_of_yours_named_as_another_type_keeps_everything_pending() {
    for packed in [false, true] {
        let name = format!("wrong-type-{packed}");
        let repository = Repository::new(&name, true, &[]);
        let share = repository.share();
        if packed {
            repo::run(share, &["gc", "-q", "--prune=now"]);
        }
        let start = repository.start.as_deref().unwrap();
        let blob = repo::run(share, &["rev-parse", "HEAD:README"]);
        let text = format!(
            "tree {blob}\nparent {start}\nauthor A <a@example.com> 1 +0000\ncommitter C <c@example.com> 1 +0000\n\nwrong\n"
        );
        plant_commit(&repository.guest, &text);
        repository.land();
        let records = repository.records();
        let before = repository.objects_and_refs();

        let output = repository.apply();

        let error = format!(
            "the agent's history names object {blob} as a tree, but it is a blob, so everything stays pending"
        );
        repository.stays_pending(&output, &error, &before);
        assert_eq!(repository.records(), records);
    }
}

/// Git once wrote a symbolic ref as a symlink, and the run's copy of your
/// `.git` keeps one as it is.
#[test]
fn a_head_that_is_a_symlink_still_names_the_branch() {
    let symlinked = |dir: &Path| {
        let link = ["-c", "core.preferSymlinkRefs=true", "symbolic-ref"];
        repo::run(dir, &[&link[..], &["HEAD", "refs/heads/main"]].concat());
        assert!(dir.join(".git/HEAD").is_symlink());
    };
    let repository = Repository::with(
        "symlinked-head",
        true,
        Setup {
            before: Some(&symlinked),
            ..Setup::default()
        },
    );
    assert!(repository.pending.join("git/HEAD").is_symlink());
    symlinked(&repository.guest);
    repository.commit("README", "hello, world\n");
    repository.land();

    let output = repository.apply();

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        repo::history(repository.share(), &repository.branch()),
        repo::history(&repository.guest, "HEAD")
    );
}

/// A file you removed during the run, beneath your directory whose mode
/// the agent changed, is in the last commit as the agent's `HEAD` holds it,
/// and so there is none.
// spec(SHARE): A file the agent never changed or removed is in the last commit as the agent's `HEAD` holds it, so a change you made to it during the run is left out.
#[test]
fn a_file_you_removed_beneath_your_directory_is_left_as_committed() {
    let keep = |share: &Path| {
        repo::commit(share, "docs/keep", "keep\n", "keep");
    };
    let repository = Repository::with(
        "yours-removed",
        true,
        Setup {
            before: Some(&keep),
            ..Setup::default()
        },
    );
    let share = repository.share();
    record(&repository.pending, &base(share, "docs"));
    dir(&repository.pending, "docs", 0o700);
    fs::remove_file(share.join("docs/keep")).unwrap();

    let output = repository.apply();

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!repository.has_branch());
}

/// A file you removed, with its directory, beneath a directory the agent
/// wrote in, then changed the mode of, is in the last commit as the agent's
/// `HEAD` holds it.
// spec(SHARE): A file the agent never changed or removed is in the last commit as the agent's `HEAD` holds it, so a change you made to it during the run is left out.
#[test]
fn a_directory_you_removed_keeps_your_files_in_the_last_commit() {
    let keep = |share: &Path| {
        repo::commit(share, "docs/keep", "keep\n", "keep");
    };
    let repository = Repository::with(
        "yours-removed-dir",
        true,
        Setup {
            before: Some(&keep),
            ..Setup::default()
        },
    );
    let share = repository.share();
    write(&repository.pending, "docs/new", 0o644, b"new\n");
    fs::write(repository.guest.join("docs/new"), "new\n").unwrap();
    fs::remove_dir_all(share.join("docs")).unwrap();
    record(&repository.pending, "BASE \"docs\" none");
    record(&repository.pending, "DIR \"docs\" 700");
    let tree = repository.tree_after(&[&["add", "docs/new"]]);

    let (output, during) = repository.apply_timed();

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    repository.assert_last_commit(&tree, during);
}

/// A file the agent deleted, with its directory, which it made again after
/// you removed yours, is gone from the last commit.
#[test]
fn a_file_the_agent_deleted_with_its_directory_is_gone_from_the_last_commit() {
    let keep = |share: &Path| {
        repo::commit(share, "docs/keep", "keep\n", "keep");
    };
    let repository = Repository::with(
        "agent-removed-dir",
        true,
        Setup {
            before: Some(&keep),
            ..Setup::default()
        },
    );
    let share = repository.share();
    delete(share, &repository.pending, "docs/keep");
    delete(share, &repository.pending, "docs");
    fs::remove_dir_all(share.join("docs")).unwrap();
    dir(&repository.pending, "docs", 0o755);
    let tree = repository.tree_after(&[&["rm", "-q", "--cached", "docs/keep"]]);

    let (output, during) = repository.apply_timed();

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    repository.assert_last_commit(&tree, during);
}

/// A file of yours that the agent replaced with a directory, then deleted
/// with the directory above it, and that you put back, is in the last
/// commit as the agent's `HEAD` holds it, though the agent deleted a file
/// beneath its directory there.
#[test]
fn a_file_you_put_back_where_the_agent_had_a_directory_is_kept() {
    let item = |share: &Path| {
        repo::commit(share, "docs/item", "keep\n", "item");
    };
    let repository = Repository::with(
        "yours-put-back",
        true,
        Setup {
            before: Some(&item),
            ..Setup::default()
        },
    );
    let (share, pending) = (repository.share(), &repository.pending);
    delete(share, pending, "docs/item");
    dir(pending, "docs/item", 0o755);
    write(pending, "docs/item/temp", 0o644, b"temp\n");
    record(pending, "DELETE \"docs/item/temp\"");
    record(pending, "DELETE \"docs/item\"");
    delete(share, pending, "docs");
    fs::remove_dir_all(pending.join("tree/docs")).unwrap();
    fs::remove_dir_all(share.join("docs")).unwrap();
    dir(pending, "docs", 0o755);
    fs::create_dir(share.join("docs")).unwrap();
    fs::write(share.join("docs/item"), "keep\n").unwrap();

    let output = repository.apply();

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!repository.has_branch());
}

/// A directory the agent made where its `HEAD` holds a file is a change it
/// didn't commit, and git keeps no empty directory.
#[test]
fn a_directory_in_place_of_a_committed_file_is_uncommitted() {
    let repository = Repository::new("directory-for-file", true, &[]);
    repository.commit("docs/guide.md", "read me\n");
    repository.land();
    delete(repository.share(), &repository.pending, "README");
    dir(&repository.pending, "README", 0o755);
    let tree = repository.tree_after(&[&["rm", "-q", "--cached", "README"]]);

    let (output, during) = repository.apply_timed();

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    repository.assert_last_commit(&tree, during);
}

/// A branch that git can't write, because a branch of yours is named
/// where its directory goes, keeps the run's other shares pending too.
// spec(SHARE): If the branch import fails, everything stays pending.
#[test]
fn a_branch_that_can_t_be_written_keeps_every_share_pending() {
    let repository = Repository::with(
        "branch-blocked",
        true,
        Setup {
            plain: Some("plain"),
            ..Setup::default()
        },
    );
    repository.commit("README", "hello, world\n");
    repository.land();
    repo::run(repository.share(), &["branch", "ap"]);
    let plain = repository.scratch.root.join("plain");
    let shares = repository
        .scratch
        .runs()
        .pending_shares(&repository.run)
        .unwrap();
    assert_eq!(shares[0].root, plain);
    let plain_records = fs::read(shares[0].dir.join("records")).unwrap();
    let records = repository.records();
    let refs = repo::run(repository.share(), &["for-each-ref"]);

    let output = repository.apply();

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let share = repository.share().display();
    let error = stderr(&output);
    let expected = format!(
        "ap: run {}: {share}: git update-ref failed: ",
        repository.run
    );
    assert!(error.starts_with(&expected), "{error}");
    assert!(
        error.ends_with(", so everything stays pending\n"),
        "{error}"
    );
    assert!(!plain.join("file").exists());
    assert_eq!(
        fs::read(shares[0].dir.join("records")).unwrap(),
        plain_records
    );
    assert_eq!(repository.records(), records);
    assert_eq!(repo::run(repository.share(), &["for-each-ref"]), refs);
}

/// A run with two repositories, where the second branch can't be written,
/// takes back the first.
// spec(SHARE): If the branch import fails, everything stays pending.
#[test]
fn a_second_branch_that_can_t_be_written_takes_back_the_first() {
    let scratch = Scratch::new("two-branches");
    let other = scratch.root.join("other");
    let runs = scratch.runs();
    let run = runs.create("notes", 1).unwrap().name;
    runs.create_pending(&run).unwrap();
    let mut repositories = Vec::new();
    for (root, guest) in [(&scratch.share, "guest"), (&other, "other-guest")] {
        fs::create_dir_all(root).unwrap();
        repo::run(root, &["init", "-q"]);
        repo::commit(root, "README", "hello\n", "start");
        let pending = runs.create_pending_share(&run, root).unwrap();
        fs::create_dir(pending.join("tree")).unwrap();
        fs::write(pending.join("records"), "").unwrap();
        repo::capture(root, &pending);
        let guest = scratch.root.join(guest);
        repo::guest(root, &guest);
        repo::commit(&guest, "a", "a\n", "one");
        write(&pending, "a", 0o644, b"a\n");
        repo::land(&guest, root, &pending);
        repositories.push((root.clone(), pending));
    }
    repo::run(&other, &["branch", "ap"]);
    let before: Vec<_> = (repositories.iter())
        .map(|(root, pending)| {
            let refs = repo::run(root, &["for-each-ref"]);
            (refs, fs::read(pending.join("records")).unwrap())
        })
        .collect();

    let output = (scratch.command(&["apply", &run]))
        .env("PATH", std::env::var_os("PATH").unwrap())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let expected = format!(
        "ap: run {run}: {}: git update-ref failed: ",
        other.display()
    );
    assert!(
        stderr(&output).starts_with(&expected),
        "{}",
        stderr(&output)
    );
    let after: Vec<_> = (repositories.iter())
        .map(|(root, pending)| {
            let refs = repo::run(root, &["for-each-ref"]);
            (refs, fs::read(pending.join("records")).unwrap())
        })
        .collect();
    assert_eq!(after, before);
}

/// A file or a directory the agent committed, then deleted without
/// committing that, is a change it didn't commit, though you never had it,
/// and so is a directory it made again without what it committed there.
#[test]
fn a_committed_file_deleted_since_is_uncommitted() {
    let cases = [
        ("deleted-file", &["DELETE \"a\""][..]),
        (
            "deleted-directory",
            &["DELETE \"docs/guide.md\"", "DELETE \"docs\""],
        ),
        // The agent makes the directory again, empty, with and without
        // hiding yours.
        ("remade-directory", &["DELETE \"docs\"", "DIR \"docs\" 755"]),
        (
            "remade-opaque",
            &["DELETE \"docs\"", "DIR \"docs\" 755 opaque"],
        ),
    ];
    for (name, lines) in cases {
        let repository = Repository::new(name, true, &[]);
        repository.commit("a", "a\n");
        repository.commit("docs/guide.md", "read me\n");
        repository.land();
        for line in lines {
            record(&repository.pending, line);
        }
        let gone = match name {
            "deleted-file" => "a",
            _ => "docs",
        };
        let tree = repository.tree_after(&[&["rm", "-q", "-r", "--cached", gone]]);

        let (output, during) = repository.apply_timed();

        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        repository.assert_last_commit(&tree, during);
    }
}

/// When a branch that was written can't be taken back, `ap apply` says
/// which one stays.
#[test]
fn a_branch_that_can_t_be_taken_back_is_named() {
    let scratch = Scratch::new("rollback-refused");
    let other = scratch.root.join("other");
    let runs = scratch.runs();
    let run = runs.create("notes", 1).unwrap().name;
    runs.create_pending(&run).unwrap();
    for (root, guest) in [(&scratch.share, "guest"), (&other, "other-guest")] {
        fs::create_dir_all(root).unwrap();
        repo::run(root, &["init", "-q"]);
        repo::commit(root, "README", "hello\n", "start");
        let pending = runs.create_pending_share(&run, root).unwrap();
        fs::create_dir(pending.join("tree")).unwrap();
        fs::write(pending.join("records"), "").unwrap();
        repo::capture(root, &pending);
        let guest = scratch.root.join(guest);
        repo::guest(root, &guest);
        repo::commit(&guest, "a", "a\n", "one");
        write(&pending, "a", 0o644, b"a\n");
        repo::land(&guest, root, &pending);
    }
    repo::run(&other, &["branch", "ap"]);
    // Your hook refuses to delete a branch.
    let hook = scratch.share.join(".git/hooks/reference-transaction");
    let zero = "0".repeat(40);
    let script = format!(
        "#!/bin/sh\nwhile read old new ref; do [ \"$new\" = {zero} ] && exit 1; done\nexit 0\n"
    );
    fs::write(&hook, script).unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();

    let output = (scratch.command(&["apply", &run]))
        .env("PATH", std::env::var_os("PATH").unwrap())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let error = stderr(&output);
    let stays = format!(
        "the branch ap/notes/{run} stays in {}, because git update-ref couldn't remove it: ",
        scratch.share.display()
    );
    assert!(error.contains(&stays), "{error}");
    assert!(
        error.ends_with(", so everything stays pending\n"),
        "{error}"
    );
    assert!(scratch.runs().has_pending(&run).unwrap());
}

/// The agent commits a submodule's commit at `deps/vendor`, and `docs`
/// with a file your `.gitignore` names.
fn with_submodule_and_ignored(name: &str) -> Repository {
    let repository = Repository::new(name, true, &[]);
    fs::write(repository.share().join(".gitignore"), "*.log\n").unwrap();
    let guest = &repository.guest;
    let start = repository.start.as_deref().unwrap();
    let gitlink = format!("160000,{start},deps/vendor");
    repo::run(guest, &["update-index", "--add", "--cacheinfo", &gitlink]);
    fs::create_dir_all(guest.join("deps/vendor")).unwrap();
    repository.commit("docs/guide.md", "read me\n");
    fs::write(guest.join("docs/debug.log"), "noise\n").unwrap();
    repo::run(guest, &["add", "-f", "docs/debug.log"]);
    repo::run(guest, &["commit", "-q", "-m", "log"]);
    write(&repository.pending, "docs/debug.log", 0o644, b"noise\n");
    dir(&repository.pending, "deps", 0o755);
    dir(&repository.pending, "deps/vendor", 0o755);
    repository.land();
    repository
}

/// A submodule's directory gone from a directory the agent made again is
/// a change it didn't commit.
#[test]
fn a_submodule_gone_from_a_directory_made_again_is_uncommitted() {
    let repository = with_submodule_and_ignored("submodule-gone");
    record(&repository.pending, "DELETE \"deps\"");
    record(&repository.pending, "DIR \"deps\" 755");
    let tree = repository.tree_after(&[&["rm", "-q", "--cached", "deps/vendor"]]);

    let (output, during) = repository.apply_timed();

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    repository.assert_last_commit(&tree, during);
}

/// A submodule's directory, and a git-ignored file the agent deleted in a
/// directory whose mode it changed, are as its `HEAD` holds them.
// spec(SHARE): When nothing is uncommitted, there is no such commit.
#[test]
fn a_submodule_s_directory_and_an_ignored_file_land_as_committed() {
    let repository = with_submodule_and_ignored("submodule-kept");
    record(&repository.pending, "DELETE \"docs/debug.log\"");
    record(&repository.pending, "DIR \"docs\" 700");

    let output = repository.apply();

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        repo::history(repository.share(), &repository.branch()),
        repo::history(&repository.guest, "HEAD")
    );
}

/// A directory the agent deleted, which held only git-ignored files in its
/// `HEAD`, leaves out what was in it, as each file is left out.
// spec(SHARE): When nothing is uncommitted, there is no such commit.
#[test]
fn a_deleted_directory_of_ignored_files_lands_as_committed() {
    let repository = Repository::new("ignored-directory", true, &[]);
    fs::write(repository.share().join(".gitignore"), "*.log\n").unwrap();
    let guest = &repository.guest;
    fs::create_dir(guest.join("logs")).unwrap();
    fs::write(guest.join("logs/debug.log"), "noise\n").unwrap();
    repo::run(guest, &["add", "-f", "logs/debug.log"]);
    repo::run(guest, &["commit", "-q", "-m", "log"]);
    write(&repository.pending, "logs/debug.log", 0o644, b"noise\n");
    repository.land();
    record(&repository.pending, "DELETE \"logs\"");

    let output = repository.apply();

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        repo::history(repository.share(), &repository.branch()),
        repo::history(guest, "HEAD")
    );
}

/// A file and a symlink the agent wrote, each in place of a directory its
/// `HEAD` holds with a git-ignored file in it, are skipped, because git
/// can't hold a file and a directory at one path. Each directory is in the
/// last commit as if the agent had deleted it, so the file it deleted
/// beside the ignored one is gone. A file in place of a directory with no
/// ignored file in it lands.
// spec(SHARE): Where the agent's `HEAD` holds a directory with a git-ignored file in it, and the agent wrote a file or symlink in its place, `ap apply` skips that file and says why.
// spec(SHARE): The last commit holds the directory as if the agent had deleted it, so its git-ignored files are as that `HEAD` holds them.
#[test]
fn a_file_in_place_of_a_directory_with_an_ignored_file_is_skipped() {
    let files = [
        "logs/debug.log",
        "logs/README",
        "var/cache/deep/run.log",
        "plain/a",
    ];
    let ignored = |share: &Path| {
        fs::write(share.join(".gitignore"), "*.log\n").unwrap();
        for file in files {
            fs::create_dir_all(share.join(file).parent().unwrap()).unwrap();
            fs::write(share.join(file), "noise\n").unwrap();
            repo::run(share, &["add", "-f", file]);
        }
        repo::run(share, &["commit", "-q", "-m", "files"]);
    };
    let repository = Repository::with(
        "file-for-ignored",
        true,
        Setup {
            before: Some(&ignored),
            ..Setup::default()
        },
    );
    let (share, pending) = (repository.share(), &repository.pending);
    for file in files {
        delete(share, pending, file);
    }
    for dir in ["logs", "var/cache", "plain"] {
        delete(share, pending, dir);
    }
    write(pending, "logs", 0o644, b"one file\n");
    fs::create_dir(pending.join("tree/var")).unwrap();
    link(pending, "var/cache", "../README");
    write(pending, "plain", 0o644, b"plain\n");
    let guest = &repository.guest;
    fs::remove_dir_all(guest.join("plain")).unwrap();
    fs::write(guest.join("plain"), "plain\n").unwrap();
    let tree = repository.tree_after(&[
        &["rm", "-q", "--cached", "logs/README", "plain/a"],
        &["add", "plain"],
    ]);

    let (output, during) = repository.apply_timed();

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let why = "it would replace a directory that holds a git-ignored file";
    assert_eq!(
        stdout(&output),
        format!("skipped logs: {why}\nskipped var/cache: {why}\nskipped 2 git-ignored files\n")
    );
    repository.assert_last_commit(&tree, during);
}

/// A submodule that a pattern for directories names isn't a git-ignored
/// file, as `ap apply` reads it, so a file the agent wrote in place of the
/// directory that holds it lands.
#[test]
fn a_file_in_place_of_a_directory_with_a_submodule_lands() {
    let repository = Repository::new("file-for-submodule", true, &[]);
    fs::write(repository.share().join(".gitignore"), "vendor/\n").unwrap();
    let guest = &repository.guest;
    let start = repository.start.as_deref().unwrap();
    let gitlink = format!("160000,{start},deps/vendor");
    repo::run(guest, &["update-index", "--add", "--cacheinfo", &gitlink]);
    repo::run(guest, &["commit", "-q", "-m", "vendor"]);
    repository.land();
    let pending = &repository.pending;
    record(pending, "DELETE \"deps\"");
    write(pending, "deps", 0o644, b"deps\n");
    fs::write(guest.join("deps"), "deps\n").unwrap();
    let tree = repository.tree_after(&[&["rm", "-q", "--cached", "deps/vendor"], &["add", "deps"]]);

    let (output, during) = repository.apply_timed();

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");
    repository.assert_last_commit(&tree, during);
}

/// From #714: the agent's `HEAD` holds `big`, whose 41 trees each name the
/// one below twice, so that it holds 2^40 files, or `wide`, whose 4 trees
/// name the one below 32 or 60 times, so that it holds about 7 million
/// files, each path 12 bytes long. Beneath a directory the agent deleted,
/// or replaced with a file, `ap apply` reads at most 64 MiB of paths, and
/// at most a million, then refuses, within a fixed time.
// spec(SHARE): `ap apply` reads it there, and then reads it again, there and beneath each directory the agent deleted.
// spec(SHARE): Each time, it reads at most a million paths, and at most 64 MiB of paths in all, and none longer than 1024 bytes.
// spec(SHARE): Past any of these limits, or at such an entry, `ap diff`'s first line says why, and `ap apply` refuses the branch import, says why, and everything stays pending.
#[test]
fn a_head_with_too_many_paths_beneath_what_the_agent_removed_stays_pending() {
    let cases = [("big", "the 64 MiB of"), ("wide", "the million")];
    for ((name, limit), replaced) in cases.into_iter().flat_map(|c| [(c, false), (c, true)]) {
        let repository = Repository::new(&format!("too-many-{name}-{replaced}"), true, &[]);
        match name {
            "big" => repo::commit_stacked(&repository.guest, name, [true; 40]),
            _ => repo::commit_wide(&repository.guest, name, &[60, 60, 60, 32]),
        }
        repository.land();
        let pending = &repository.pending;
        record(pending, &format!("DELETE \"{name}\""));
        if replaced {
            write(pending, name, 0o644, b"a file\n");
        }
        let before = repository.objects_and_refs();

        let output = repository.apply_within(20);

        let why = format!("the agent's HEAD holds more than {limit} paths that `ap` reads");
        let error = format!("{why}, so everything stays pending");
        repository.stays_pending(&output, &error, &before);
    }
}

/// From #694: the agent commits `big`, a byte more than 512 MiB of zeros,
/// which its git compresses to about half a MiB, loose or packed. Or it
/// packs its commits, and the index of that pack is a byte more than 512
/// MiB long, which costs almost nothing as a sparse file. Or it packs its
/// commits twice, and each index is a byte more than 256 MiB long. `ap
/// apply` refuses each before it reads it, within a fixed time.
// spec(SHARE): `ap diff` and `ap apply` read none of the agent's objects or pack indexes larger than 512 MiB.
// spec(SHARE): At a larger one, `ap diff`'s first line says why, and `ap apply` refuses the branch import, says why, and everything stays pending.
// spec(SHARE): They do the same when the agent's pack indexes are larger than 512 MiB in all, before they read one.
#[test]
fn an_object_or_pack_index_larger_than_the_limit_stays_pending() {
    const LARGER: u64 = (512 << 20) + 1;
    for case in ["loose", "packed", "index", "indexes"] {
        let repository = Repository::new(&format!("too-large-{case}"), true, &[]);
        let guest = &repository.guest;
        let packs = || {
            let dir = fs::read_dir(guest.join(".git/objects/pack"));
            let names = dir
                .into_iter()
                .flatten()
                .map(|entry| entry.unwrap().file_name());
            names.collect::<std::collections::BTreeSet<_>>()
        };
        let earlier = packs();
        let blob = match case {
            "index" | "indexes" => {
                repository.commit("a", "a\n");
                repo::run(guest, &["repack", "-q", "-d"]);
                if case == "indexes" {
                    repository.commit("b", "b\n");
                    repo::run(guest, &["repack", "-q", "-d"]);
                }
                String::new()
            }
            _ => repo::commit_zeros(guest, "big", LARGER, case == "loose"),
        };
        let written = packs().difference(&earlier).cloned().collect::<Vec<_>>();
        let pack = repository.pending.join("tree/.git/objects/pack");
        let indexes = written
            .iter()
            .filter(|name| name.to_str().unwrap().ends_with(".idx"))
            .map(|idx| pack.join(idx))
            .collect::<Vec<_>>();
        let idx = indexes.first().cloned();
        repository.land();
        if case.starts_with("index") {
            let each = (512 << 20) / indexes.len() as u64 + 1;
            for idx in &indexes {
                let idx = fs::OpenOptions::new().write(true).open(idx);
                idx.unwrap().set_len(each).unwrap();
            }
        }
        let before = repository.objects_and_refs();

        let output = repository.apply_within(2);

        let why = match case {
            "loose" => format!("{blob} is larger than the limit"),
            "packed" => {
                let idx = idx.unwrap().display().to_string();
                format!("{idx} has an entry at 12 larger than the limit")
            }
            "index" => format!("{} is larger than the limit", idx.unwrap().display()),
            _ => format!(
                "the pack indexes in {} are larger than the limit in all",
                pack.display()
            ),
        };
        let error = format!("{why}, so everything stays pending");
        repository.stays_pending(&output, &error, &before);
    }
}

/// From #714: the agent's `HEAD` holds a file 1024 bytes long beneath
/// `deep`, or one a byte longer beneath `deep1`. Beneath a directory the
/// agent deleted, `ap apply` reads the first, and refuses the second.
// spec(SHARE): `ap apply` reads it there, and then reads it again, there and beneath each directory the agent deleted.
// spec(SHARE): Each time, it reads at most a million paths, and at most 64 MiB of paths in all, and none longer than 1024 bytes.
// spec(SHARE): Past any of these limits, or at such an entry, `ap diff`'s first line says why, and `ap apply` refuses the branch import, says why, and everything stays pending.
#[test]
fn a_head_with_a_path_too_long_beneath_what_the_agent_removed_stays_pending() {
    for name in ["deep", "deep1"] {
        let repository = Repository::new(&format!("too-long-{name}"), true, &[]);
        repo::commit_stacked(&repository.guest, name, [false; 509]);
        repository.land();
        record(&repository.pending, &format!("DELETE \"{name}\""));
        let before = repository.objects_and_refs();
        // The agent's index never held it.
        let tree = repository.tree_after(&[]);

        let (output, during) = repository.apply_timed();

        if name == "deep" {
            assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
            repository.assert_last_commit(&tree, during);
            continue;
        }
        let why = "the agent's HEAD holds a path longer than the 1024 bytes that `ap` reads";
        let error = format!("{why}, so everything stays pending");
        repository.stays_pending(&output, &error, &before);
    }
}

/// From #714: the root of the agent's `HEAD`, which holds the paths it
/// changed, holds a tree entry with a name that `git fsck --strict`
/// refuses, which `ap apply` refuses before it runs `git fsck`.
// spec(SHARE): `ap apply` reads it there, and then reads it again, there and beneath each directory the agent deleted.
// spec(SHARE): It reads no tree entry whose name is empty, `.`, or `..`, or holds a `/`, which `git fsck --strict` refuses.
// spec(SHARE): Past any of these limits, or at such an entry, `ap diff`'s first line says why, and `ap apply` refuses the branch import, says why, and everything stays pending.
#[test]
fn a_head_with_a_name_git_refuses_stays_pending() {
    for (case, name) in [
        ("empty", ""),
        ("dot", "."),
        ("dots", ".."),
        ("slash", "a/b"),
    ] {
        let repository = Repository::new(&format!("named-{case}"), true, &[]);
        repo::commit_named(&repository.guest, name.as_bytes());
        repository.land();
        let before = repository.objects_and_refs();

        let output = repository.apply();

        let why = "the agent's HEAD holds a tree entry with a name that git refuses";
        let error = format!("{why}, so everything stays pending");
        repository.stays_pending(&output, &error, &before);
    }
}

/// From #714, from review: the agent's `HEAD` holds `d`, a chain of 400
/// trees above 13 that each name the one below twice, so that it holds
/// 2^13 files, each path 829 bytes long, with about 20 MiB of paths in all,
/// within both limits. Your `.gitignore` has 30 patterns, none of which
/// matches. A path costs time in its length, not its depth squared, so a
/// directory the agent deleted, or replaced with a file, lands within a
/// fixed time.
// spec(SHARE): `ap apply` reads it there, and then reads it again, there and beneath each directory the agent deleted.
#[test]
fn a_deep_tree_within_the_limits_lands_within_a_fixed_time() {
    let mut patterns: Vec<String> = (0..10).map(|n| format!("*.tmp{n}\n")).collect();
    patterns.extend((0..10).map(|n| format!("/build{n}/\n")));
    patterns.extend((0..10).map(|n| format!("**/cache{n}/**\n")));
    let gitignore = patterns.concat();
    for replaced in [false, true] {
        let ignored = |share: &Path| {
            fs::write(share.join(".gitignore"), &gitignore).unwrap();
            repo::run(share, &["add", ".gitignore"]);
            repo::run(share, &["commit", "-q", "-m", "ignore"]);
        };
        let repository = Repository::with(
            &format!("deep-within-{replaced}"),
            true,
            Setup {
                before: Some(&ignored),
                ..Setup::default()
            },
        );
        let guest = &repository.guest;
        let twice = std::iter::repeat_n(true, 13).chain(std::iter::repeat_n(false, 400));
        repo::commit_stacked(guest, "d", twice);
        repository.land();
        let pending = &repository.pending;
        record(pending, "DELETE \"d\"");
        let mut change: Vec<&[&str]> = Vec::new();
        if replaced {
            write(pending, "d", 0o644, b"d\n");
            fs::write(guest.join("d"), "d\n").unwrap();
            change.push(&["add", "d"]);
        }
        let tree = repository.tree_after(&change);

        let from = std::time::SystemTime::now();
        let output = repository.apply_within(20);
        let to = std::time::SystemTime::now();

        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        assert_eq!(stdout(&output), "");
        let seconds = |time: std::time::SystemTime| {
            time.duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
        };
        repository.assert_last_commit(&tree, (seconds(from), seconds(to)));
    }
}

/// From #714, from review: the agent's `HEAD` holds `d`, as in
/// `a_deep_tree_within_the_limits_lands_within_a_fixed_time` but with 2^14
/// files and about 40 MiB of paths, and the agent replaced it with a file.
/// Only the last file `ap apply` reads beneath it is git-ignored, so it
/// reads all of `d` to find it, and then all of it again for the last
/// commit. Each read has the limit to itself, so it lands.
// spec(SHARE): `ap apply` reads it there, and then reads it again, there and beneath each directory the agent deleted.
// spec(SHARE): Each time, it reads at most a million paths, and at most 64 MiB of paths in all, and none longer than 1024 bytes.
#[test]
fn each_read_of_the_head_has_the_limit_to_itself() {
    let last = format!("d/{}{}f", "a/".repeat(400), "b/".repeat(14));
    let ignored = |share: &Path| {
        fs::write(share.join(".gitignore"), format!("/{last}\n")).unwrap();
        repo::run(share, &["add", ".gitignore"]);
        repo::run(share, &["commit", "-q", "-m", "ignore"]);
    };
    let repository = Repository::with(
        "read-twice",
        true,
        Setup {
            before: Some(&ignored),
            ..Setup::default()
        },
    );
    let guest = &repository.guest;
    let twice = std::iter::repeat_n(true, 14).chain(std::iter::repeat_n(false, 400));
    repo::commit_stacked(guest, "d", twice);
    repository.land();
    let pending = &repository.pending;
    record(pending, "DELETE \"d\"");
    write(pending, "d", 0o644, b"d\n");
    // The last commit holds `d` as if the agent had deleted it, so only its
    // git-ignored file is left.
    let readme = repo::run(guest, &["rev-parse", "HEAD:README"]);
    let kept = format!("100644,{readme},{last}");
    let tree = repository.tree_after(&[&["update-index", "--add", "--cacheinfo", &kept]]);

    let from = std::time::SystemTime::now();
    let output = repository.apply_within(20);
    let to = std::time::SystemTime::now();

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let why = "it would replace a directory that holds a git-ignored file";
    assert_eq!(stdout(&output), format!("skipped d: {why}\n"));
    let seconds = |time: std::time::SystemTime| {
        time.duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    };
    repository.assert_last_commit(&tree, (seconds(from), seconds(to)));
}

/// From #714, from review: your `.gitignore` names `logs/`, a directory,
/// and the agent replaced `logs` with a file. Its `HEAD` holds `logs/a`,
/// which git ignores because its directory is ignored, though the file
/// `logs` isn't, so `ap apply` skips that file and says why.
// spec(SHARE): Where the agent's `HEAD` holds a directory with a git-ignored file in it, and the agent wrote a file or symlink in its place, `ap apply` skips that file and says why.
#[test]
fn a_file_in_place_of_an_ignored_directory_is_skipped() {
    let ignored = |share: &Path| {
        fs::write(share.join(".gitignore"), "logs/\n").unwrap();
        fs::create_dir(share.join("logs")).unwrap();
        fs::write(share.join("logs/a"), "noise\n").unwrap();
        repo::run(share, &["add", "-f", ".gitignore", "logs/a"]);
        repo::run(share, &["commit", "-q", "-m", "logs"]);
    };
    let repository = Repository::with(
        "file-for-ignored-directory",
        true,
        Setup {
            before: Some(&ignored),
            ..Setup::default()
        },
    );
    let (share, pending) = (repository.share(), &repository.pending);
    delete(share, pending, "logs/a");
    delete(share, pending, "logs");
    write(pending, "logs", 0o644, b"one file\n");

    let output = repository.apply();

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let why = "it would replace a directory that holds a git-ignored file";
    assert_eq!(
        stdout(&output),
        format!("skipped logs: {why}\nskipped 1 git-ignored file\n")
    );
    assert!(!repository.has_branch());
}

/// From #713: your `.gitignore` ignores `*.log` files, but no directory,
/// and the agent's `HEAD` tracks `foo.log`, `docs/old.log`, and
/// `mine.log`. The agent replaced the first two with directories of files
/// git doesn't ignore, and wrote a file in `mine.log`, which you had made
/// a directory before the run. Git can't hold a file and a directory at
/// one path, so each git-ignored file stays as that `HEAD` holds it, and
/// `ap apply` skips the files and symlinks in those directories and says
/// why. A file the agent wrote elsewhere lands.
// spec(SHARE): Where the agent's `HEAD` holds a git-ignored file, and a directory is in its place in the share, `ap apply` skips each file and symlink the agent wrote in that directory, and says why.
// spec(SHARE): The last commit holds the git-ignored file as that `HEAD` holds it.
#[test]
fn a_directory_in_place_of_an_ignored_file_is_skipped() {
    let ignored = |share: &Path| {
        fs::write(share.join(".gitignore"), "*.log\n!*/\n").unwrap();
        fs::create_dir(share.join("docs")).unwrap();
        let files = [".gitignore", "foo.log", "docs/old.log", "mine.log"];
        for file in &files[1..] {
            fs::write(share.join(file), "kept\n").unwrap();
        }
        repo::run(share, &[&["add", "-f"][..], &files].concat());
        repo::run(share, &["commit", "-q", "-m", "ignored"]);
        fs::remove_file(share.join("mine.log")).unwrap();
        fs::create_dir(share.join("mine.log")).unwrap();
    };
    let repository = Repository::with(
        "directory-for-ignored",
        true,
        Setup {
            before: Some(&ignored),
            ..Setup::default()
        },
    );
    let (share, pending) = (repository.share(), &repository.pending);
    delete(share, pending, "foo.log");
    dir(pending, "foo.log", 0o755);
    // As the share server records a directory made where a file was.
    delete(share, pending, "docs/old.log");
    fs::create_dir_all(pending.join("tree/docs/old.log")).unwrap();
    record(pending, "DIR \"docs/old.log\" 755 opaque");
    write(pending, "foo.log/bar", 0o644, b"bar\n");
    dir(pending, "docs/old.log/a", 0o755);
    write(pending, "docs/old.log/a/b", 0o644, b"b\n");
    link(pending, "docs/old.log/a/c", "b");
    write(pending, "mine.log/new", 0o644, b"new\n");
    write(pending, "notes", 0o644, b"notes\n");
    let guest = &repository.guest;
    fs::write(guest.join("notes"), "notes\n").unwrap();
    let tree = repository.tree_after(&[&["add", "notes"]]);

    let (output, during) = repository.apply_timed();

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let why = "it is in a directory that would replace a git-ignored file";
    assert_eq!(
        stdout(&output),
        format!(
            "skipped docs/old.log/a/b: {why}\nskipped docs/old.log/a/c: {why}\n\
             skipped foo.log/bar: {why}\nskipped mine.log/new: {why}\n\
             skipped 2 git-ignored files\n"
        )
    );
    repository.assert_last_commit(&tree, during);
    let kept = format!("{}:docs/old.log", repository.branch());
    assert_eq!(repo::run(share, &["cat-file", "-p", &kept]), "kept");
}
