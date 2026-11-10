//! Which files git ignores, checked against `git check-ignore` itself. Each
//! test builds a directory, and every path in it must get the same answer
//! from the module and from git.

use std::fs;
use std::io::Write;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use ap::gitignore::Gitignore;

/// What to put in a test directory. A name ending in `/` is a directory, a
/// name containing ` -> ` is a symlink, and anything else is a file with the
/// given contents.
type Tree<'a> = &'a [(&'a str, &'a str)];

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("gitignore")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn build(root: &Path, tree: Tree) {
    for (name, contents) in tree {
        if let Some((link, target)) = name.split_once(" -> ") {
            let link = root.join(link);
            fs::create_dir_all(link.parent().unwrap()).unwrap();
            symlink(target, link).unwrap();
        } else if let Some(dir) = name.strip_suffix('/') {
            fs::create_dir_all(root.join(dir)).unwrap();
        } else {
            let file = root.join(name);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, contents).unwrap();
        }
    }
}

/// Every path under `root`, relative to it, except `.git`.
fn paths(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, bool)>) {
    let mut entries: Vec<_> = fs::read_dir(root.join(dir))
        .unwrap()
        .map(|e| e.unwrap())
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let rel = dir.join(entry.file_name());
        if rel == Path::new(".git") {
            continue;
        }
        let is_dir = entry.file_type().unwrap().is_dir();
        out.push((rel.clone(), is_dir));
        if is_dir {
            paths(root, &rel, out);
        }
    }
}

fn git(root: &Path) -> Command {
    let mut git = Command::new("git");
    // Only the `.gitignore` files in the tree may count, so git must not
    // read the owner's configuration or global excludes.
    git.current_dir(root)
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    git
}

/// The paths git ignores, of those given.
fn oracle(root: &Path, paths: &[(PathBuf, bool)]) -> Vec<PathBuf> {
    let status = git(root).args(["init", "-q"]).status().unwrap();
    assert!(status.success());
    let mut child = git(root)
        .args(["check-ignore", "--stdin", "-z", "--no-index"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = Vec::new();
    for (path, _) in paths {
        input.extend_from_slice(path.as_os_str().as_encoded_bytes());
        input.push(0);
    }
    child.stdin.take().unwrap().write_all(&input).unwrap();
    let output = child.wait_with_output().unwrap();
    // 0 means some path is ignored, and 1 means none is.
    assert!(matches!(output.status.code(), Some(0 | 1)), "{output:?}");
    output
        .stdout
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| PathBuf::from(String::from_utf8(p.to_vec()).unwrap()))
        .collect()
}

/// Builds `tree`, and checks that the module ignores exactly what git does.
fn agrees(name: &str, tree: Tree) {
    let root = scratch(name);
    build(&root, tree);
    let mut all = Vec::new();
    paths(&root, Path::new(""), &mut all);
    let want = oracle(&root, &all);
    let ignore = Gitignore::load(&root).unwrap();
    let got: Vec<PathBuf> = all
        .iter()
        .filter(|(path, is_dir)| ignore.is_ignored(path, *is_dir))
        .map(|(path, _)| path.clone())
        .collect();
    assert_eq!(got, want, "in {}", root.display());
}

#[test]
fn comments_and_blank_lines() {
    agrees(
        "comments",
        &[
            (
                ".gitignore",
                "# a.txt\n\n   \n#b.txt\n \\#c.txt\n\\#d.txt\n",
            ),
            ("a.txt", ""),
            ("#b.txt", ""),
            ("#d.txt", ""),
            ("e.txt", ""),
        ],
    );
}

#[test]
fn trailing_spaces() {
    agrees(
        "spaces",
        &[
            (".gitignore", "a.txt   \nb\\ \nc\\  \n d.txt\n"),
            ("a.txt", ""),
            ("b ", ""),
            ("c ", ""),
            (" d.txt", ""),
            ("d.txt", ""),
        ],
    );
}

#[test]
fn crlf_and_bom() {
    agrees(
        "crlf",
        &[
            (".gitignore", "\u{feff}a.txt\r\nb.txt\r\n"),
            ("a.txt", ""),
            ("b.txt", ""),
            ("c.txt", ""),
        ],
    );
}

#[test]
fn negation() {
    agrees(
        "negation",
        &[
            (".gitignore", "*.log\n!keep.log\n\\!bang\n!\n"),
            ("a.log", ""),
            ("keep.log", ""),
            ("sub/keep.log", ""),
            ("sub/b.log", ""),
            ("!bang", ""),
        ],
    );
}

#[test]
fn negation_cannot_reach_into_an_ignored_directory() {
    agrees(
        "reinclude",
        &[
            (".gitignore", "build/\n!build/keep\n"),
            ("build/keep", ""),
            ("build/other", ""),
            ("build/.gitignore", "!*\n"),
            ("build/deep/x", ""),
        ],
    );
}

#[test]
fn negation_of_contents_after_star() {
    agrees(
        "star-reinclude",
        &[
            (".gitignore", "/out/*\n!/out/keep\n"),
            ("out/keep", ""),
            ("out/drop", ""),
            ("out/dir/x", ""),
        ],
    );
}

#[test]
fn directory_only_patterns() {
    agrees(
        "dironly",
        &[
            (".gitignore", "cache/\nlink/\nfile/\n"),
            ("cache/a", ""),
            ("sub/cache/b", ""),
            ("file", ""),
            ("target/", ""),
            ("link -> target", ""),
        ],
    );
}

#[test]
fn anchored_patterns() {
    agrees(
        "anchored",
        &[
            (".gitignore", "/top\nsub/mid\na/b/\n"),
            ("top", ""),
            ("x/top", ""),
            ("sub/mid", ""),
            ("x/sub/mid", ""),
            ("a/b/c", ""),
            ("x/a/b/c", ""),
        ],
    );
}

#[test]
fn star_and_question_mark() {
    agrees(
        "star",
        &[
            (".gitignore", "*.o\n/d*/x\nf?o\n/q/?\n"),
            ("a.o", ""),
            ("sub/b.o", ""),
            ("d1/x", ""),
            ("d1/e/x", ""),
            ("foo", ""),
            ("fo", ""),
            ("fooo", ""),
            ("q/a", ""),
            ("q/ab", ""),
        ],
    );
}

#[test]
fn character_ranges() {
    agrees(
        "ranges",
        &[
            (
                ".gitignore",
                "[a-c].txt\nx[!0-4]\ny[^0-4]\n[]z]z\n[[:digit:]][[:upper:]]\n\\*star\nq[\\]]\nr[a-]\n",
            ),
            ("a.txt", ""),
            ("d.txt", ""),
            ("x3", ""),
            ("x7", ""),
            ("y3", ""),
            ("y7", ""),
            ("]z", ""),
            ("zz", ""),
            ("az", ""),
            ("1A", ""),
            ("1a", ""),
            ("*star", ""),
            ("xstar", ""),
            ("q]", ""),
            ("r-", ""),
            ("ra", ""),
            ("rb", ""),
        ],
    );
}

#[test]
fn double_stars() {
    agrees(
        "doublestar",
        &[
            (
                ".gitignore",
                "**/logs\n/out/**\na/**/b\nc**/d\ne/**f\n**\\/g\n",
            ),
            ("logs", ""),
            ("x/y/logs/z", ""),
            ("out/1/2", ""),
            ("outside", ""),
            ("a/b", ""),
            ("a/x/y/b", ""),
            ("cx/d", ""),
            ("c/d", ""),
            ("c/x/d", ""),
            ("e/f", ""),
            ("e/xf", ""),
            ("e/x/f", ""),
            ("g", ""),
            ("x/g", ""),
        ],
    );
}

#[test]
fn nested_gitignore_files() {
    agrees(
        "nested",
        &[
            (".gitignore", "*.tmp\nsecret\n"),
            ("a.tmp", ""),
            ("sub/.gitignore", "!keep.tmp\n/only-here\nsecret/\n"),
            ("sub/keep.tmp", ""),
            ("sub/drop.tmp", ""),
            ("sub/only-here", ""),
            ("only-here", ""),
            ("sub/deep/only-here", ""),
            ("sub/secret", ""),
            ("sub/deep/.gitignore", "!secret\n"),
            ("sub/deep/secret", ""),
            ("other/keep.tmp", ""),
        ],
    );
}

#[test]
fn a_gitignore_does_not_apply_to_its_own_directory() {
    agrees(
        "own-dir",
        &[
            ("sub/.gitignore", "sub\n/\n*\n!.gitignore\n"),
            ("sub/x", ""),
        ],
    );
}

#[test]
fn a_symlinked_gitignore_is_not_read() {
    agrees(
        "symlink",
        &[("rules", "a\n"), (".gitignore -> rules", ""), ("a", "")],
    );
}

/// A small generator, so a failure prints the seed that reproduces it.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.next() as usize % items.len()]
    }
}

#[test]
fn random_trees_agree_with_git() {
    const NAMES: &[&str] = &["a", "b", "ab", "a.c", "b.c", ".h", "[a]", "a b"];
    const DIRS: &[&str] = &["", "a/", "b/", "a/b/", "x/a/", "a/a/a/"];
    const PIECES: &[&str] = &[
        "a", "b", "*", "?", "**", "/", "/", "[ab]", "[!a]", ".c", "\\", " ", "!", "#", "[", "]",
        "-", "x",
    ];
    for seed in 1..=150u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let mut files: Vec<(String, String)> = Vec::new();
        for _ in 0..12 {
            let dir = rng.pick(DIRS);
            files.push((format!("{dir}{}", rng.pick(NAMES)), String::new()));
        }
        for _ in 0..3 {
            let mut rules = String::new();
            for _ in 0..(1 + rng.next() % 4) {
                for _ in 0..(1 + rng.next() % 5) {
                    rules.push_str(rng.pick(PIECES));
                }
                rules.push('\n');
            }
            files.push((format!("{}.gitignore", rng.pick(DIRS)), rules));
        }
        // A file and a directory can't share a name, so the first one wins.
        let mut tree: Vec<(&str, &str)> = Vec::new();
        for (name, contents) in &files {
            let clash = tree.iter().any(|(other, _)| {
                other == name
                    || other.starts_with(&format!("{name}/"))
                    || name.starts_with(&format!("{other}/"))
            });
            if !clash {
                tree.push((name, contents));
            }
        }
        agrees(&format!("random-{seed}"), &tree);
    }
}
