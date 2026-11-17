//! A copy-on-write share of a repository's root, against the device's own
//! relay in this process: the share's `.git` is captured when the run
//! starts, but for its objects. These tests start `git`, so they live apart
//! from the lock tests in `cow.rs`, which a child holding a copy of their
//! descriptors for a moment would break.

mod guest;

use guest::*;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// A tree to share, and a place beside it for the pending directory.
fn pending(tree: &Tree) -> PathBuf {
    tree.base.join("pending")
}

/// `git` in `root`, reading no configuration but the repository's own.
fn git(root: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "A")
        .env("GIT_AUTHOR_EMAIL", "a@example.com")
        .env("GIT_COMMITTER_NAME", "A")
        .env("GIT_COMMITTER_EMAIL", "a@example.com")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// Makes `root` a repository on `main`, with one commit of its files.
fn repository(root: &Path) -> String {
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["add", "hello.txt", "dir"]);
    git(root, &["commit", "-q", "-m", "first"]);
    git(root, &["rev-parse", "HEAD"])
}

/// The path of the loose object `id` in a git directory.
fn object(id: &str) -> String {
    format!(".git/objects/{}/{}", &id[..2], &id[2..])
}

// spec(SHARE): The exception is the `.git` directory at the root of a copy-on-write share: everything in it except objects is captured when the run starts, so commits you make during the run don't move `HEAD` under the agent.
// spec(SHARE): Objects are read live, which keeps the capture small in a large repository, so pruning objects on your side during a run can remove ones the agent needs.
// spec(SHARE): Tracked files stay live, so a commit, pull, or branch switch on your side during a run looks like uncommitted changes to the agent.
#[test]
fn a_run_captures_git_but_its_objects_and_records_head() {
    let tree = Tree::new("cow-git");
    let first = repository(&tree.root);
    let pending = pending(&tree);
    let mut guest = Guest::cow(&tree.root, &pending);
    let seen = |guest: &mut Guest| {
        [".git/HEAD", ".git/refs/heads/main", ".git/index"].map(|path| guest.read_file(path))
    };
    let before = seen(&mut guest);
    for (path, seen) in [".git/HEAD", ".git/refs/heads/main", ".git/index"]
        .iter()
        .zip(&before)
    {
        assert_eq!(seen, &Ok(fs::read(tree.root.join(path)).unwrap()));
    }
    // The copy keeps your modes.
    let mode = fs::metadata(tree.root.join(".git/config")).unwrap().mode();
    assert_eq!(guest.mode(".git/config"), Ok(mode));
    assert_eq!(before[1], Ok(format!("{first}\n").into_bytes()));

    // You commit a change on your side during the run.
    fs::write(tree.root.join("hello.txt"), b"yours, committed").unwrap();
    git(&tree.root, &["commit", "-q", "-am", "second"]);
    let second = git(&tree.root, &["rev-parse", "HEAD"]);
    git(&tree.root, &["branch", "-q", "yours"]);

    // The agent's `HEAD`, refs, and index stay as they were.
    assert_eq!(seen(&mut guest), before);
    let refs = guest.walk(".git/refs/heads").unwrap();
    assert!(!guest.list(refs).contains("yours"));
    // The new commit's object, and the tracked file, show live.
    let made = fs::read(tree.root.join(object(&second))).unwrap();
    assert_eq!(guest.read_file(&object(&second)), Ok(made));
    assert_eq!(guest.read_file("hello.txt").unwrap(), b"yours, committed");
    let objects = guest.walk(".git").unwrap();
    assert!(guest.list(objects).contains("objects"));
    // An object you prune goes for the agent too.
    fs::remove_file(tree.root.join(object(&second))).unwrap();
    assert_eq!(guest.read_file(&object(&second)), Err(ENOENT));

    // What the agent writes in `.git` lands in pending, not in yours.
    guest
        .overwrite(".git/refs/heads/main", b"the agent's\n")
        .unwrap();
    assert_eq!(
        guest.read_file(".git/refs/heads/main").unwrap(),
        b"the agent's\n"
    );
    drop(guest);
    assert_eq!(
        fs::read_to_string(tree.root.join(".git/refs/heads/main")).unwrap(),
        format!("{second}\n")
    );
    // The pending directory records the commit `HEAD` named at the start.
    assert_eq!(
        fs::read_to_string(pending.join("head")).unwrap(),
        format!("{first}\n")
    );
}

#[test]
fn a_repository_with_no_commits_records_that_it_had_none() {
    let tree = Tree::new("cow-git-unborn");
    git(&tree.root, &["init", "-q", "-b", "main"]);
    let pending = pending(&tree);
    drop(Guest::cow(&tree.root, &pending));
    assert_eq!(fs::read_to_string(pending.join("head")).unwrap(), "none\n");
}

// spec(SHARE): A nested repository's `.git` stays live.
#[test]
fn a_nested_repository_s_git_stays_live() {
    let tree = Tree::new("cow-git-nested");
    repository(&tree.root);
    let nested = tree.root.join("dir");
    git(&nested, &["init", "-q", "-b", "main"]);
    let mut guest = Guest::cow(&tree.root, &pending(&tree));
    assert_eq!(
        guest.read_file("dir/.git/HEAD").unwrap(),
        b"ref: refs/heads/main\n"
    );
    git(&nested, &["checkout", "-q", "-b", "moved"]);
    git(&tree.root, &["checkout", "-q", "-b", "moved"]);
    assert_eq!(
        guest.read_file("dir/.git/HEAD").unwrap(),
        b"ref: refs/heads/moved\n"
    );
    // The share's own `.git` was captured.
    assert_eq!(
        guest.read_file(".git/HEAD").unwrap(),
        b"ref: refs/heads/main\n"
    );
}

#[test]
fn a_git_file_is_not_captured() {
    let tree = Tree::new("cow-git-file");
    fs::write(tree.root.join(".git"), b"gitdir: /elsewhere\n").unwrap();
    let pending = pending(&tree);
    let mut guest = Guest::cow(&tree.root, &pending);
    fs::write(tree.root.join(".git"), b"gitdir: /moved\n").unwrap();
    assert_eq!(guest.read_file(".git").unwrap(), b"gitdir: /moved\n");
    drop(guest);
    assert!(!pending.join("head").exists());
    assert!(!pending.join("git").exists());
}

#[test]
fn a_symlink_on_the_way_to_a_ref_is_not_followed() {
    for (label, linked) in [("packed", ".git/packed-refs"), ("heads", ".git/refs/heads")] {
        let tree = Tree::new(&format!("cow-git-linked-{label}"));
        let first = repository(&tree.root);
        let outside = tree.base.join("outside/linked");
        fs::remove_file(tree.root.join(".git/refs/heads/main")).unwrap();
        match label {
            "packed" => fs::write(&outside, format!("{first} refs/heads/main\n")).unwrap(),
            _ => {
                fs::create_dir(&outside).unwrap();
                fs::write(outside.join("main"), format!("{first}\n")).unwrap();
                fs::remove_dir(tree.root.join(linked)).unwrap();
            }
        }
        std::os::unix::fs::symlink(&outside, tree.root.join(linked)).unwrap();
        let pending = pending(&tree);
        let mut guest = Guest::cow(&tree.root, &pending);
        // The copy keeps the symlink, which the guest resolves itself.
        assert_eq!(guest.mode(linked).map(kind), Ok(S_IFLNK));
        drop(guest);
        assert_eq!(fs::read_to_string(pending.join("head")).unwrap(), "none\n");
    }
}

#[test]
fn the_capture_keeps_each_entry_as_itself_with_your_mode() {
    let tree = Tree::new("cow-git-modes");
    repository(&tree.root);
    let info = tree.root.join(".git/info");
    fs::set_permissions(&info, fs::Permissions::from_mode(0o555)).unwrap();
    let fifo =
        std::ffi::CString::new(tree.root.join(".git/pipe").into_os_string().into_vec()).unwrap();
    // SAFETY: `fifo` is NUL-terminated and outlives the call.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o640) }, 0);
    let pending = pending(&tree);
    let mut guest = Guest::cow(&tree.root, &pending);
    assert_eq!(guest.mode(".git/info"), Ok(libc::S_IFDIR | 0o555));
    assert_eq!(guest.mode(".git/pipe"), Ok(S_IFIFO | 0o640));
    drop(guest);
    fs::set_permissions(&info, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(pending.join("git/info"), fs::Permissions::from_mode(0o755)).unwrap();
}
