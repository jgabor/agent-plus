//! Repositories for the tests of a branch import. The host's `git` builds
//! your repository and the agent's, and is the oracle for what lands. A
//! run's pending directory gets the agent's `.git` the way the share
//! server writes it: a copy of yours, but its objects, made when the run
//! starts, and each file the agent's git wrote since, in the tree, with a
//! record.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

/// `git -C dir`, with no config but its defaults, and one identity.
pub fn git(dir: &Path) -> Command {
    let mut git = Command::new("git");
    git.arg("-C")
        .arg(dir)
        .args(["-c", "init.defaultBranch=main"])
        // No git in the background, which could change a `.git` while a
        // test reads it.
        .args(["-c", "maintenance.auto=false", "-c", "gc.auto=0"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "A U Thor")
        .env("GIT_AUTHOR_EMAIL", "author@example.com")
        .env("GIT_COMMITTER_NAME", "C O Mitter")
        .env("GIT_COMMITTER_EMAIL", "committer@example.com")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES");
    git
}

/// Runs `git -C dir` with `args`, which must succeed, and returns what it
/// printed, trimmed.
pub fn run(dir: &Path, args: &[&str]) -> String {
    let output = git(dir).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// Runs `git -C dir` with `args` and `input` on its standard input, which
/// must succeed, and returns what it printed, trimmed.
pub fn run_with(dir: &Path, args: &[&str], input: &[u8]) -> String {
    let mut git = git(dir);
    git.args(args).stdin(Stdio::piped()).stdout(Stdio::piped());
    let mut child = git.stderr(Stdio::piped()).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_vec();
    // Git may print before it reads everything, so its output is read
    // while this writes.
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let output = child.wait_with_output().unwrap();
    writer.join().unwrap().unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// Commits `path` in the repository `dir`, without writing it, as a tree
/// for each of `twice`, above one that holds `README` from `HEAD` as `f`.
/// Each holds the tree below it twice, as `a` and `b`, where `twice` says
/// so, from the bottom up, and once, as `a`, where it doesn't. Host `git`
/// writes the trees, in one `git mktree`.
pub fn commit_stacked(dir: &Path, path: &str, twice: impl IntoIterator<Item = bool>) {
    let readme = run(dir, &["rev-parse", "HEAD:README"]);
    let readme = git::Id::from_hex(&readme).unwrap();
    let mut entries = vec![entry(0o100644, b"f", readme)];
    let mut text = format!("100644 blob {readme}\tf\n\n");
    let mut tree = git::Object::tree(entries).id();
    for twice in twice {
        entries = vec![entry(0o40000, b"a", tree)];
        text.push_str(&format!("040000 tree {tree}\ta\n"));
        if twice {
            entries.push(entry(0o40000, b"b", tree));
            text.push_str(&format!("040000 tree {tree}\tb\n"));
        }
        text.push('\n');
        tree = git::Object::tree(entries).id();
    }
    let written = run_with(dir, &["mktree", "--batch"], text.as_bytes());
    assert_eq!(written.lines().last(), Some(&*tree.to_string()));
    let listing = run(dir, &["ls-tree", "HEAD"]);
    let root = format!("{listing}\n040000 tree {tree}\t{path}\n");
    let root = run_with(dir, &["mktree"], root.as_bytes());
    commit_tree(dir, &root, path);
}

/// Commits a tree in the repository `dir` that holds `README` from `HEAD`,
/// and the tree of `HEAD` named `name`, which may be one that `git mktree`
/// refuses, such as an empty one. Host `git` writes it.
pub fn commit_named(dir: &Path, name: &[u8]) {
    let id = |rev| git::Id::from_hex(&run(dir, &["rev-parse", rev])).unwrap();
    let tree = git::Object::tree(vec![
        entry(0o100644, b"README", id("HEAD:README")),
        entry(0o40000, name, id("HEAD^{tree}")),
    ]);
    let args = ["hash-object", "-t", "tree", "-w", "--literally", "--stdin"];
    let written = run_with(dir, &args, &tree.data);
    assert_eq!(written, tree.id().to_string());
    commit_tree(dir, &written, "named");
}

fn entry(mode: u32, name: &[u8], id: git::Id) -> git::TreeEntry {
    git::TreeEntry {
        mode,
        name: name.to_vec(),
        id,
    }
}

/// Commits `tree` in the repository `dir` after its `HEAD`, which then
/// names the commit.
fn commit_tree(dir: &Path, tree: &str, message: &str) {
    let commit = run(dir, &["commit-tree", tree, "-p", "HEAD", "-m", message]);
    run(dir, &["update-ref", "HEAD", &commit]);
}

/// Limits the CPU time of the process `command` starts to `seconds`, past
/// which it gets `SIGXCPU` and ends.
pub fn limit_cpu(command: &mut Command, seconds: u64) {
    // SAFETY: `setrlimit` is async-signal-safe, and touches nothing but the
    // child.
    unsafe {
        command.pre_exec(move || {
            let limit = libc::rlimit {
                rlim_cur: seconds,
                rlim_max: seconds,
            };
            match libc::setrlimit(libc::RLIMIT_CPU, &limit) {
                0 => Ok(()),
                _ => Err(std::io::Error::last_os_error()),
            }
        });
    }
}

/// Writes `data` to `path` in the repository `dir`, and commits it.
pub fn commit(dir: &Path, path: &str, data: &str, message: &str) -> String {
    let file = dir.join(path);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, data).unwrap();
    run(dir, &["add", path]);
    run(dir, &["commit", "-q", "-m", message]);
    run(dir, &["rev-parse", "HEAD"])
}

/// Copies your `.git` at `share` into `pending` as the share server does
/// when the run starts: everything but what `objects` holds, to `git`, and
/// the commit `HEAD` names, to `head`. It reads only a SHA-1 name, so a
/// `HEAD` with none, or with a longer one, counts as none.
pub fn capture(share: &Path, pending: &Path) {
    copy_tree(&share.join(".git"), &pending.join("git"), true);
    let head = git(share)
        .args(["rev-parse", "--verify", "-q", "HEAD"])
        .output()
        .unwrap();
    let head = match String::from_utf8(head.stdout).unwrap() {
        id if id.len() == 41 => id,
        _ => "none\n".to_owned(),
    };
    fs::write(pending.join("head"), head).unwrap();
}

fn copy_tree(from: &Path, to: &Path, top: bool) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let (from, to) = (entry.path(), to.join(entry.file_name()));
        let kind = entry.file_type().unwrap();
        if kind.is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(&from).unwrap(), &to).unwrap();
        } else if kind.is_dir() {
            match top && entry.file_name() == "objects" {
                true => fs::create_dir(&to).unwrap(),
                false => copy_tree(&from, &to, false),
            }
        } else {
            fs::copy(&from, &to).unwrap();
        }
    }
}

/// The agent's own repository at `at`, cloned from yours at `share`, as
/// the agent's git sees it when the run starts.
pub fn guest(share: &Path, at: &Path) {
    let output = git(share)
        .args(["clone", "-q", "--no-local", "."])
        .arg(at)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Puts each file of the agent's `.git` at `guest` in the tree of
/// `pending`, with a record, as the share server does when the agent's git
/// writes it: each object your repository at `share` doesn't have, and
/// each other file that isn't as the run's copy of your `.git` holds it.
pub fn land(guest: &Path, share: &Path, pending: &Path) {
    land_tree(&guest.join(".git"), share, pending, "");
}

fn land_tree(dir: &Path, share: &Path, pending: &Path, path: &str) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        let inner = match path {
            "" => name,
            _ => format!("{path}/{name}"),
        };
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            land_tree(&entry.path(), share, pending, &inner);
            continue;
        }
        // The agent's git writes no symlink, so one is as you had it.
        if kind.is_symlink() {
            continue;
        }
        let data = fs::read(entry.path()).unwrap();
        let unchanged = match inner.starts_with("objects/") {
            true => share.join(".git").join(&inner).exists(),
            false => fs::read(pending.join("git").join(&inner)).ok() == Some(data.clone()),
        };
        if !unchanged {
            let mode = entry.metadata().unwrap().permissions().mode() & 0o7777;
            write(pending, &format!(".git/{inner}"), mode, &data);
        }
    }
}

/// Puts the agent's copy of `path` in the tree, and records it.
pub fn write(pending: &Path, path: &str, mode: u32, data: &[u8]) {
    let file = pending.join("tree").join(path);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, data).unwrap();
    let mut records = fs::OpenOptions::new()
        .append(true)
        .open(pending.join("records"))
        .unwrap();
    (records.write_all(format!("FILE \"{path}\" {mode:o}\n").as_bytes())).unwrap();
}

/// Each commit `rev` reaches, newest first, and each one's contents, as
/// `git cat-file` prints them.
pub fn history(dir: &Path, rev: &str) -> Vec<String> {
    let commits = run(dir, &["rev-list", rev]);
    (commits.lines())
        .map(|id| format!("{id}\n{}", run(dir, &["cat-file", "-p", id])))
        .collect()
}
