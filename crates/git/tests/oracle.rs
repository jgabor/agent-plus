//! What `crates/git` reads, checked against the `git` on the host. Each test
//! builds a repository with `git`, and every object and ref in it must read
//! the same from the crate and from `git cat-file`, `git rev-parse`, and
//! `git for-each-ref`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use git::{Error, Id, Kind, Repository, Target};

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("git").join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// `git` in `root`, reading no configuration but the repository's own.
fn git(root: &Path) -> Command {
    let mut git = Command::new("git");
    git.current_dir(root)
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "A")
        .env("GIT_AUTHOR_EMAIL", "a@example.com")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_NAME", "A")
        .env("GIT_COMMITTER_EMAIL", "a@example.com")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z");
    git
}

fn output(root: &Path, args: &[&str], input: &[u8]) -> Output {
    let mut child = git(root)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

/// Runs `git`, and returns its stdout. It must succeed.
fn run(root: &Path, args: &[&str]) -> Vec<u8> {
    let out = output(root, args, b"");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn text(root: &Path, args: &[&str]) -> String {
    String::from_utf8(run(root, args)).unwrap()
}

fn init(name: &str) -> PathBuf {
    let root = scratch(name);
    run(&root, &["init", "-q", "-b", "main"]);
    root
}

fn commit(root: &Path, file: &str, contents: &[u8]) {
    let path = root.join(file);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
    run(root, &["add", "-A"]);
    run(root, &["commit", "-q", "-m", file]);
}

fn id(hex: &str) -> Id {
    Id::from_hex(hex.trim()).unwrap_or_else(|| panic!("not an id: {hex:?}"))
}

/// Every object in the repository must read as `git cat-file` reads it.
fn assert_objects_match(root: &Path) -> usize {
    let repo = Repository::at(root.join(".git"));
    let list = text(
        root,
        &["cat-file", "--batch-all-objects", "--batch-check=%(objectname) %(objecttype)"],
    );
    let mut count = 0;
    for line in list.lines() {
        let (hex, kind) = line.split_once(' ').unwrap();
        let object = repo
            .loose_object(&id(hex))
            .unwrap()
            .unwrap_or_else(|| panic!("{hex} is missing"));
        assert_eq!(object.kind.to_string(), kind, "the type of {hex}");
        assert_eq!(
            object.data,
            run(root, &["cat-file", kind, hex]),
            "the contents of {hex}"
        );
        count += 1;
    }
    count
}

/// Every ref must read as `git for-each-ref` and `git rev-parse` read it.
fn assert_refs_match(root: &Path) {
    let repo = Repository::at(root.join(".git"));
    let expected = text(root, &["for-each-ref", "--format=%(refname) %(objectname)"]);
    let mut actual = String::new();
    for (name, _) in repo.refs().unwrap() {
        let id = repo.resolve(&name).unwrap().unwrap();
        actual.push_str(&format!("{name} {id}\n"));
    }
    assert_eq!(actual, expected);
    for name in expected.lines().map(|l| l.split_once(' ').unwrap().0) {
        let hex = text(root, &["rev-parse", "--verify", name]);
        assert_eq!(repo.resolve(name).unwrap(), Some(id(&hex)), "{name}");
    }
    let head = text(root, &["rev-parse", "--verify", "HEAD"]);
    assert_eq!(repo.resolve("HEAD").unwrap(), Some(id(&head)), "HEAD");
}

#[test]
fn every_kind_of_loose_object_reads_as_git_reads_it() {
    let root = init("kinds");
    commit(&root, "empty", b"");
    let binary: Vec<u8> = (0..=255u8).cycle().take(70_000).collect();
    commit(&root, "dir/binary", &binary);
    // Large enough that zlib writes it in many blocks.
    let large: Vec<u8> = (0..2_000_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    commit(&root, "dir/sub/large", &large);
    run(&root, &["tag", "-a", "-m", "annotated", "v1"]);
    run(&root, &["tag", "-a", "-m", "a tag of a tag", "v1-again", "v1"]);
    let tree_count = text(&root, &["count-objects", "-v"]);
    assert!(tree_count.contains("in-pack: 0"), "{tree_count}");
    let count = assert_objects_match(&root);
    // 3 blobs, 6 trees, 3 commits, and 2 tags.
    assert_eq!(count, 14);
}

#[test]
fn an_object_written_with_a_compression_level_reads_the_same() {
    for level in ["0", "1", "9"] {
        let root = init(&format!("level-{level}"));
        run(&root, &["config", "core.compression", level]);
        commit(&root, "file", &b"some text that compresses\n".repeat(500));
        assert_objects_match(&root);
    }
}

#[test]
fn an_object_that_is_not_loose_is_missing() {
    let root = init("missing");
    commit(&root, "file", b"one\n");
    let repo = Repository::at(root.join(".git"));
    let absent = id("0123456789abcdef0123456789abcdef01234567");
    assert!(repo.loose_object(&absent).unwrap().is_none());
    run(&root, &["gc", "-q", "--prune=now"]);
    let head = text(&root, &["rev-parse", "HEAD"]);
    assert!(repo.loose_object(&id(&head)).unwrap().is_none());
}

/// Writes a loose object from raw bytes, as `git` would name it.
fn plant(root: &Path, hex: &str, raw: &[u8]) {
    let path = root.join(".git/objects").join(&hex[..2]).join(&hex[2..]);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, miniz_oxide::deflate::compress_to_vec_zlib(raw, 6)).unwrap();
}

/// Both `git cat-file` and the crate must refuse the object.
fn assert_both_refuse(root: &Path, hex: &str) {
    let out = output(root, &["cat-file", "-p", hex], b"");
    assert!(!out.status.success(), "git read {hex}");
    let repo = Repository::at(root.join(".git"));
    match repo.loose_object(&id(hex)) {
        Err(Error::Corrupt(_)) => {}
        other => panic!("{hex}: {other:?}"),
    }
}

#[test]
fn a_loose_object_git_refuses_is_refused() {
    let root = init("corrupt");
    let cases: &[&[u8]] = &[
        b"blob\0four",
        b"blob x\0four",
        b"blob 4",
        b"kite 4\0four",
        b"blob 99999999999999999999999\0four",
    ];
    for (n, raw) in cases.iter().enumerate() {
        let hex = format!("{n:02}{}", "e".repeat(38));
        plant(&root, &hex, raw);
        assert_both_refuse(&root, &hex);
    }
    // Not zlib at all.
    let hex = "f".repeat(40);
    let path = root.join(".git/objects/ff").join(&hex[2..]);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, b"plain text").unwrap();
    assert_both_refuse(&root, &hex);
}

/// The name `git` gives a loose object whose uncompressed bytes are `raw`.
fn name_of(raw: &[u8]) -> String {
    let mut child = Command::new("sha1sum")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(raw).unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8(out.stdout).unwrap()[..40].to_string()
}

#[test]
fn a_loose_object_whose_size_disagrees_with_its_header_is_refused() {
    // `git cat-file` reads these leniently, so `git fsck` is the oracle.
    // Each is named by its hash, so fsck can object only to its contents.
    for raw in [&b"blob 3\0four"[..], b"blob 5\0four"] {
        let root = init("size");
        let hex = name_of(raw);
        plant(&root, &hex, raw);
        let out = output(&root, &["fsck", "--strict"], b"");
        assert!(!out.status.success(), "git fsck accepts {raw:?}");
        let repo = Repository::at(root.join(".git"));
        assert!(
            matches!(repo.loose_object(&id(&hex)), Err(Error::Corrupt(_))),
            "{raw:?}"
        );
    }
}

#[test]
fn a_planted_object_git_accepts_reads_as_git_reads_it() {
    let root = init("planted");
    let raw = b"blob 4\0four";
    let hex = name_of(raw);
    plant(&root, &hex, raw);
    run(&root, &["fsck", "--strict"]);
    let repo = Repository::at(root.join(".git"));
    let object = repo.loose_object(&id(&hex)).unwrap().unwrap();
    assert_eq!(object.kind, Kind::Blob);
    assert_eq!(object.data, run(&root, &["cat-file", "blob", &hex]));
}

#[test]
fn loose_and_packed_refs_read_as_git_reads_them() {
    let root = init("refs");
    commit(&root, "a", b"a\n");
    run(&root, &["branch", "packed-only"]);
    run(&root, &["branch", "nested/deeper/name"]);
    run(&root, &["tag", "light"]);
    run(&root, &["tag", "-a", "-m", "m", "heavy"]);
    run(&root, &["pack-refs", "--all"]);
    // A loose ref overrides the packed one with the same name.
    commit(&root, "b", b"b\n");
    run(&root, &["branch", "-f", "packed-only", "HEAD~1"]);
    run(&root, &["branch", "loose-only"]);
    run(&root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    run(
        &root,
        &["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/main"],
    );
    run(&root, &["symbolic-ref", "refs/heads/alias", "refs/heads/main"]);
    assert!(root.join(".git/packed-refs").exists());
    assert_refs_match(&root);

    let repo = Repository::at(root.join(".git"));
    assert_eq!(
        repo.find_ref("HEAD").unwrap(),
        Some(Target::Symbolic("refs/heads/main".into()))
    );
    assert_eq!(
        repo.find_ref("refs/heads/alias").unwrap(),
        Some(Target::Symbolic("refs/heads/main".into()))
    );
    assert_eq!(repo.find_ref("refs/heads/nothing").unwrap(), None);
    assert_eq!(repo.resolve("refs/heads/nothing").unwrap(), None);
}

#[test]
fn a_detached_head_reads_as_git_reads_it() {
    let root = init("detached");
    commit(&root, "a", b"a\n");
    commit(&root, "b", b"b\n");
    run(&root, &["checkout", "-q", "--detach", "HEAD~1"]);
    assert_refs_match(&root);
    let repo = Repository::at(root.join(".git"));
    assert!(matches!(repo.find_ref("HEAD").unwrap(), Some(Target::Object(_))));
}

#[test]
fn a_repository_with_every_ref_packed_reads_as_git_reads_it() {
    let root = init("all-packed");
    commit(&root, "a", b"a\n");
    run(&root, &["tag", "-a", "-m", "m", "t"]);
    run(&root, &["branch", "other"]);
    run(&root, &["pack-refs", "--all"]);
    assert!(!root.join(".git/refs/heads/main").exists());
    assert_refs_match(&root);
}

#[test]
fn a_repository_with_no_commits_has_no_refs() {
    let root = init("empty");
    let repo = Repository::at(root.join(".git"));
    assert_eq!(repo.refs().unwrap(), Vec::new());
    assert_eq!(repo.resolve("HEAD").unwrap(), None);
    let out = output(&root, &["rev-parse", "--verify", "-q", "HEAD"], b"");
    assert!(!out.status.success());
}

#[test]
fn a_name_git_would_refuse_is_refused() {
    let root = init("names");
    commit(&root, "a", b"a\n");
    let repo = Repository::at(root.join(".git"));
    for name in [
        "refs/heads/../../config",
        "refs/heads/./main",
        "refs/heads//main",
        "refs/heads/main/",
        "refs/heads/.hidden",
        "refs/heads/x.lock",
        "refs/heads/a..b",
        "refs/heads/a b",
        "refs/heads/a~b",
        "refs/heads/a@{b",
        "refs",
        "config",
        "/etc/passwd",
    ] {
        let out = output(&root, &["check-ref-format", name], b"");
        let git_refuses = !out.status.success() || !name.starts_with("refs/");
        assert!(git_refuses, "git accepts {name}");
        assert!(
            matches!(repo.find_ref(name), Err(Error::Corrupt(_))),
            "{name}"
        );
    }
    // A symbolic ref may not point outside `refs/` either.
    fs::write(root.join(".git/refs/heads/escape"), "ref: ../../config\n").unwrap();
    assert!(matches!(
        repo.resolve("refs/heads/escape"),
        Err(Error::Corrupt(_))
    ));
}

#[test]
fn a_cycle_of_symbolic_refs_is_refused() {
    let root = init("cycle");
    commit(&root, "a", b"a\n");
    fs::write(root.join(".git/refs/heads/one"), "ref: refs/heads/two\n").unwrap();
    fs::write(root.join(".git/refs/heads/two"), "ref: refs/heads/one\n").unwrap();
    let out = output(&root, &["rev-parse", "--verify", "-q", "refs/heads/one"], b"");
    assert!(!out.status.success());
    let repo = Repository::at(root.join(".git"));
    assert!(matches!(repo.resolve("refs/heads/one"), Err(Error::Corrupt(_))));
}
