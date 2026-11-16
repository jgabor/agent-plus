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
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("git")
        .join(name);
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
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname) %(objecttype)",
        ],
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
    // Git lists every ref, and the crate those under `refs/`.
    let listed = text(root, &["for-each-ref", "--format=%(refname) %(objectname)"]);
    let expected: String = listed
        .split_inclusive('\n')
        .filter(|line| line.starts_with("refs/"))
        .collect();
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
    run(
        &root,
        &["tag", "-a", "-m", "a tag of a tag", "v1-again", "v1"],
    );
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
        b"blob 04\0four",
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
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );
    run(
        &root,
        &["symbolic-ref", "refs/heads/alias", "refs/heads/main"],
    );
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
    assert!(matches!(
        repo.find_ref("HEAD").unwrap(),
        Some(Target::Object(_))
    ));
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
    let out = output(
        &root,
        &["rev-parse", "--verify", "-q", "refs/heads/one"],
        b"",
    );
    assert!(!out.status.success());
    let repo = Repository::at(root.join(".git"));
    assert!(matches!(
        repo.resolve("refs/heads/one"),
        Err(Error::Corrupt(_))
    ));
}

#[test]
fn a_loose_ref_reads_only_as_git_reads_it() {
    let root = init("loose-ref");
    commit(&root, "a", b"a\n");
    let head = text(&root, &["rev-parse", "HEAD"]);
    let head = head.trim();
    for (n, contents) in [
        head.as_bytes().to_vec(),
        format!("{head} trailing words\n").into_bytes(),
        format!("{head}x\n").into_bytes(),
        format!("{}\n", &head[..39]).into_bytes(),
        format!("{head}\x0c\n").into_bytes(),
        format!("{head}\r\n").into_bytes(),
        [head.as_bytes(), b" \xff\n"].concat(),
        b"ref:refs/heads/main".to_vec(),
        b"ref: refs/heads/main \n".to_vec(),
        b"ref: refs/heads/main\x0c\n".to_vec(),
        b"ref:\trefs/heads/main\r\n".to_vec(),
        b"not a ref\n".to_vec(),
        [head.as_bytes(), b"\0garbage\n"].concat(),
        b"ref: refs/heads/main\0garbage\n".to_vec(),
        b"ref: refs/heads/main\n\0".to_vec(),
        b"ref: refs/heads/main \0x".to_vec(),
        format!("{}\n", "0".repeat(40)).into_bytes(),
    ]
    .iter()
    .enumerate()
    {
        let name = format!("refs/heads/case{n}");
        fs::write(root.join(".git").join(&name), contents).unwrap();
        assert_resolves_as_git(&root, &name);
    }
}

/// `name` must resolve as `git rev-parse --verify` resolves it, and a name
/// git refuses must be refused.
fn assert_resolves_as_git(root: &Path, name: &str) {
    let out = output(root, &["rev-parse", "--verify", "-q", name], b"");
    let ours = Repository::at(root.join(".git")).resolve(name);
    if out.status.success() {
        let theirs = id(&String::from_utf8(out.stdout).unwrap());
        assert_eq!(ours.unwrap(), Some(theirs), "{name}");
    } else {
        assert!(matches!(ours, Err(Error::Corrupt(_))), "{name}: {ours:?}");
    }
}

#[test]
fn a_ref_below_a_ref_file_is_missing() {
    let root = init("below");
    commit(&root, "a", b"a\n");
    let out = output(
        &root,
        &["rev-parse", "--verify", "-q", "refs/heads/main/x"],
        b"",
    );
    assert!(!out.status.success());
    let repo = Repository::at(root.join(".git"));
    assert_eq!(repo.find_ref("refs/heads/main/x").unwrap(), None);
    fs::write(
        root.join(".git/refs/heads/alias"),
        "ref: refs/heads/main/x\n",
    )
    .unwrap();
    assert_eq!(repo.resolve("refs/heads/alias").unwrap(), None);
}

#[test]
fn a_symlinked_ref_reads_as_git_reads_it() {
    let root = init("symlink");
    commit(&root, "a", b"a\n");
    run(
        &root,
        &[
            "-c",
            "core.preferSymlinkRefs=true",
            "symbolic-ref",
            "refs/heads/link",
            "refs/heads/main",
        ],
    );
    assert!(root.join(".git/refs/heads/link").is_symlink());
    let repo = Repository::at(root.join(".git"));
    assert_eq!(
        repo.find_ref("refs/heads/link").unwrap(),
        Some(Target::Symbolic("refs/heads/main".into()))
    );
    assert_resolves_as_git(&root, "refs/heads/link");
    // `git for-each-ref` lists no symlink.
    assert_refs_match(&root);
}

#[test]
fn a_symbolic_ref_keeps_whitespace_that_is_not_ascii() {
    let root = init("nbsp");
    commit(&root, "a", b"a\n");
    run(&root, &["branch", "utf"]);
    commit(&root, "b", b"b\n");
    run(&root, &["branch", "utf\u{a0}"]);
    fs::write(
        root.join(".git/refs/heads/alias"),
        "ref: refs/heads/utf\u{a0}\n",
    )
    .unwrap();
    assert_resolves_as_git(&root, "refs/heads/alias");
}

#[test]
fn a_chain_of_symbolic_refs_is_followed_as_far_as_git_follows_it() {
    for length in 1..=6 {
        let root = init(&format!("chain-{length}"));
        commit(&root, "a", b"a\n");
        let mut next = "refs/heads/main".to_string();
        for n in 0..length {
            let name = format!("refs/heads/link{n}");
            fs::write(root.join(".git").join(&name), format!("ref: {next}\n")).unwrap();
            next = name;
        }
        assert_resolves_as_git(&root, &next);
    }
}

#[test]
fn a_packed_refs_file_git_refuses_is_refused() {
    let root = init("bad-packed");
    commit(&root, "a", b"a\n");
    run(&root, &["pack-refs", "--all"]);
    let good = fs::read_to_string(root.join(".git/packed-refs")).unwrap();
    let hex = text(&root, &["rev-parse", "HEAD"]);
    let hex = hex.trim();
    for contents in [
        good.trim_end().to_string(),
        format!("^{hex}\n{good}"),
        format!("{good}^{hex}\n^{hex}\n"),
        format!("{good}{hex}refs/heads/x\n"),
        format!("# bogus header\n{hex} refs/heads/main\n"),
        format!("# pack-refs with:sorted\n{hex} refs/heads/main\n"),
        format!("{good}{hex} refs/heads/z@{{b\n"),
        format!("{good}{hex} refs/heads/z b\n"),
        format!("{good}{hex} refs\t/z\n"),
        format!("{good}{hex} refs/../z\n"),
        format!("{good}{hex} refs/heads/../z\n"),
        format!("{good}{hex} ZEBRA\n"),
        // One whitespace byte after the id, which may be a newline.
        format!("{good}{hex}\trefs/heads/z\n"),
        format!("{good}{hex}\nrefs/heads/z\n"),
        // Without `sorted`, git takes every line to be a whole record.
        format!("{hex}\nrefs/heads/z\n"),
        format!("{hex} refs/heads/main\n"),
        format!("{hex} refs/heads/main\n"),
    ] {
        fs::write(root.join(".git/packed-refs"), &contents).unwrap();
        // With a `sorted` header, `git rev-parse` reads only the lines it
        // needs. The crate reads the whole file, as `git for-each-ref` does.
        let out = output(&root, &["for-each-ref"], b"");
        let repo = Repository::at(root.join(".git"));
        if out.status.success() {
            assert_refs_match(&root);
        } else {
            assert!(matches!(repo.refs(), Err(Error::Corrupt(_))), "{contents}");
            let main = repo.resolve("refs/heads/main");
            assert!(matches!(main, Err(Error::Corrupt(_))), "{contents}");
        }
    }
}

#[test]
fn a_broken_loose_ref_is_left_out_as_git_leaves_it_out() {
    let root = init("broken");
    commit(&root, "a", b"a\n");
    run(&root, &["branch", "other"]);
    fs::write(root.join(".git/refs/heads/broken"), "not a ref\n").unwrap();
    assert_refs_match(&root);
    // A broken loose ref hides the packed one it would override.
    run(&root, &["tag", "packed"]);
    run(&root, &["pack-refs", "--all"]);
    fs::write(root.join(".git/refs/tags/packed"), "not a ref\n").unwrap();
    assert_refs_match(&root);
    // A symbolic ref to nothing, or in a cycle, is left out too.
    run(
        &root,
        &["symbolic-ref", "refs/tags/dangling", "refs/tags/missing"],
    );
    fs::write(root.join(".git/refs/heads/one"), "ref: refs/heads/two\n").unwrap();
    fs::write(root.join(".git/refs/heads/two"), "ref: refs/heads/one\n").unwrap();
    assert_refs_match(&root);
    // So is a ref to the null id, and a symbolic ref to it.
    let null = format!("{}\n", "0".repeat(40));
    fs::write(root.join(".git/refs/heads/null"), null).unwrap();
    run(
        &root,
        &["symbolic-ref", "refs/heads/to-null", "refs/heads/null"],
    );
    assert_refs_match(&root);
}

#[test]
fn a_ref_whose_name_is_not_utf8_leaves_the_others_readable() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let root = init("not-utf8");
    commit(&root, "a", b"a\n");
    let name = OsStr::from_bytes(b"refs/tags/x\xff");
    let status = git(&root)
        .arg("update-ref")
        .arg(name)
        .arg("HEAD")
        .status()
        .unwrap();
    assert!(status.success());
    let repo = Repository::at(root.join(".git"));
    let names = |repo: &Repository| -> Vec<String> {
        repo.refs()
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    };
    assert_eq!(names(&repo), ["refs/heads/main"]);
    run(&root, &["pack-refs", "--all"]);
    assert_eq!(names(&repo), ["refs/heads/main"]);
    assert_resolves_as_git(&root, "refs/heads/main");
}
