//! What `crates/git` reads, checked against the `git` on the host. Each test
//! builds a repository with `git`, and every object and ref in it must read
//! the same from the crate and from `git cat-file`, `git rev-parse`, and
//! `git for-each-ref`, whether the object is loose or packed.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
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
            .object(&id(hex))
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

/// A repository whose history gives `git` much to delta: a file that grows
/// a line at a time, a binary file that changes in place, and tags.
fn history(name: &str) -> PathBuf {
    let root = init(name);
    let mut text = String::new();
    let mut binary: Vec<u8> = (0..20_000u32).map(|i| (i * 7 % 251) as u8).collect();
    for n in 0..60 {
        text.push_str(&format!("line {n} of a file that grows\n"));
        binary[n * 300] ^= 0xff;
        fs::create_dir_all(root.join("dir")).unwrap();
        fs::write(root.join("dir/text"), &text).unwrap();
        fs::write(root.join("binary"), &binary).unwrap();
        run(&root, &["add", "-A"]);
        run(
            &root,
            &["commit", "-q", "-m", &format!("commit {n}\n\n{text}")],
        );
    }
    run(&root, &["tag", "-a", "-m", "annotated", "v1"]);
    root
}

/// The longest delta chain in the repository's packs.
fn longest_chain(root: &Path) -> usize {
    let mut longest = 0;
    for entry in fs::read_dir(root.join(".git/objects/pack")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "idx") {
            let listing = text(root, &["verify-pack", "-v", path.to_str().unwrap()]);
            for line in listing.lines() {
                if let Some(rest) = line.strip_prefix("chain length = ") {
                    longest = longest.max(rest.split(':').next().unwrap().parse().unwrap());
                }
            }
        }
    }
    longest
}

fn assert_all_packed(root: &Path) {
    let counts = text(root, &["count-objects", "-v"]);
    assert!(counts.starts_with("count: 0\n"), "{counts}");
}

#[test]
fn every_object_in_a_pack_reads_as_git_reads_it() {
    // `gc --aggressive` searches more bases, and finds shorter chains.
    for (name, args, deep) in [
        ("offset-deltas", &["repack", "-adf"][..], 20),
        (
            "ref-deltas",
            &["-c", "repack.useDeltaBaseOffset=false", "repack", "-adf"],
            20,
        ),
        (
            "aggressive",
            &["gc", "-q", "--aggressive", "--prune=now"],
            5,
        ),
    ] {
        let root = history(name);
        run(&root, args);
        assert_all_packed(&root);
        let chain = longest_chain(&root);
        assert!(chain >= deep, "{name}: the longest chain is {chain}");
        // 60 commits, 120 trees, 120 blobs, and a tag.
        assert_eq!(assert_objects_match(&root), 301, "{name}");
    }
}

#[test]
fn a_loose_and_a_packed_object_with_the_same_id_read_the_same() {
    let root = history("both");
    // Without `-d`, the loose objects stay beside the pack.
    run(&root, &["repack", "-a"]);
    let repo = Repository::at(root.join(".git"));
    let list = text(
        &root,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ],
    );
    for hex in list.lines() {
        let loose = repo.loose_object(&id(hex)).unwrap().unwrap();
        let packed = repo.packed_object(&id(hex)).unwrap().unwrap();
        assert_eq!(loose, packed, "{hex}");
    }
}

#[test]
fn an_object_in_no_pack_is_missing() {
    let root = history("no-pack");
    run(&root, &["repack", "-adf"]);
    let repo = Repository::at(root.join(".git"));
    let absent = id("0123456789abcdef0123456789abcdef01234567");
    assert!(repo.packed_object(&absent).unwrap().is_none());
    assert!(repo.object(&absent).unwrap().is_none());
    let empty = Repository::at(init("no-pack-at-all").join(".git"));
    assert!(empty.packed_object(&absent).unwrap().is_none());
}

/// The repository's only pack and its index.
fn only_pack(root: &Path) -> (PathBuf, PathBuf) {
    let mut found = Vec::new();
    for entry in fs::read_dir(root.join(".git/objects/pack")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "pack") {
            found.push(path);
        }
    }
    assert_eq!(found.len(), 1, "{found:?}");
    let pack = found.pop().unwrap();
    let idx = pack.with_extension("idx");
    // Git writes both read-only.
    for path in [&idx, &pack] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
    }
    (idx, pack)
}

#[test]
fn an_index_with_64_bit_offsets_reads_as_git_reads_it() {
    let root = history("large-offsets");
    run(&root, &["repack", "-adf"]);
    let (idx, pack) = only_pack(&root);
    let small = fs::metadata(&idx).unwrap().len();
    fs::remove_file(&idx).unwrap();
    // Every offset past 0x40 goes in the table of 64-bit offsets.
    run(
        &root,
        &[
            "index-pack",
            "--index-version=2,0x40",
            pack.to_str().unwrap(),
        ],
    );
    let large = fs::metadata(&idx).unwrap().len();
    assert_eq!(large - small, 8 * 300, "one 64-bit offset for all but one");
    assert_eq!(assert_objects_match(&root), 301);
}

/// Every object `git` lists, read from a fresh `Repository`.
fn read_all(root: &Path, ids: &[Id]) -> Vec<Result<Option<git::Object>, Error>> {
    let repo = Repository::at(root.join(".git"));
    ids.iter().map(|id| repo.packed_object(id)).collect()
}

fn listed_ids(root: &Path) -> Vec<Id> {
    let list = text(
        root,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ],
    );
    list.lines().map(id).collect()
}

#[test]
fn a_truncated_pack_or_index_is_refused() {
    let root = init("truncated");
    commit(&root, "a", &b"some text\n".repeat(20));
    commit(&root, "a", &b"some text\n".repeat(21));
    run(&root, &["repack", "-adf"]);
    let ids = listed_ids(&root);
    let (idx, pack) = only_pack(&root);
    for path in [idx, pack] {
        let whole = fs::read(&path).unwrap();
        for len in 0..whole.len() {
            fs::write(&path, &whole[..len]).unwrap();
            for read in read_all(&root, &ids) {
                assert!(
                    matches!(read, Err(Error::Corrupt(_))),
                    "{path:?} cut to {len}: {read:?}"
                );
            }
        }
        fs::write(&path, &whole).unwrap();
    }
}

#[test]
fn a_changed_byte_in_a_pack_or_index_reads_as_git_reads_it_or_is_refused() {
    let root = init("flipped");
    commit(&root, "a", &b"some text\n".repeat(20));
    commit(&root, "a", &b"some text\n".repeat(21));
    run(&root, &["repack", "-adf"]);
    let ids = listed_ids(&root);
    let good = read_all(&root, &ids);
    let (idx, pack) = only_pack(&root);
    for path in [idx, pack] {
        let whole = fs::read(&path).unwrap();
        for at in 0..whole.len() {
            let mut changed = whole.clone();
            changed[at] ^= 0x41;
            fs::write(&path, &changed).unwrap();
            for ((id, read), good) in ids.iter().zip(read_all(&root, &ids)).zip(&good) {
                match read {
                    Err(Error::Corrupt(_)) | Ok(None) => {}
                    Ok(Some(object)) if Some(&object) == good.as_ref().unwrap().as_ref() => {}
                    // Git reads what it can't tell was changed, and so must
                    // the crate.
                    Ok(Some(object)) => {
                        let hex = id.to_string();
                        let kind = object.kind.to_string();
                        assert_eq!(
                            object.data,
                            run(&root, &["cat-file", &kind, &hex]),
                            "{path:?} changed at {at}: {hex}"
                        );
                    }
                    Err(error) => panic!("{path:?} changed at {at}: {error}"),
                }
            }
        }
        fs::write(&path, &whole).unwrap();
    }
}

/// An entry's header: its type and size, as a pack writes them.
fn entry_header(kind: u8, size: usize) -> Vec<u8> {
    let mut bytes = vec![(kind << 4) | (size & 15) as u8];
    let mut size = size >> 4;
    while size > 0 {
        *bytes.last_mut().unwrap() |= 0x80;
        bytes.push((size & 0x7f) as u8);
        size >>= 7;
    }
    bytes
}

/// A pack entry of a whole object.
fn whole(kind: u8, data: &[u8]) -> Vec<u8> {
    let mut entry = entry_header(kind, data.len());
    entry.extend(miniz_oxide::deflate::compress_to_vec_zlib(data, 6));
    entry
}

/// A pack entry of a delta against the object named `base`.
fn ref_delta(base: &Id, delta: &[u8]) -> Vec<u8> {
    let mut entry = entry_header(7, delta.len());
    entry.extend(base.0);
    entry.extend(miniz_oxide::deflate::compress_to_vec_zlib(delta, 6));
    entry
}

/// Writes a pack of `entries`, each with the name given, and its version 2
/// index. Neither is checked against its contents, as `git` doesn't check
/// them when it reads an object.
fn plant_pack(root: &Path, entries: &[(Id, Vec<u8>)]) {
    let mut pack = b"PACK\0\0\0\x02".to_vec();
    pack.extend((entries.len() as u32).to_be_bytes());
    let mut offsets = Vec::new();
    for (id, entry) in entries {
        offsets.push((*id, pack.len() as u32));
        pack.extend(entry);
    }
    let checksum = [0x5a; 20];
    pack.extend(checksum);
    offsets.sort();
    let mut idx = b"\xfftOc\0\0\0\x02".to_vec();
    for first in 0..=255u8 {
        let below = offsets.iter().filter(|(id, _)| id.0[0] <= first).count();
        idx.extend((below as u32).to_be_bytes());
    }
    for (id, _) in &offsets {
        idx.extend(id.0);
    }
    for _ in &offsets {
        idx.extend([0; 4]);
    }
    for (_, offset) in &offsets {
        idx.extend(offset.to_be_bytes());
    }
    idx.extend(checksum);
    idx.extend([0; 20]);
    let dir = root.join(".git/objects/pack");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("pack-planted.pack"), pack).unwrap();
    fs::write(dir.join("pack-planted.idx"), idx).unwrap();
}

/// A delta's header: the base's size, then the result's.
fn delta_header(base: usize, result: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    for mut size in [base, result] {
        loop {
            let more = if size >= 0x80 { 0x80 } else { 0 };
            bytes.push((size & 0x7f) as u8 | more);
            size >>= 7;
            if more == 0 {
                break;
            }
        }
    }
    bytes
}

#[test]
fn a_planted_delta_reads_as_git_reads_it() {
    let base = id(&"a".repeat(40));
    let target = id(&"b".repeat(40));
    let cases: Vec<Vec<u8>> = vec![
        // Copy all 11 bytes of the base, then insert 3.
        [delta_header(11, 14), vec![0x90, 11, 3], b"!!!".to_vec()].concat(),
        // A copy of size 0 means 0x10000, more than the base holds.
        [delta_header(11, 11), vec![0x80]].concat(),
        // A copy past the end of the base.
        [delta_header(11, 11), vec![0x91, 1, 11]].concat(),
        // An insert past the end of the delta.
        [delta_header(11, 3), vec![3, b'x']].concat(),
        // The reserved opcode.
        [delta_header(11, 1), vec![0, 1, b'x']].concat(),
        // The wrong size of base.
        [delta_header(10, 1), vec![1, b'x']].concat(),
        // A result shorter or longer than the header says.
        [delta_header(11, 2), vec![1, b'x']].concat(),
        [delta_header(11, 1), vec![2, b'x', b'y']].concat(),
        // Shorter than any delta git takes.
        delta_header(0, 0),
    ];
    for (n, delta) in cases.iter().enumerate() {
        let root = init(&format!("delta-{n}"));
        plant_pack(
            &root,
            &[
                (base, whole(3, b"hello world")),
                (target, ref_delta(&base, delta)),
            ],
        );
        let hex = target.to_string();
        let theirs = output(&root, &["cat-file", "blob", &hex], b"");
        let ours = Repository::at(root.join(".git")).object(&target);
        match ours {
            Ok(Some(object)) => {
                assert!(theirs.status.success(), "{n}: git refuses {delta:?}");
                assert_eq!(object.kind, Kind::Blob, "{n}");
                assert_eq!(object.data, theirs.stdout, "{n}");
            }
            Err(Error::Corrupt(_)) => {
                assert!(!theirs.status.success(), "{n}: git reads {delta:?}");
            }
            other => panic!("{n}: {other:?}"),
        }
    }
}

#[test]
fn a_planted_entry_git_refuses_is_refused() {
    let one = id(&"a".repeat(40));
    let two = id(&"b".repeat(40));
    let delta = [delta_header(1, 1), vec![1, b'x']].concat();
    let cases: Vec<Vec<(Id, Vec<u8>)>> = vec![
        // Types 0 and 5 name no object.
        vec![(one, whole(0, b"x"))],
        vec![(one, whole(5, b"x"))],
        // A size larger than the data.
        vec![(
            one,
            [entry_header(3, 2), whole(3, b"x")[1..].to_vec()].concat(),
        )],
        // A base in no pack.
        vec![(one, ref_delta(&two, &delta))],
        // Two deltas, each the other's base.
        vec![
            (one, ref_delta(&two, &delta)),
            (two, ref_delta(&one, &delta)),
        ],
        // An offset delta whose base is before the pack.
        vec![(one, [entry_header(6, delta.len()), vec![0x7f]].concat())],
    ];
    for (n, entries) in cases.iter().enumerate() {
        let root = init(&format!("entry-{n}"));
        plant_pack(&root, entries);
        let ours = Repository::at(root.join(".git")).object(&one);
        assert!(matches!(ours, Err(Error::Corrupt(_))), "{n}: {ours:?}");
        // A cycle of deltas is left to the crate alone.
        if n != 4 {
            let theirs = output(&root, &["cat-file", "-p", &one.to_string()], b"");
            assert!(!theirs.status.success(), "{n}: git reads it");
        }
    }
}
