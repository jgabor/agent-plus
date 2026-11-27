//! `cargo xtask gate`: the checks a change must pass.

use crate::lock::Hold;
use crate::workspace::{self, no_cargo};
use crate::{Problem, Verdict, base, bench, floors, ledger, lock, score, testrun, toml};
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::{fs, io};

struct Check {
    name: &'static str,
    args: &'static [&'static str],
}

const CHECKS: &[Check] = &[
    Check {
        name: "format",
        args: &["fmt", "--all", "--check"],
    },
    Check {
        name: "lint",
        args: &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--locked",
            "--",
            "-D",
            "warnings",
        ],
    },
    // Lints the code that only macOS builds, which needn't link, so Linux
    // can check it.
    Check {
        name: "lint-macos",
        args: &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--locked",
            "--target",
            "aarch64-apple-darwin",
            "--",
            "-D",
            "warnings",
        ],
    },
];

/// The package that holds the T2 checks.
pub const T2_PACKAGE: &str = "t2";

/// What an adapter change touches, as HARNESS.md lists it: the adapters, how
/// a harness release is fetched and locked, the T2 checks and the fake model
/// they talk to, and the guest that a harness runs on.
const ADAPTER_PATHS: [&str; 6] = [
    "crates/ap/src/adapter/",
    "crates/ap/src/lock.rs",
    "crates/t2/",
    "crates/fake-upstream/",
    "guest/os/",
    "guest/kernel/",
];

/// What a merge already knows, which changes two of the checks.
pub struct Merging<'a> {
    /// `prepare` leaves these to the owner, who decides by merging.
    pub prepare: bool,
    /// Hears the name of a slow check as it starts.
    pub starting: &'a dyn Fn(&str),
}

/// The checks that failed, in the order they ran.
pub type Failed = Vec<&'static str>;

pub fn verdict(failed: &Failed) -> Verdict {
    if failed.is_empty() {
        Verdict::Pass
    } else {
        Verdict::Fail
    }
}

pub fn run() -> Result<Verdict, Problem> {
    let root = workspace::root()?;
    let mut hold = lock::testing(&root)?;
    let (failed, _) = check(&root, &mut hold, None)?;
    Ok(verdict(&failed))
}

/// Runs every check, and returns those that failed and the spec score that
/// the tests earned. A T2 check that didn't run earns nothing.
pub fn check(
    root: &Path,
    hold: &mut Hold,
    merging: Option<&Merging>,
) -> Result<(Failed, score::Score), Problem> {
    require_tools(root)?;
    let mut failed = Failed::new();
    let findings = floors::check(root)?;
    for finding in &findings {
        eprintln!("floor: {finding}");
    }
    record("floors", findings.is_empty(), &mut failed);
    for check in CHECKS {
        let passed = cargo_passes(root, check.args)?;
        record(check.name, passed, &mut failed);
    }
    if let Some(merging) = merging {
        (merging.starting)("tests");
    }
    let kept = Kept::find(root)?;
    for kept in &kept {
        kept.copy(Side::Tree);
    }
    let mut results = testrun::run_except(root, hold, Some(T2_PACKAGE))?;
    for kept in &kept {
        kept.copy(Side::Store);
    }
    record("tests", results.all_passed(), &mut failed);
    let has_t2 = workspace::packages(root)?
        .iter()
        .any(|package| package.name == T2_PACKAGE);
    if has_t2 && needs_t2(root)? {
        if let Some(merging) = merging {
            (merging.starting)(T2_PACKAGE);
        }
        let passed = testrun::run_package(root, T2_PACKAGE, hold, &mut results)?;
        record("t2", passed, &mut failed);
    } else if has_t2 {
        println!("skip t2");
        eprintln!("t2: no change touches an adapter path, so T2 didn't run. HARNESS.md lists them");
    }
    let score = score::compute(root, &results)?;
    score::report_findings(&score);
    record("spec", score.findings.is_empty(), &mut failed);
    compare_with_base(root, &score, merging, &mut failed)?;
    let word = match verdict(&failed) {
        Verdict::Pass => "pass",
        Verdict::Fail => "fail",
    };
    println!("gate: {word}");
    Ok((failed, score))
}

/// Whether the tree, with its untracked files, differs from its base on an
/// adapter path, a file or a directory that ends in `/`. A tree with no base,
/// or one that differs from it only in the ledger, where an earlier attempt at
/// a record may be, is `main` as it is, which T2 checks too.
fn needs_t2(root: &Path) -> Result<bool, Problem> {
    let Some(commit) = base::find(root)? else {
        return Ok(true);
    };
    let changed = base::git_text(
        root,
        &["diff", "--name-only", "--no-renames", "-z", &commit, "--"],
    )?;
    let untracked = base::git_text(root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    let mut paths = changed
        .split('\0')
        .chain(untracked.split('\0'))
        .filter(|path| !path.is_empty() && !path.starts_with(&format!("{}/", ledger::DIRECTORY)))
        .peekable();
    let unchanged = paths.peek().is_none();
    Ok(unchanged
        || paths.any(|path| {
            ADAPTER_PATHS
                .iter()
                .any(|entry| path == *entry || (entry.ends_with('/') && path.starts_with(entry)))
        }))
}

fn compare_with_base(
    root: &Path,
    score: &score::Score,
    merging: Option<&Merging>,
    failed: &mut Failed,
) -> Result<(), Problem> {
    let Some(commit) = base::find(root)? else {
        println!("skip base");
        eprintln!("base: no origin/main in this repository, so nothing was compared");
        return Ok(());
    };
    let allowed = merging.is_some_and(|merging| merging.prepare);
    let findings = if allowed && base::is_exact_revert(root, &commit)? {
        Vec::new()
    } else {
        base::removed_checks(root, &commit, score)?
    };
    for finding in &findings {
        eprintln!("floor: {finding}");
    }
    record("checks-kept", findings.is_empty(), failed);
    let floor = ledger::newest(root, &commit)?.map_or(0, |base| base.proven);
    if score.proven < floor {
        eprintln!(
            "hills: proven promises fell from {floor} to {}. Restore what proved them, or hand the PR to the owner",
            score.proven
        );
    }
    if merging.is_none() && score.proven == floor {
        eprintln!(
            "hills: the spec count is level with the base's at {floor}, so the PR body needs a `Hill: none, <reason>` line unless the journey or a budget rises"
        );
    }
    record("hills", score.proven >= floor || allowed, failed);
    Ok(())
}

/// A pinned guest file, or a directory of files, that t1's tests fetch into
/// the tree. A store in the shared git directory keeps it, so that a fresh
/// tree needn't fetch it.
struct Kept {
    name: &'static str,
    digest: String,
    tree: PathBuf,
    store: PathBuf,
    /// The files in a directory, relative to it, or none for a file. The last
    /// one has the pinned digest.
    files: Vec<PathBuf>,
}

impl Kept {
    /// What the workspace pins. A pin that t1 can't read fails its tests, so
    /// the gate needn't say so.
    fn find(root: &Path) -> Result<Vec<Kept>, Problem> {
        let store = lock::directory(root)?;
        let mut kept = Vec::new();
        if let Some(digest) = pinned(root, "guest/kernel/pin.toml") {
            kept.push(Kept {
                name: "kernel",
                digest,
                tree: root.join("target/guest/vmlinux"),
                store: store.join("kernel/vmlinux"),
                files: Vec::new(),
            });
        }
        if let Some(os) = Os::find(root) {
            kept.push(Kept {
                name: "os",
                files: os.layers().into(),
                digest: os.digest,
                tree: os.seed,
                store: store.join("os"),
            });
        }
        Ok(kept)
    }

    /// Copies it from the store to the tree, or from the tree to the store,
    /// unless the side it goes to holds it. The copy replaces what was there
    /// only once its digest matches, so a reader never sees part of one. It
    /// saves a download, so a failure only warns.
    fn copy(&self, side: Side) {
        let (from, to) = match side {
            Side::Tree => (&self.store, &self.tree),
            Side::Store => (&self.tree, &self.store),
        };
        if self.whole(to) || !from.exists() {
            return;
        }
        let partial = to.with_extension(format!("{}.part", process::id()));
        let name = self.name;
        if let Err(e) = self.copy_files(from, &partial) {
            eprintln!("{name}: couldn't copy {}: {e}", from.display());
        } else if !self.whole(&partial) {
            eprintln!("{name}: {} doesn't match pin.toml", from.display());
        } else {
            if to.is_dir() {
                let _ = fs::remove_dir_all(to);
            }
            if let Err(e) = fs::rename(&partial, to) {
                eprintln!("{name}: couldn't move it to {}: {e}", to.display());
            }
        }
        let _ = fs::remove_file(&partial).or_else(|_| fs::remove_dir_all(&partial));
    }

    fn copy_files(&self, from: &Path, to: &Path) -> io::Result<()> {
        to.parent().map_or(Ok(()), fs::create_dir_all)?;
        if self.files.is_empty() {
            return fs::copy(from, to).map(drop);
        }
        for file in &self.files {
            let target = to.join(file);
            target.parent().map_or(Ok(()), fs::create_dir_all)?;
            fs::copy(from.join(file), target)?;
        }
        Ok(())
    }

    /// Whether `path` holds every file, and the last has the pinned digest.
    fn whole(&self, path: &Path) -> bool {
        if self.files.is_empty() {
            return whole(&[path], &self.digest);
        }
        whole(
            &self
                .files
                .iter()
                .map(|file| path.join(file))
                .collect::<Vec<_>>(),
            &self.digest,
        )
    }
}

/// Where a kept file is copied to.
enum Side {
    Tree,
    Store,
}

/// The pinned `OS minimal` layer, as t1's tests keep it in the tree: in a
/// store under `target/guest` that holds only that layer.
pub struct Os {
    pub digest: String,
    pub seed: PathBuf,
}

impl Os {
    /// `None` when the workspace pins no OS layer.
    pub fn find(root: &Path) -> Option<Os> {
        let digest = pinned(root, "guest/os/pin.toml")?;
        let seed = root.join("target/guest").join(format!("os-{digest}"));
        Some(Os { digest, seed })
    }

    /// The layer's files, as a store keeps them, relative to the store: its
    /// frame, then its uncompressed copy, which has the pinned digest.
    pub fn layers(&self) -> [PathBuf; 2] {
        ["zstd-layers", "plain-layers"].map(|directory| Path::new(directory).join(&self.digest))
    }

    /// Whether the tree holds both files, and the uncompressed one matches the
    /// pin. t1 checks the frame itself before it uses one.
    pub fn in_tree(&self) -> bool {
        whole(
            &self.layers().map(|layer| self.seed.join(layer)),
            &self.digest,
        )
    }
}

/// The digest that the pin at `file` names, or `None` if it names none.
fn pinned(root: &Path, file: &str) -> Option<String> {
    let text = fs::read_to_string(root.join(file)).unwrap_or_default();
    let pin = toml::document(&text).unwrap_or_default();
    pin.str("sha256").map(str::to_string)
}

/// Whether every file exists, and the last one has this digest.
fn whole(files: &[impl AsRef<Path>], digest: &str) -> bool {
    files.iter().all(|file| file.as_ref().exists())
        && files
            .last()
            .is_some_and(|last| matches(last.as_ref(), digest))
}

fn matches(path: &Path, digest: &str) -> bool {
    path.exists() && bench::sha256(path).is_ok_and(|actual| actual == digest)
}

pub fn record(name: &'static str, passed: bool, failed: &mut Failed) {
    println!("{} {name}", if passed { "pass" } else { "fail" });
    if !passed {
        failed.push(name);
    }
}

/// Fails before any check runs when a tool is missing, because a missing tool
/// says nothing about the change.
pub fn require_tools(root: &Path) -> Result<(), Problem> {
    for (tool, component) in [("fmt", "rustfmt"), ("clippy", "clippy")] {
        let output = Command::new("cargo")
            .args([tool, "--version"])
            .current_dir(root)
            .output()
            .map_err(no_cargo)?;
        if !output.status.success() {
            return Err(Problem(format!(
                "`cargo {tool}` can't run: {}. Install it with `rustup component add {component}`",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
    }
    Ok(())
}

/// Runs a cargo command, with its output on our stderr, so that stdout carries
/// only the result. `CC` and `CXX` point at a program that fails, so a build
/// that tries to compile C fails even if the floors missed it. In a merge,
/// our input is the pipe to the supervisor, which the tree's code mustn't
/// reach.
pub fn cargo_passes(root: &Path, args: &[&str]) -> Result<bool, Problem> {
    let status = Command::new("cargo")
        .args(args)
        .current_dir(root)
        .env("CC", "false")
        .env("CXX", "false")
        .stdin(Stdio::null())
        .stdout(Stdio::from(io::stderr()))
        .status()
        .map_err(no_cargo)?;
    Ok(status.success())
}
