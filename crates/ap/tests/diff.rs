//! `ap diff` and `ap discard`, from SPEC.md's `SHARE` section. Nothing here
//! boots: a test writes a run's pending directory the way the share server
//! does, as its `cow` module describes, and makes a run active the way a run
//! process does, by holding the lock on its record.
//!
//! The hunks are checked against the `git` on the host: `ap diff` must print
//! what `git diff --no-index` prints for the same two files, and any
//! difference fails the test.

mod common;

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
}

/// Puts the agent's copy of `path` in the tree, and records it.
fn write(pending: &Path, path: &str, mode: u32, data: &[u8]) {
    let file = pending.join("tree").join(path);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, data).unwrap();
    record(pending, &format!("FILE \"{path}\" {mode:o}"));
}

/// Adds a line to a share's records.
fn record(pending: &Path, line: &str) {
    let records = pending.join("records");
    let mut text = fs::read_to_string(&records).unwrap();
    text.push_str(line);
    text.push('\n');
    fs::write(records, text).unwrap();
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
const LINES: [&str; 9] = [
    "a\n",
    "b\n",
    "}\n",
    "\n",
    "fn x() {\n",
    "    c\n",
    "    d\n",
    "\tif y {\n",
    "   \n",
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
        "diff --git a/README b/README\nnew file mode 100644\n--- /dev/null\n+++ b/README\n@@ -0,0 +1 @@\n+hi\n"
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
    scratch.agent("notes");
    let (_, pending) = scratch.run("notes", 1);
    write(&pending, "agent.txt", 0o644, b"notes\n");
    // A run of another agent that has the agent's name, as a run made
    // before the agent was built could.
    let runs = scratch.runs();
    let named = runs
        .create_with("other", 2, || Ok("notes".to_owned()))
        .unwrap();
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
