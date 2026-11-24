//! `ap diff` and `ap discard`, from SPEC.md's `SHARE` section. Nothing here
//! boots: a test writes a run's pending directory the way the share server
//! does, as its `cow` module describes, and makes a run active the way a run
//! process does, by holding the lock on its record.
//!
//! The hunks are checked against the `git` on the host: `ap diff` must print
//! what `git diff --no-index` prints for the same two files, and any
//! difference fails the test.

mod common;

use common::repo;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
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
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("diff")
            .join(name);
        let _ = fs::remove_dir_all(&root);
        let home = root.join("home");
        let share = root.join("share");
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

    /// Runs `ap` with only `HOME` in its environment and no terminal.
    fn ap(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ap"))
            .args(args)
            .current_dir(&self.root)
            .env_clear()
            .env("HOME", &self.home)
            .stdin(Stdio::null())
            .output()
            .unwrap()
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
}

/// Puts the agent's copy of `path` in the tree, and records it.
fn write(pending: &Path, path: &str, mode: u32, data: &[u8]) {
    let file = pending.join("tree").join(path);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, data).unwrap();
    record(pending, &format!("FILE \"{path}\" {mode:o}"));
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

/// `git` with no config but its defaults.
fn git() -> Command {
    let mut git = Command::new("git");
    git.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_EXTERNAL_DIFF");
    git
}

/// What `git diff --no-index` prints for the two files, from the first
/// hunk on.
fn git_hunks(dir: &Path, old: &[u8], new: &[u8]) -> Vec<u8> {
    let (a, b) = (dir.join("old"), dir.join("new"));
    fs::write(&a, old).unwrap();
    fs::write(&b, new).unwrap();
    let output = git()
        .args(["diff", "--no-index", "--no-color", "--no-ext-diff"])
        .arg(&a)
        .arg(&b)
        .output()
        .unwrap();
    let text = output.stdout;
    match text.windows(3).position(|window| window == b"\n@@") {
        Some(at) => text[at + 1..].to_vec(),
        None => Vec::new(),
    }
}

/// A small generator, so a failing case is reproduced by its number.
struct Random(u64);

impl Random {
    fn below(&mut self, limit: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % limit as u64) as usize
    }
}

/// Lines that slide, indent, and repeat, so the cases reach git's
/// heuristics and the places where two diffs are both shortest.
const LINES: [&str; 11] = [
    "a\n",
    "b\n",
    "}\n",
    "\n",
    "fn x() {\n",
    "    c\n",
    "    d\n",
    "\tif y {\n",
    "   \n",
    "\x0c\n",
    "\x0b \n",
];

/// A text of up to `len` lines from `LINES`, and maybe a last line with no
/// newline.
fn text(random: &mut Random, len: usize) -> Vec<u8> {
    let mut text = Vec::new();
    for _ in 0..random.below(len + 1) {
        text.extend(LINES[random.below(LINES.len())].as_bytes());
    }
    if random.below(4) == 0 {
        text.extend(b"end");
    }
    text
}

/// `text` with a few lines removed, added, or replaced.
fn edit(random: &mut Random, text: &[u8], edits: usize) -> Vec<u8> {
    let mut lines: Vec<Vec<u8>> = (text.split_inclusive(|&byte| byte == b'\n'))
        .map(<[u8]>::to_vec)
        .collect();
    for _ in 0..random.below(edits) + 1 {
        let at = random.below(lines.len() + 1);
        let line = LINES[random.below(LINES.len())].as_bytes().to_vec();
        match random.below(3) {
            0 if at < lines.len() => drop(lines.remove(at)),
            1 => lines.insert(at, line),
            _ if at < lines.len() => lines[at] = line,
            _ => {}
        }
    }
    // Only the last line may lack its newline.
    let last = lines.len().saturating_sub(1);
    for line in &mut lines[..last] {
        if !line.ends_with(b"\n") {
            line.push(b'\n');
        }
    }
    lines.concat()
}

fn same_as_git(dir: &Path, case: usize, old: &[u8], new: &[u8]) {
    let ours = ap::unified::hunks(old, new);
    let theirs = git_hunks(dir, old, new);
    assert!(
        ours == theirs,
        "case {case} differs from git\nold: {:?}\nnew: {:?}\nours:\n{}\ngit's:\n{}",
        String::from_utf8_lossy(old),
        String::from_utf8_lossy(new),
        String::from_utf8_lossy(&ours),
        String::from_utf8_lossy(&theirs),
    );
}

// spec(SHARE): It prints a unified diff of each changed text file against your file as it is now, in the form `git diff` prints.
#[test]
fn hunks_are_the_ones_git_prints() {
    let scratch = Scratch::new("oracle");
    let mut random = Random(0x5eed_0fa9);
    for case in 0..1500 {
        let old = text(&mut random, 30);
        let new = match random.below(8) {
            0 => text(&mut random, 30),
            _ => edit(&mut random, &old, 4),
        };
        same_as_git(&scratch.root, case, &old, &new);
    }
}

/// Large texts with unique lines and moved ones, where git gives up on the
/// shortest diff past a cost, and takes a long run of matching lines as a
/// good place to split.
#[test]
fn hunks_of_large_files_are_the_ones_git_prints() {
    let scratch = Scratch::new("oracle-large");
    let mut random = Random(0x0b16_f11e);
    for (case, lines) in [(0, 9_000), (1, 20_000), (2, 150_000)] {
        let old: Vec<Vec<u8>> = (0..lines)
            .map(|i| match random.below(3) {
                0 => LINES[random.below(LINES.len())].as_bytes().to_vec(),
                _ => format!("line {i}\n").into_bytes(),
            })
            .collect();
        let mut new = old.clone();
        for _ in 0..lines / 25 {
            let at = random.below(new.len());
            let moved = old[random.below(old.len())].clone();
            match random.below(2) {
                0 => new.insert(at, moved),
                _ => new[at] = moved,
            }
        }
        same_as_git(&scratch.root, case, &old.concat(), &new.concat());
    }
}

// spec(SHARE): `ap diff <agent | run>` shows what changed, during a run or after it.
// spec(SHARE): It uses no pager and no color.
#[test]
fn diff_shows_changed_added_and_deleted_text_files() {
    let scratch = Scratch::new("text");
    let old = "fn main() {\n    one();\n    two();\n    three();\n}\n";
    let new = "fn main() {\n    one();\n    2();\n    three();\n}\n";
    fs::create_dir(scratch.share.join("src")).unwrap();
    fs::write(scratch.share.join("src/main.rs"), old).unwrap();
    fs::write(scratch.share.join("gone.txt"), "bye\n").unwrap();
    fs::write(scratch.share.join("same.txt"), "same\n").unwrap();
    let (run, pending) = scratch.run("notes", 1);
    record(&pending, &base(&scratch.share, "src/main.rs"));
    write(&pending, "src/main.rs", 0o644, new.as_bytes());
    record(&pending, &base(&scratch.share, "new.txt"));
    write(&pending, "new.txt", 0o644, b"hello\n");
    record(&pending, &base(&scratch.share, "gone.txt"));
    record(&pending, "DELETE \"gone.txt\"");
    // Written again, with the same bytes, it hasn't changed.
    record(&pending, &base(&scratch.share, "same.txt"));
    write(&pending, "same.txt", 0o644, b"same\n");
    // A run that is active shows its changes too.
    let _held = scratch.runs().hold(&run).unwrap();

    let output = scratch.ap(&["diff", &run]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stderr(&output), "");
    let hunks = git_hunks(&scratch.root, old.as_bytes(), new.as_bytes());
    let expected = format!(
        concat!(
            "diff --git a/gone.txt b/gone.txt\n",
            "deleted file mode 100644\n",
            "--- a/gone.txt\n",
            "+++ /dev/null\n",
            "@@ -1 +0,0 @@\n",
            "-bye\n",
            "diff --git a/new.txt b/new.txt\n",
            "new file mode 100644\n",
            "--- /dev/null\n",
            "+++ b/new.txt\n",
            "@@ -0,0 +1 @@\n",
            "+hello\n",
            "diff --git a/src/main.rs b/src/main.rs\n",
            "--- a/src/main.rs\n",
            "+++ b/src/main.rs\n",
            "{}",
        ),
        String::from_utf8(hunks).unwrap()
    );
    assert_eq!(stdout(&output), expected);
    // `git apply` takes it, and makes your files the agent's.
    let copy = scratch.root.join("copy");
    fs::create_dir_all(copy.join("src")).unwrap();
    fs::write(copy.join("src/main.rs"), old).unwrap();
    fs::write(copy.join("gone.txt"), "bye\n").unwrap();
    // Its own repository, so `git apply` takes paths from here, not from the
    // one this test runs in.
    let init = git().args(["init", "-q"]).current_dir(&copy).status();
    assert!(init.unwrap().success());
    let mut apply = git()
        .args(["apply", "-"])
        .current_dir(&copy)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    apply
        .stdin
        .take()
        .unwrap()
        .write_all(&output.stdout)
        .unwrap();
    assert!(apply.wait().unwrap().success());
    assert_eq!(fs::read_to_string(copy.join("src/main.rs")).unwrap(), new);
    assert_eq!(fs::read_to_string(copy.join("new.txt")).unwrap(), "hello\n");
    assert!(!copy.join("gone.txt").exists());
    // Your files are as they were.
    let yours = fs::read_to_string(scratch.share.join("src/main.rs")).unwrap();
    assert_eq!(yours, old);
}

// spec(SHARE): A binary file, a mode change, and a symlink each get one line instead.
#[test]
fn a_binary_file_a_mode_change_and_a_symlink_each_get_one_line() {
    let scratch = Scratch::new("lines");
    fs::write(scratch.share.join("logo.png"), b"\x89PNG\0one").unwrap();
    fs::write(scratch.share.join("run.sh"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(
        scratch.share.join("run.sh"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    symlink("old-target", scratch.share.join("old-link")).unwrap();
    let (run, pending) = scratch.run("notes", 1);
    write(&pending, "logo.png", 0o644, b"\x89PNG\0two");
    write(&pending, "run.sh", 0o755, b"#!/bin/sh\n");
    fs::create_dir_all(pending.join("tree")).unwrap();
    fs::write(pending.join("tree/latest"), "").unwrap();
    record(&pending, "SYMLINK \"latest\" \"build/out\"");
    record(&pending, "DELETE \"old-link\"");

    let output = scratch.ap(&["diff", &run]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        concat!(
            "symlink latest -> build/out\n",
            "Binary files a/logo.png and b/logo.png differ\n",
            "delete symlink old-link\n",
            "mode change 100644 => 100755 run.sh\n",
        )
    );
}

// spec(SHARE): So does a file that `ap apply` would skip, other than a git-ignored one, and the line says why.
// spec(SHARE): Git-ignored files are neither shown nor applied.
// spec(SHARE): `ap diff` and `ap apply` report how many files they skipped, and why.
#[test]
fn a_file_apply_would_skip_gets_a_line_that_says_why() {
    let scratch = Scratch::new("skipped");
    fs::write(scratch.share.join(".gitignore"), "*.log\nbuild/\n").unwrap();
    fs::write(scratch.share.join("mine.txt"), "mine\n").unwrap();
    fs::write(scratch.share.join("old.log"), "old\n").unwrap();
    fs::create_dir_all(scratch.share.join("vendor/lib/.git")).unwrap();
    let (run, pending) = scratch.run("notes", 1);
    // You change your file after the agent first wrote it.
    record(&pending, &base(&scratch.share, "mine.txt"));
    write(&pending, "mine.txt", 0o644, b"the agent's\n");
    fs::write(scratch.share.join("mine.txt"), "yours, again\n").unwrap();
    write(
        &pending,
        "vendor/lib/.git/HEAD",
        0o644,
        b"ref: refs/heads/main\n",
    );
    write(&pending, "vendor/lib/.GIT/config", 0o644, b"[core]\n");
    fs::write(pending.join("tree/pipe"), "").unwrap();
    record(&pending, "FIFO \"pipe\" 644");
    fs::write(pending.join("tree/escape"), "").unwrap();
    record(&pending, "SYMLINK \"escape\" \"../../etc/passwd\"");
    write(&pending, "out.log", 0o644, b"log\n");
    write(&pending, "build/a.o", 0o644, b"\0");
    record(&pending, "DELETE \"old.log\"");

    let output = scratch.ap(&["diff", &run]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        concat!(
            "skipped escape: it is a symlink whose target leaves the share\n",
            "skipped mine.txt: it changed on your side after the agent first changed it\n",
            "skipped pipe: it is a fifo, and special files are never applied\n",
            "skipped vendor/lib/.GIT/config: it is under .git\n",
            "skipped vendor/lib/.git/HEAD: it is under .git\n",
            "skipped 3 git-ignored files\n",
        )
    );
}

#[test]
fn a_repository_s_own_git_directory_isn_t_shown_as_files() {
    let scratch = Scratch::new("repository");
    fs::create_dir(scratch.share.join(".git")).unwrap();
    let (run, pending) = scratch.run("notes", 1);
    write(&pending, ".git/refs/heads/main", 0o644, b"0000\n");
    write(&pending, "README", 0o644, b"hi\n");

    let output = scratch.ap(&["diff", &run]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!(
            "branch ap/notes/{run} would carry 0 commits\ndiff --git a/README b/README\nnew file mode 100644\n--- /dev/null\n+++ b/README\n@@ -0,0 +1 @@\n+hi\n"
        )
    );
}

// spec(SHARE): For a share whose root holds a `.git` directory, the first line says how many commits the branch would carry.
// spec(SHARE): It counts the commits the agent's `HEAD` reaches that your repository doesn't have.
// spec(SHARE): When `ap` reads git state that an agent wrote, here or in a persisted repository, it reads refs, objects, the index, and ignore files with its own code.
#[test]
fn the_first_line_says_how_many_commits_the_branch_would_carry() {
    let scratch = Scratch::new("commits");
    let share = &scratch.share;
    repo::run(share, &["init", "-q"]);
    repo::commit(share, "README", "hello\n", "start");
    let (run, pending) = scratch.run("notes", 1);
    repo::capture(share, &pending);
    let guest = scratch.root.join("guest");
    repo::guest(share, &guest);
    // A `PATH` with nothing in it, so there is no `git` to run.
    let empty = scratch.root.join("empty");
    fs::create_dir(&empty).unwrap();
    let counts = |expected: &str| {
        let output = Command::new(env!("CARGO_BIN_EXE_ap"))
            .args(["diff", &run])
            .env_clear()
            .env("HOME", &scratch.home)
            .env("PATH", &empty)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        let shown = stdout(&output);
        let first = shown.lines().next().unwrap_or_default().to_owned();
        assert_eq!(
            first,
            format!("branch ap/notes/{run} would carry {expected}")
        );
        shown
    };
    counts("0 commits");
    repo::commit(&guest, "a", "a\n", "one");
    repo::land(&guest, share, &pending);
    counts("1 commit");
    repo::commit(&guest, "b", "b\n", "two");
    repo::run(&guest, &["repack", "-q", "-d"]);
    repo::commit(&guest, "c", "c\n", "three");
    repo::land(&guest, share, &pending);
    write(&pending, "c", 0o644, b"c\n");
    let shown = counts("3 commits");
    let expected = repo::run(
        &guest,
        &["rev-list", "--count", "HEAD", "--not", "main@{u}"],
    );
    assert_eq!(expected, "3");
    assert!(shown.ends_with("+++ b/c\n@@ -0,0 +1 @@\n+c\n"), "{shown}");
}

/// A file the agent wrote in place of a directory its `HEAD` holds with a
/// git-ignored file in it, which `ap apply` skips, gets a line instead of
/// a diff. The file it deleted beside the ignored one is shown deleted, as
/// it is from the last commit.
// spec(SHARE): So does a file that `ap apply` would skip, other than a git-ignored one, and the line says why.
#[test]
fn a_file_in_place_of_a_directory_with_an_ignored_file_gets_a_line() {
    let scratch = Scratch::new("file-for-ignored");
    let share = &scratch.share;
    repo::run(share, &["init", "-q"]);
    fs::write(share.join(".gitignore"), "*.log\n").unwrap();
    fs::create_dir(share.join("logs")).unwrap();
    for file in ["logs/debug.log", "logs/README"] {
        fs::write(share.join(file), "noise\n").unwrap();
        repo::run(share, &["add", "-f", file]);
    }
    repo::run(share, &["commit", "-q", "-m", "logs"]);
    let (run, pending) = scratch.run("notes", 1);
    repo::capture(share, &pending);
    for path in ["logs/debug.log", "logs/README", "logs"] {
        record(&pending, &base(share, path));
        record(&pending, &format!("DELETE \"{path}\""));
    }
    write(&pending, "logs", 0o644, b"one file\n");

    let output = scratch.ap(&["diff", &run]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!(
            "branch ap/notes/{run} would carry 0 commits\n\
             skipped logs: it would replace a directory that holds a git-ignored file\n\
             diff --git a/logs/README b/logs/README\n\
             deleted file mode 100644\n\
             --- a/logs/README\n\
             +++ /dev/null\n\
             @@ -1 +0,0 @@\n\
             -noise\n\
             skipped 1 git-ignored file\n"
        )
    );
}

/// A commit of yours that the agent's history names, whose tree `ap diff`
/// can't read, because the agent wrote other contents under its name, is
/// history that can't be read, said on the first line.
// spec(SHARE): If `ap` can't read the agent's history, the line says why instead.
#[test]
fn a_tree_that_can_t_be_read_is_said_on_the_first_line() {
    let scratch = Scratch::new("tree-unread");
    let share = &scratch.share;
    repo::run(share, &["init", "-q"]);
    let start = repo::commit(share, "README", "hello\n", "start");
    let (run, pending) = scratch.run("notes", 1);
    repo::capture(share, &pending);
    let other = scratch.root.join("other");
    repo::run(&scratch.root, &["init", "-q", "other"]);
    let mut hash = repo::git(&other);
    hash.args(["hash-object", "-w", "--stdin"]);
    let mut child = (hash.stdin(Stdio::piped()).stdout(Stdio::piped()))
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), b"other\n").unwrap();
    let blob = String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap();
    let loose = |id: &str| format!("objects/{}/{}", &id[..2], &id[2..]);
    let data = fs::read(other.join(".git").join(loose(blob.trim()))).unwrap();
    write(&pending, &format!(".git/{}", loose(&start)), 0o444, &data);
    write(&pending, "notes", 0o644, b"notes\n");

    let output = scratch.ap(&["diff", &run]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let shown = stdout(&output);
    let first = shown.lines().next().unwrap();
    assert_eq!(
        first,
        format!(
            "branch ap/notes/{run}: its commits can't be counted: the agent's object {start} doesn't hold what its name says"
        )
    );
}

/// A history `ap` can't read, such as one whose object format isn't
/// SHA-1, still shows the agent's files.
// spec(SHARE): If `ap` can't read the agent's history, the line says why instead.
#[test]
fn a_history_that_can_t_be_read_still_shows_the_files() {
    let scratch = Scratch::new("sha256");
    let share = &scratch.share;
    repo::run(share, &["init", "-q", "--object-format=sha256"]);
    repo::commit(share, "README", "hello\n", "start");
    let (run, pending) = scratch.run("notes", 1);
    repo::capture(share, &pending);
    let guest = scratch.root.join("guest");
    repo::guest(share, &guest);
    repo::commit(&guest, "a", "a\n", "one");
    repo::land(&guest, share, &pending);
    write(&pending, "a", 0o644, b"a\n");

    let output = scratch.ap(&["diff", &run]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let shown = stdout(&output);
    let first = shown.lines().next().unwrap();
    let expected = format!("branch ap/notes/{run}: its commits can't be counted: ");
    assert!(first.starts_with(&expected), "{shown}");
    assert!(shown.ends_with("+++ b/a\n@@ -0,0 +1 @@\n+a\n"), "{shown}");
}

/// From #714: the agent's `HEAD` holds `big`, whose 41 trees each name the
/// one below twice, so that it holds 2^40 files, or a file 1025 bytes long
/// beneath `deep1`. Beneath a directory the agent replaced with a file,
/// `ap diff` reads at most a million paths, and none that long, then says
/// why on the first line, within a fixed time.
// spec(SHARE): `ap diff` and `ap apply` read at most a million paths of the tree of the agent's `HEAD`, and none longer than 1024 bytes.
// spec(SHARE): They read it in each directory that holds a path the agent changed, and beneath each directory the agent deleted or replaced.
// spec(SHARE): Past either limit, `ap diff`'s first line says why, and `ap apply` refuses the branch import, says why, and everything stays pending.
#[test]
fn a_head_with_too_many_paths_beneath_what_the_agent_replaced_is_said_first() {
    let cases = [
        (
            "big",
            40,
            true,
            "more than the million paths that `ap` reads",
        ),
        (
            "deep1",
            509,
            false,
            "a path longer than the 1024 bytes that `ap` reads",
        ),
    ];
    for (name, depth, twice, why) in cases {
        let scratch = Scratch::new(&format!("too-many-{name}"));
        let share = &scratch.share;
        repo::run(share, &["init", "-q"]);
        repo::commit(share, "README", "hello\n", "start");
        let (run, pending) = scratch.run("notes", 1);
        repo::capture(share, &pending);
        let guest = scratch.root.join("guest");
        repo::guest(share, &guest);
        repo::commit_stacked(&guest, name, depth, twice);
        repo::land(&guest, share, &pending);
        record(&pending, &format!("DELETE \"{name}\""));
        write(&pending, name, 0o644, b"a file\n");
        let mut command = Command::new(env!("CARGO_BIN_EXE_ap"));
        command
            .args(["diff", &run])
            .current_dir(&scratch.root)
            .env_clear()
            .env("HOME", &scratch.home)
            .stdin(Stdio::null());
        repo::limit_cpu(&mut command, 20);

        let output = command.output().unwrap();

        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        let shown = stdout(&output);
        let first = shown.lines().next().unwrap();
        assert_eq!(
            first,
            format!(
                "branch ap/notes/{run}: its commits can't be counted: the agent's HEAD holds {why}"
            )
        );
    }
}

/// What the agent named, such as a pack, reaches your terminal only with
/// its control characters escaped.
#[test]
fn a_name_the_agent_chose_prints_no_control_characters() {
    let scratch = Scratch::new("controls");
    let share = &scratch.share;
    repo::run(share, &["init", "-q"]);
    repo::commit(share, "README", "hello\n", "start");
    let (run, pending) = scratch.run("notes", 1);
    repo::capture(share, &pending);
    // A ref to an object no loose file holds, so the packs are read, and
    // a pack whose name sets your clipboard when a terminal prints it.
    let id = "0123456789abcdef0123456789abcdef01234567";
    write(
        &pending,
        ".git/refs/heads/main",
        0o644,
        format!("{id}\n").as_bytes(),
    );
    let pack = pending.join("tree/.git/objects/pack");
    fs::create_dir_all(&pack).unwrap();
    for (ext, data) in [("idx", "not an index"), ("pack", "not a pack")] {
        fs::write(pack.join(format!("pack-\x1b]52;c;aGk=\x07.{ext}")), data).unwrap();
        let path = format!(".git/objects/pack/pack-\\x1b]52;c;aGk=\\x07.{ext}");
        record(&pending, &format!("FILE \"{path}\" 444"));
    }

    let output = scratch.ap(&["diff", &run]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let shown = stdout(&output);
    let first = shown.lines().next().unwrap();
    let expected = format!("branch ap/notes/{run}: its commits can't be counted: ");
    assert!(first.starts_with(&expected), "{shown:?}");
    assert!(!shown.contains(['\x1b', '\x07']), "{shown:?}");
    assert_eq!(shown.lines().count(), 1, "{shown:?}");
}

/// A name as a record holds it: in quotes, with each byte that isn't
/// printable ASCII, and `"` and `\\`, as `\\xNN`.
fn escaped(name: &str) -> String {
    let plain = |byte: u8| (0x20..0x7f).contains(&byte) && byte != b'"' && byte != b'\\';
    let inner: String = (name.bytes())
        .map(|byte| match plain(byte) {
            true => char::from(byte).to_string(),
            false => format!("\\x{byte:02x}"),
        })
        .collect();
    format!("\"{inner}\"")
}

// spec(Grants): `ap diff` and `ap logs` do the same for names that an agent chose.
#[test]
fn a_path_the_agent_chose_prints_with_controls_and_overrides_escaped() {
    let scratch = Scratch::new("escaped");
    let (run, pending) = scratch.run("notes", 1);
    // A name that clears your screen, one that a right-to-left override
    // shows as `exe.txt`, and an isolate around a binary file's name.
    for (name, data) in [
        ("clear\x1b[2J.txt", &b"a\n"[..]),
        ("\u{202e}txt.exe", b"b\n"),
        ("\u{2067}bin\u{2069}", b"\0"),
    ] {
        fs::write(pending.join("tree").join(name), data).unwrap();
        record(&pending, &format!("FILE {} 644", escaped(name)));
    }
    // A symlink whose name rings the bell, and whose target overrides.
    fs::write(pending.join("tree/link\x07"), "").unwrap();
    record(
        &pending,
        &format!(
            "SYMLINK {} {}",
            escaped("link\x07"),
            escaped("\u{202d}target")
        ),
    );

    let output = scratch.ap(&["diff", &run]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let shown = stdout(&output);
    assert_eq!(
        shown,
        concat!(
            r#"diff --git "a/clear\033[2J.txt" "b/clear\033[2J.txt""#,
            "\nnew file mode 100644\n--- /dev/null\n",
            r#"+++ "b/clear\033[2J.txt""#,
            "\n@@ -0,0 +1 @@\n+a\n",
            r#"symlink "link\a" -> "\342\200\255target""#,
            "\n",
            r#"diff --git "a/\342\200\256txt.exe" "b/\342\200\256txt.exe""#,
            "\nnew file mode 100644\n--- /dev/null\n",
            r#"+++ "b/\342\200\256txt.exe""#,
            "\n@@ -0,0 +1 @@\n+b\n",
            r#"Binary files /dev/null and "b/\342\201\247bin\342\201\251" differ"#,
            "\n",
        )
    );
    let bidi = ['\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}', '\u{202e}'];
    let isolates = ['\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}'];
    assert!(!shown.contains(['\x1b', '\x07']), "{shown:?}");
    assert!(
        !shown.contains(bidi) && !shown.contains(isolates),
        "{shown:?}"
    );
}

#[test]
fn a_deleted_directory_shows_each_of_your_files_in_it() {
    let scratch = Scratch::new("directory");
    fs::create_dir_all(scratch.share.join("docs/deep")).unwrap();
    fs::write(scratch.share.join("docs/a.md"), "a\n").unwrap();
    fs::write(scratch.share.join("docs/deep/b.md"), "b\n").unwrap();
    let (run, pending) = scratch.run("notes", 1);
    write(&pending, "docs/c.md", 0o600, b"c\n");
    record(&pending, "DELETE \"docs\"");
    // The agent makes the directory again, which hides yours, and writes
    // one file back.
    fs::create_dir_all(pending.join("tree/docs")).unwrap();
    record(&pending, "DIR \"docs\" 755 opaque");
    write(&pending, "docs/a.md", 0o644, b"a\n");

    let output = scratch.ap(&["diff", &run]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        concat!(
            "diff --git a/docs/deep/b.md b/docs/deep/b.md\n",
            "deleted file mode 100644\n",
            "--- a/docs/deep/b.md\n",
            "+++ /dev/null\n",
            "@@ -1 +0,0 @@\n",
            "-b\n",
        )
    );
}

// spec(SHARE): It exits 0 whether or not anything changed.
#[test]
fn diff_exits_0_when_nothing_changed() {
    let scratch = Scratch::new("nothing");
    scratch.agent("notes");
    let (run, _) = scratch.run("notes", 1);

    for name in [run.as_str(), "notes"] {
        let output = scratch.ap(&["diff", name]);

        assert_eq!(output.status.code(), Some(0), "{name}: {}", stderr(&output));
        assert_eq!(stdout(&output), "", "{name}");
        assert_eq!(stderr(&output), "", "{name}");
    }
    let output = scratch.ap(&["diff", "nobody"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "ap: no agent or run is named nobody\n");
}

// spec(SHARE): Each path is relative to its share's root.
// spec(SHARE): When a run has more than one copy-on-write share, each share's changes start with a line that names its root.
#[test]
fn each_share_s_changes_start_with_a_line_that_names_it() {
    let scratch = Scratch::new("shares");
    let other = scratch.root.join("other");
    fs::create_dir(&other).unwrap();
    let runs = scratch.runs();
    let run = runs.create("notes", 1).unwrap().name;
    runs.create_pending(&run).unwrap();
    for (root, file) in [(&scratch.share, "one"), (&other, "two")] {
        let pending = runs.create_pending_share(&run, root).unwrap();
        fs::create_dir(pending.join("tree")).unwrap();
        fs::write(pending.join("records"), "").unwrap();
        write(&pending, file, 0o644, b"x\n");
    }

    let output = scratch.ap(&["diff", &run]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let file = |name: &str| {
        format!(
            "diff --git a/{name} b/{name}\nnew file mode 100644\n--- /dev/null\n+++ b/{name}\n@@ -0,0 +1 @@\n+x\n"
        )
    };
    assert_eq!(
        stdout(&output),
        format!(
            "share {}\n{}share {}\n{}",
            scratch.share.display(),
            file("one"),
            other.display(),
            file("two")
        )
    );
}

// spec(SHARE): `ap diff`, `ap apply`, and `ap discard` take an agent or a run.
// spec(SHARE): A name means an agent in the store if there is one, and a run otherwise.
// spec(SHARE): With an agent name, `ap diff`, `ap apply`, and `ap discard` act on its single run with pending changes.
#[test]
fn a_name_means_an_agent_first_then_a_run() {
    let scratch = Scratch::new("names");
    // A run of another agent that has the agent's name, made before the
    // agent was built.
    let runs = scratch.runs();
    let named = runs
        .create_with("other", 2, || Ok("notes".to_owned()))
        .unwrap();
    scratch.agent("notes");
    let (_, pending) = scratch.run("notes", 1);
    write(&pending, "agent.txt", 0o644, b"notes\n");
    runs.create_pending(&named.name).unwrap();
    let pending = runs
        .create_pending_share(&named.name, &scratch.share)
        .unwrap();
    fs::create_dir(pending.join("tree")).unwrap();
    fs::write(pending.join("records"), "").unwrap();
    write(&pending, "run.txt", 0o644, b"run\n");
    // A run with nothing pending doesn't count.
    scratch.run("notes", 3);

    let output = scratch.ap(&["diff", "notes"]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("b/agent.txt"),
        "{}",
        stdout(&output)
    );
    assert!(!stdout(&output).contains("run.txt"), "{}", stdout(&output));

    // The agent's run goes, and the name still means the agent.
    let output = scratch.ap(&["discard", "notes"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let output = scratch.ap(&["diff", "notes"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");
    // A run's own name reaches it when no agent has that name.
    let (run, pending) = scratch.run("other", 4);
    write(&pending, "by-name.txt", 0o644, b"x\n");
    let output = scratch.ap(&["diff", &run]);
    assert!(
        stdout(&output).contains("b/by-name.txt"),
        "{}",
        stdout(&output)
    );
}

// spec(SHARE): If several runs have pending changes, each command lists them and exits 1, without a prompt.
// spec(SHARE): The list is newest first.
#[test]
fn several_runs_with_pending_changes_are_listed() {
    let scratch = Scratch::new("several");
    scratch.agent("notes");
    let (older, pending) = scratch.run("notes", 1);
    write(&pending, "a", 0o644, b"a\n");
    let (newer, pending) = scratch.run("notes", 2);
    write(&pending, "b", 0o644, b"b\n");

    for command in ["diff", "discard"] {
        let output = scratch.ap(&[command, "notes"]);

        assert_eq!(output.status.code(), Some(1), "{command}");
        assert_eq!(stdout(&output), "", "{command}");
        assert_eq!(
            stderr(&output),
            format!(
                "ap: agent notes has pending changes in 2 runs, so name one of them:\n{newer}\n{older}\n"
            ),
            "{command}"
        );
    }
    assert!(scratch.runs().has_pending(&older).unwrap());
    assert!(scratch.runs().has_pending(&newer).unwrap());
}

// spec(SHARE): `ap discard <agent | run>` throws the changes away.
// spec(SHARE): `ap apply` and `ap discard` fail while the run is active.
#[test]
fn discard_throws_the_changes_away_but_not_while_the_run_is_active() {
    let scratch = Scratch::new("discard");
    fs::write(scratch.share.join("kept.txt"), "yours\n").unwrap();
    let (run, pending) = scratch.run("notes", 1);
    write(&pending, "kept.txt", 0o644, b"the agent's\n");
    let held = scratch.runs().hold(&run).unwrap();

    let output = scratch.ap(&["discard", &run]);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        format!(
            "ap: run {run} is active, so its changes stay. Stop it with `ap stop {run}` first\n"
        )
    );
    assert!(scratch.runs().has_pending(&run).unwrap());

    drop(held);
    let output = scratch.ap(&["discard", &run]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");
    assert_eq!(stderr(&output), "");
    assert!(!scratch.store().join("pending").join(&run).exists());
    let output = scratch.ap(&["diff", &run]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");
    // Your file is as it was.
    let yours = fs::read_to_string(scratch.share.join("kept.txt")).unwrap();
    assert_eq!(yours, "yours\n");
}

#[test]
fn a_file_that_replaces_a_directory_shows_the_directory_s_files_deleted() {
    let scratch = Scratch::new("replaced");
    fs::create_dir(scratch.share.join("docs")).unwrap();
    fs::write(scratch.share.join("docs/a"), "a\n").unwrap();
    fs::write(scratch.share.join("notes"), "notes\n").unwrap();
    let (run, pending) = scratch.run("notes", 1);
    // The agent removes `docs/a` and `docs`, and makes a file `docs`.
    record(&pending, &base(&scratch.share, "docs/a"));
    record(&pending, "DELETE \"docs/a\"");
    record(&pending, &base(&scratch.share, "docs"));
    record(&pending, "DELETE \"docs\"");
    write(&pending, "docs", 0o644, b"now a file\n");
    // And removes the file `notes`, and makes a directory there.
    record(&pending, &base(&scratch.share, "notes"));
    record(&pending, "DELETE \"notes\"");
    fs::create_dir(pending.join("tree/notes")).unwrap();
    record(&pending, "DIR \"notes\" 755");
    write(&pending, "notes/b", 0o644, b"b\n");

    let output = scratch.ap(&["diff", &run]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let new = |name: &str, line: &str| {
        format!(
            "diff --git a/{name} b/{name}\nnew file mode 100644\n--- /dev/null\n+++ b/{name}\n@@ -0,0 +1 @@\n+{line}\n"
        )
    };
    let deleted = |name: &str, line: &str| {
        format!(
            "diff --git a/{name} b/{name}\ndeleted file mode 100644\n--- a/{name}\n+++ /dev/null\n@@ -1 +0,0 @@\n-{line}\n"
        )
    };
    assert_eq!(
        stdout(&output),
        [
            new("docs", "now a file"),
            deleted("docs/a", "a"),
            deleted("notes", "notes"),
            new("notes/b", "b"),
        ]
        .concat()
    );
}

#[test]
fn your_files_are_never_read_through_a_symlink() {
    let scratch = Scratch::new("through");
    let outside = scratch.root.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("file"), "outside\n").unwrap();
    symlink(&outside, scratch.share.join("docs")).unwrap();
    let (run, pending) = scratch.run("notes", 1);
    record(&pending, "DELETE \"docs\"");
    fs::create_dir(pending.join("tree/docs")).unwrap();
    record(&pending, "DIR \"docs\" 755");
    write(&pending, "docs/file", 0o644, b"the agent's\n");

    let output = scratch.ap(&["diff", &run]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        concat!(
            "delete symlink docs\n",
            "diff --git a/docs/file b/docs/file\n",
            "new file mode 100644\n",
            "--- /dev/null\n",
            "+++ b/docs/file\n",
            "@@ -0,0 +1 @@\n",
            "+the agent's\n",
        )
    );
}

/// The share server's order for a rename: the tree first, then the
/// records.
#[test]
fn diff_reads_an_active_run_while_its_agent_renames_files() {
    let scratch = Scratch::new("renaming");
    let (run, pending) = scratch.run("notes", 1);
    write(&pending, "a", 0o644, b"x\n");
    let _held = scratch.runs().hold(&run).unwrap();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let renamer = {
        let (pending, stop) = (pending.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut names = ("a", "b");
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let tree = pending.join("tree");
                fs::rename(tree.join(names.0), tree.join(names.1)).unwrap();
                record(&pending, &format!("FILE \"{}\" 644", names.1));
                record(&pending, &format!("DELETE \"{}\"", names.0));
                names = (names.1, names.0);
                std::thread::sleep(std::time::Duration::from_micros(200));
            }
        })
    };

    let outputs: Vec<Output> = (0..40).map(|_| scratch.ap(&["diff", &run])).collect();

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    renamer.join().unwrap();
    for output in outputs {
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        assert!(stdout(&output).ends_with("+x\n"), "{}", stdout(&output));
    }
}

#[test]
fn a_repository_s_files_you_changed_too_are_still_shown() {
    let scratch = Scratch::new("repository-conflict");
    fs::create_dir(scratch.share.join(".git")).unwrap();
    fs::write(scratch.share.join("file"), "one\n").unwrap();
    let (run, pending) = scratch.run("notes", 1);
    record(&pending, &base(&scratch.share, "file"));
    write(&pending, "file", 0o644, b"the agent's\n");
    fs::write(scratch.share.join("file"), "two\n").unwrap();

    let output = scratch.ap(&["diff", &run]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("-two\n+the agent's\n"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn a_file_you_add_to_a_directory_the_agent_deleted_is_skipped() {
    let scratch = Scratch::new("added-to-deleted");
    fs::create_dir(scratch.share.join("docs")).unwrap();
    let (run, pending) = scratch.run("notes", 1);
    record(&pending, &base(&scratch.share, "docs"));
    record(&pending, "DELETE \"docs\"");
    fs::write(scratch.share.join("docs/yours"), "yours\n").unwrap();

    let output = scratch.ap(&["diff", &run]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "skipped docs/yours: it changed on your side after the agent first changed it\n"
    );
}

// spec(SHARE): A mode change line shows any change to a file's permission bits.
#[test]
fn any_change_of_permission_bits_is_a_mode_change() {
    let scratch = Scratch::new("permissions");
    fs::write(scratch.share.join("secret"), "s\n").unwrap();
    fs::set_permissions(
        scratch.share.join("secret"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let (run, pending) = scratch.run("notes", 1);
    write(&pending, "secret", 0o600, b"s\n");

    let output = scratch.ap(&["diff", &run]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "mode change 100644 => 100600 secret\n");
}

#[test]
fn discard_fails_while_the_run_is_active_even_with_nothing_pending() {
    let scratch = Scratch::new("active-empty");
    let (run, _) = scratch.run("notes", 1);
    let held = scratch.runs().hold(&run).unwrap();

    let output = scratch.ap(&["discard", &run]);

    assert_eq!(output.status.code(), Some(1));
    assert!(scratch.store().join("pending").join(&run).exists());
    drop(held);
}

// spec(SHARE): With nothing pending, `ap discard` does nothing and exits 0.
#[test]
fn discard_with_nothing_pending_does_nothing() {
    let scratch = Scratch::new("discard-nothing");
    scratch.agent("notes");
    let (run, _) = scratch.run("notes", 1);

    for name in ["notes", run.as_str()] {
        let output = scratch.ap(&["discard", name]);

        assert_eq!(output.status.code(), Some(0), "{name}: {}", stderr(&output));
        assert_eq!(stdout(&output), "", "{name}");
        assert_eq!(stderr(&output), "", "{name}");
    }
}

#[test]
fn a_symlink_shows_once_and_only_when_it_changed() {
    let scratch = Scratch::new("symlinks");
    symlink("target", scratch.share.join("same")).unwrap();
    symlink("old", scratch.share.join("moved")).unwrap();
    let outside = scratch.root.join("outside");
    fs::create_dir(&outside).unwrap();
    symlink(&outside, scratch.share.join("escape")).unwrap();
    let (run, pending) = scratch.run("notes", 1);
    for (name, target) in [("same", "target"), ("moved", "new"), ("via", "escape/data")] {
        fs::write(pending.join("tree").join(name), "").unwrap();
        record(&pending, &format!("SYMLINK \"{name}\" \"{target}\""));
    }

    let output = scratch.ap(&["diff", &run]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        concat!(
            "symlink moved -> new\n",
            "skipped via: it is a symlink whose target goes through a symlink\n",
        )
    );
}

// spec(SHARE): Your `.gitignore` files in the share decide which files are git-ignored, and ones the agent wrote don't.
#[test]
fn a_gitignore_the_agent_wrote_hides_nothing() {
    let scratch = Scratch::new("gitignore");
    let (run, pending) = scratch.run("notes", 1);
    write(&pending, ".gitignore", 0o644, b"*.txt\n");
    write(&pending, "a.txt", 0o644, b"a\n");

    let output = scratch.ap(&["diff", &run]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("+++ b/.gitignore\n"),
        "{}",
        stdout(&output)
    );
    assert!(
        stdout(&output).contains("+++ b/a.txt\n"),
        "{}",
        stdout(&output)
    );
}

// spec(SHARE): A run has pending changes once its agent changes a file in a copy-on-write share, even if it changes the file back.
#[test]
fn a_change_the_agent_undid_is_still_pending() {
    let scratch = Scratch::new("undone");
    scratch.agent("notes");
    fs::write(scratch.share.join("file"), "same\n").unwrap();
    let (one, pending) = scratch.run("notes", 1);
    write(&pending, "file", 0o644, b"same\n");
    let (two, pending) = scratch.run("notes", 2);
    write(&pending, "other", 0o644, b"x\n");

    let output = scratch.ap(&["diff", "notes"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).ends_with(&format!("{two}\n{one}\n")),
        "{}",
        stderr(&output)
    );
    let output = scratch.ap(&["diff", &one]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");
}

#[test]
fn a_file_you_add_beneath_nested_directories_the_agent_deleted_is_skipped() {
    let scratch = Scratch::new("nested-deleted");
    fs::create_dir_all(scratch.share.join("docs/deep")).unwrap();
    fs::write(scratch.share.join("docs/deep/old"), "old\n").unwrap();
    let (run, pending) = scratch.run("notes", 1);
    for path in ["docs/deep/old", "docs/deep", "docs"] {
        record(&pending, &base(&scratch.share, path));
        record(&pending, &format!("DELETE \"{path}\""));
    }
    fs::write(scratch.share.join("docs/deep/new"), "new\n").unwrap();

    let output = scratch.ap(&["diff", &run]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        concat!(
            "skipped docs/deep/new: it changed on your side after the agent first changed it\n",
            "diff --git a/docs/deep/old b/docs/deep/old\n",
            "deleted file mode 100644\n",
            "--- a/docs/deep/old\n",
            "+++ /dev/null\n",
            "@@ -1 +0,0 @@\n",
            "-old\n",
        )
    );
}

/// The agent turns `a` from a file into a directory and back, in the
/// share server's order: the tree first, then the records.
#[test]
fn diff_reads_an_active_run_while_its_agent_replaces_a_file_with_a_directory() {
    let scratch = Scratch::new("replacing");
    let (run, pending) = scratch.run("notes", 1);
    write(&pending, "a", 0o644, b"x\n");
    write(&pending, "b", 0o644, b"y\n");
    let _held = scratch.runs().hold(&run).unwrap();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let replacer = {
        let (pending, stop) = (pending.clone(), stop.clone());
        std::thread::spawn(move || {
            let a = pending.join("tree/a");
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                fs::remove_file(&a).unwrap();
                fs::create_dir(&a).unwrap();
                record(&pending, "DELETE \"a\"");
                record(&pending, "DIR \"a\" 755");
                std::thread::sleep(std::time::Duration::from_micros(200));
                fs::remove_dir(&a).unwrap();
                fs::write(&a, "x\n").unwrap();
                record(&pending, "DELETE \"a\"");
                record(&pending, "FILE \"a\" 644");
                std::thread::sleep(std::time::Duration::from_micros(200));
            }
        })
    };

    let outputs: Vec<Output> = (0..40).map(|_| scratch.ap(&["diff", &run])).collect();

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    replacer.join().unwrap();
    for output in outputs {
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        assert!(stdout(&output).ends_with("+y\n"), "{}", stdout(&output));
    }
}

#[test]
fn a_share_whose_directory_became_a_symlink_fails() {
    let scratch = Scratch::new("root-symlink");
    let root = scratch.root.join("project");
    fs::create_dir(&root).unwrap();
    let (run, pending) = scratch.run_sharing("notes", 1, &root);
    write(&pending, "file", 0o644, b"the agent's\n");
    let elsewhere = scratch.root.join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    fs::write(elsewhere.join("file"), "not yours to show\n").unwrap();
    fs::remove_dir(&root).unwrap();
    symlink(&elsewhere, &root).unwrap();

    let output = scratch.ap(&["diff", &run]);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert_eq!(
        stderr(&output),
        format!(
            "ap: run {run}: {}: the share's directory is now reached through a symlink\n",
            root.display()
        )
    );
}

// spec(SHARE): A directory gets no line of its own, as in `git diff`.
// spec(SHARE): `ap diff` counts them across every share.
#[test]
fn directories_get_no_line_and_ignored_files_count_across_shares() {
    let scratch = Scratch::new("directories");
    let other = scratch.root.join("other");
    fs::create_dir(&other).unwrap();
    fs::create_dir(scratch.share.join("kept")).unwrap();
    let runs = scratch.runs();
    let run = runs.create("notes", 1).unwrap().name;
    runs.create_pending(&run).unwrap();
    for root in [&scratch.share, &other] {
        fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        let pending = runs.create_pending_share(&run, root).unwrap();
        fs::create_dir(pending.join("tree")).unwrap();
        fs::write(pending.join("records"), "").unwrap();
        fs::create_dir(pending.join("tree/new")).unwrap();
        record(&pending, "DIR \"new\" 700");
        write(&pending, "out.log", 0o644, b"log\n");
    }
    let pending = runs.pending_shares(&run).unwrap()[0].dir.clone();
    fs::create_dir(pending.join("tree/kept")).unwrap();
    record(&pending, "DIR \"kept\" 700");

    let output = scratch.ap(&["diff", &run]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "skipped 2 git-ignored files\n");
}

#[test]
fn a_directory_the_agent_made_again_gets_no_line() {
    let scratch = Scratch::new("opaque");
    fs::write(scratch.share.join(".gitignore"), "build\n").unwrap();
    fs::create_dir(scratch.share.join("build")).unwrap();
    fs::write(scratch.share.join("build/old"), "old\n").unwrap();
    fs::create_dir(scratch.share.join("docs")).unwrap();
    let (run, pending) = scratch.run("notes", 1);
    for dir in ["build", "docs"] {
        record(&pending, &base(&scratch.share, dir));
        record(&pending, &format!("DELETE \"{dir}\""));
        fs::create_dir(pending.join("tree").join(dir)).unwrap();
        record(&pending, &format!("DIR \"{dir}\" 755 opaque"));
    }
    fs::write(scratch.share.join("docs/new"), "new\n").unwrap();

    let output = scratch.ap(&["diff", &run]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        concat!(
            "skipped docs/new: it changed on your side after the agent first changed it\n",
            "skipped 1 git-ignored file\n",
        )
    );
}

/// The share server records the share's root as `""`, when the agent
/// changes the root itself, as `chmod` on it does.
#[test]
fn a_change_to_the_share_s_own_directory_gets_no_line() {
    let scratch = Scratch::new("root");
    let (run, pending) = scratch.run("notes", 1);
    record(&pending, &base(&scratch.share, ""));
    record(&pending, "DIR \"\" 700");
    write(&pending, "file", 0o644, b"x\n");

    let output = scratch.ap(&["diff", &run]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).starts_with("diff --git a/file b/file\n"),
        "{}",
        stdout(&output)
    );
}
