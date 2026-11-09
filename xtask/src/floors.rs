//! The floors that Cargo's own view of the tree can check: no C anywhere in the tree, and
//! no build script that hasn't been audited.

use crate::Problem;
use crate::json::{self, Json};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The hosts v0.1.0 supports, and the static build the journey and the bench
/// run. Each one resolves a different dependency graph.
const TARGETS: [&str; 4] = [
    "x86_64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-gnu",
    "aarch64-apple-darwin",
];

const C_BUILDERS: [&str; 4] = ["cc", "cmake", "bindgen", "pkg-config"];

const FOREIGN_EXTENSIONS: &[&str] = &[
    "c", "C", "h", "H", "cc", "cp", "cpp", "cxx", "c++", "hh", "hpp", "hxx", "h++", "m", "mm", "S",
    "s", "sx", "asm", "a", "o", "obj", "so", "dylib", "lib", "dll",
];

const CRATES_IO: &str = "registry+https://github.com/rust-lang/crates.io-index";

const AUDIT_FILE: &str = "harness/build-scripts.txt";

/// Returns one message for each broken rule. An empty list means the floors
/// hold.
pub fn check(root: &Path) -> Result<Vec<String>, Problem> {
    let audited = read_audit_list(root)?;
    let metadata = TARGETS
        .iter()
        .map(|target| read_metadata(root, target))
        .collect::<Result<Vec<_>, _>>()?;
    let unexpected = |e: String| Problem(format!("unexpected `cargo metadata` output: {e}"));
    let mut known = HashSet::new();
    for metadata in &metadata {
        for package in list(metadata, "packages").map_err(unexpected)? {
            known.insert(package_key(package).map_err(unexpected)?);
        }
    }
    let host_side = packages_cargo_builds(root, "all")?;
    let mut findings = BTreeSet::new();
    for (target, metadata) in TARGETS.iter().zip(&metadata) {
        let target_side = packages_cargo_builds(root, target)?;
        // A line no package matches means Cargo's format moved, and a package
        // that fails to match would escape the floor.
        if let Some(key) = target_side.iter().find(|key| !known.contains(*key)) {
            return Err(Problem(format!(
                "`cargo tree` names a package `cargo metadata` doesn't: `{key}`"
            )));
        }
        let built = Built {
            target_side: &target_side,
            host_side: &host_side,
        };
        inspect(metadata, &audited, built, &mut findings).map_err(unexpected)?;
    }
    Ok(findings.into_iter().collect())
}

/// What Cargo builds. `resolve.nodes` also lists packages reachable only
/// through a weak feature, like `dep?/x`, and nothing builds those.
///
/// `cargo tree --target <triple>` settles features for that platform, but it
/// matches build scripts, proc macros, and their dependencies against this
/// machine instead of the target. So the host side falls back on `--target
/// all`, which keeps every platform and can only reject too much.
#[derive(Clone, Copy)]
struct Built<'a> {
    target_side: &'a HashSet<String>,
    host_side: &'a HashSet<String>,
}

fn read_metadata(root: &Path, target: &str) -> Result<Json, Problem> {
    let output = Command::new("cargo")
        .args([
            "metadata",
            "--format-version",
            "1",
            "--locked",
            "--all-features",
            "--filter-platform",
            target,
        ])
        .current_dir(root)
        .output()
        .map_err(|e| Problem(format!("couldn't run `cargo metadata`: {e}")))?;
    if !output.status.success() {
        return Err(Problem(format!(
            "`cargo metadata` failed for {target}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    json::parse(&String::from_utf8_lossy(&output.stdout))
        .map_err(|e| Problem(format!("couldn't read `cargo metadata` output: {e}")))
}

/// Returns the key of every package Cargo builds for `target`, which is a
/// triple or `all`.
fn packages_cargo_builds(root: &Path, target: &str) -> Result<HashSet<String>, Problem> {
    let output = Command::new("cargo")
        .args([
            "tree",
            "--locked",
            "--workspace",
            "--all-features",
            "--edges",
            "normal,build,dev",
            "--target",
            target,
            "--prefix",
            "none",
            "--format",
            "{p}",
        ])
        .current_dir(root)
        .output()
        .map_err(|e| Problem(format!("couldn't run `cargo tree`: {e}")))?;
    if !output.status.success() {
        return Err(Problem(format!(
            "`cargo tree` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .map(tree_line_key)
        .collect()
}

/// Turns `name v1.0.0 (proc-macro) (source) (*)` into the key `package_key`
/// makes for the same package.
fn tree_line_key(line: &str) -> Result<String, Problem> {
    let unexpected = || Problem(format!("unexpected `cargo tree` line: `{line}`"));
    let line = line.strip_suffix(" (*)").unwrap_or(line);
    let (name, rest) = line.split_once(" v").ok_or_else(unexpected)?;
    let rest = rest.replacen(" (proc-macro)", "", 1);
    if name.is_empty() || rest.is_empty() {
        return Err(unexpected());
    }
    Ok(format!("{name} {rest}"))
}

/// Identifies a package by source as well as name and version, because one
/// tree can hold the same name and version from two sources. It follows how
/// `cargo tree` prints a source.
fn package_key(package: &Json) -> Result<String, String> {
    let label = format!("{} {}", text(package, "name")?, text(package, "version")?);
    let source = field(package, "source")?.as_str();
    Ok(match source {
        None => {
            let manifest = Path::new(text(package, "manifest_path")?);
            let directory = manifest.parent().ok_or("a manifest has no parent")?;
            format!("{label} ({})", directory.display())
        }
        Some(CRATES_IO) => label,
        Some(other) => {
            let url = other.strip_prefix("git+").unwrap_or(other);
            match url.rsplit_once('#') {
                Some((base, commit)) => {
                    format!("{label} ({base}#{})", commit.get(..8).unwrap_or(commit))
                }
                None => format!("{label} ({url})"),
            }
        }
    })
}

/// Reads `<name> <version> <reason>` lines into `name version` keys.
fn read_audit_list(root: &Path) -> Result<HashSet<String>, Problem> {
    let path = root.join(AUDIT_FILE);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashSet::new()),
        Err(e) => return Err(Problem(format!("couldn't read {AUDIT_FILE}: {e}"))),
    };
    let mut audited = HashSet::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut words = line.split_whitespace();
        match (words.next(), words.next(), words.next()) {
            (Some(name), Some(version), Some(_reason)) => {
                audited.insert(format!("{name} {version}"));
            }
            _ => {
                return Err(Problem(format!(
                    "{AUDIT_FILE}:{}: expected `<name> <version> <reason>`, got `{line}`",
                    index + 1
                )));
            }
        }
    }
    Ok(audited)
}

fn field<'a>(value: &'a Json, key: &str) -> Result<&'a Json, String> {
    value.get(key).ok_or_else(|| format!("missing `{key}`"))
}

fn text<'a>(value: &'a Json, key: &str) -> Result<&'a str, String> {
    field(value, key)?
        .as_str()
        .ok_or_else(|| format!("`{key}` isn't a string"))
}

fn list<'a>(value: &'a Json, key: &str) -> Result<&'a [Json], String> {
    field(value, key)?
        .as_arr()
        .ok_or_else(|| format!("`{key}` isn't a list"))
}

fn inspect(
    metadata: &Json,
    audited: &HashSet<String>,
    built: Built<'_>,
    findings: &mut BTreeSet<String>,
) -> Result<(), String> {
    let mut packages = HashMap::new();
    for package in list(metadata, "packages")? {
        packages.insert(text(package, "id")?, package);
    }
    let mut members = HashSet::new();
    for member in list(metadata, "workspace_members")? {
        members.insert(member.as_str().ok_or("a member isn't a string")?);
    }
    let mut edges: HashMap<&str, Vec<(&str, bool)>> = HashMap::new();
    for node in list(field(metadata, "resolve")?, "nodes")? {
        let mut deps = Vec::new();
        for dep in list(node, "deps")? {
            let pkg = text(dep, "pkg")?;
            let kinds = list(dep, "dep_kinds")?;
            if kinds.is_empty() {
                return Err(format!("`{pkg}` has no `dep_kinds`"));
            }
            for kind in kinds {
                deps.push((pkg, field(kind, "kind")?.as_str() == Some("build")));
            }
        }
        edges.insert(text(node, "id")?, deps);
    }

    let mut seen = HashSet::new();
    let mut stack: Vec<(&str, bool)> = members.iter().map(|id| (*id, false)).collect();
    while let Some((id, on_host)) = stack.pop() {
        if !seen.insert((id, on_host)) {
            continue;
        }
        let package = packages
            .get(id)
            .ok_or_else(|| format!("no package for `{id}`"))?;
        let on_host = on_host || has_target_kind(package, "proc-macro")?;
        let side = if on_host {
            built.host_side
        } else {
            built.target_side
        };
        if !side.contains(&package_key(package)?) {
            continue;
        }
        if let Some(deps) = edges.get(id) {
            stack.extend(deps.iter().map(|(dep, build)| (*dep, on_host || *build)));
        }
        inspect_package(package, members.contains(id), audited, findings)?;
    }
    Ok(())
}

fn has_target_kind(package: &Json, wanted: &str) -> Result<bool, String> {
    for target in list(package, "targets")? {
        for kind in list(target, "kind")? {
            if kind.as_str() == Some(wanted) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn inspect_package(
    package: &Json,
    is_ours: bool,
    audited: &HashSet<String>,
    findings: &mut BTreeSet<String>,
) -> Result<(), String> {
    let name = text(package, "name")?;
    let version = text(package, "version")?;
    let label = format!("{name} {version}");

    if C_BUILDERS.contains(&name) {
        findings.insert(format!(
            "{label}: builds C or binds to it. Remove what depends on it"
        ));
    }
    if package.get("links").and_then(Json::as_str).is_some() {
        findings.insert(format!(
            "{label}: has a `links` key, so it links native code. Remove it"
        ));
    }

    let has_build_script = has_target_kind(package, "custom-build")?;
    if has_build_script && is_ours {
        findings.insert(format!(
            "{label}: our own crates can't have a build script. Remove it"
        ));
    } else if has_build_script && !audited.contains(&label) {
        findings.insert(format!(
            "{label}: has a build script nobody has audited. Read it, then add `{label} <reason>` to {AUDIT_FILE}"
        ));
    }

    let manifest = Path::new(text(package, "manifest_path")?);
    let directory = manifest.parent().ok_or("a manifest has no parent")?;
    let mut visited = HashSet::new();
    find_foreign_files(directory, true, &label, findings, &mut visited).map_err(|e| e.to_string())
}

fn is_foreign(name: &str) -> bool {
    let extension = name.rsplit_once('.').map(|(_, extension)| extension);
    extension.is_some_and(|e| FOREIGN_EXTENSIONS.contains(&e)) || name.contains(".so.")
}

/// Follows symbolic links, so a linked file or directory can't hide native
/// code. Only the top of a package can hold Cargo's output or a git directory.
fn find_foreign_files(
    directory: &Path,
    is_top: bool,
    label: &str,
    findings: &mut BTreeSet<String>,
    visited: &mut HashSet<PathBuf>,
) -> std::io::Result<()> {
    if !visited.insert(fs::canonicalize(directory)?) {
        return Ok(());
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let Ok(kind) = fs::metadata(&path) else {
            continue; // A broken link has no content to inspect.
        };
        if kind.is_dir() {
            if !(is_top && (name == "target" || name == ".git")) {
                find_foreign_files(&path, false, label, findings, visited)?;
            }
        } else if kind.is_file() && name.to_str().is_some_and(is_foreign) {
            findings.insert(format!(
                "{label}: contains {}, which is C, assembly, or a prebuilt library",
                path.display()
            ));
        }
    }
    Ok(())
}
