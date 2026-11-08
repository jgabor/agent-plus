//! The floors that `cargo metadata` can check: no C anywhere in the tree, and
//! no build script that hasn't been audited.

use crate::Problem;
use crate::json::{self, Json};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::process::Command;

/// The hosts v0.1.0 supports. Each one resolves a different dependency graph.
const TARGETS: [&str; 3] = [
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "aarch64-apple-darwin",
];

const C_BUILDERS: [&str; 4] = ["cc", "cmake", "bindgen", "pkg-config"];

const FOREIGN_EXTENSIONS: [&str; 15] = [
    "c", "h", "cc", "cpp", "cxx", "hh", "hpp", "S", "s", "asm", "a", "o", "so", "dylib", "lib",
];

const AUDIT_FILE: &str = "harness/build-scripts.txt";

/// Returns one message for each broken rule. An empty list means the floors
/// hold.
pub fn check(root: &Path) -> Result<Vec<String>, Problem> {
    let audited = read_audit_list(root)?;
    let mut findings = BTreeSet::new();
    for target in TARGETS {
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
            findings.insert(format!(
                "`cargo metadata` failed for {target}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
            continue;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let metadata = json::parse(&text)
            .map_err(|e| Problem(format!("couldn't read `cargo metadata` output: {e}")))?;
        inspect(&metadata, &audited, &mut findings)
            .map_err(|e| Problem(format!("unexpected `cargo metadata` output: {e}")))?;
    }
    Ok(findings.into_iter().collect())
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
    let mut edges: HashMap<&str, Vec<&str>> = HashMap::new();
    for node in list(field(metadata, "resolve")?, "nodes")? {
        let mut deps = Vec::new();
        for dep in list(node, "deps")? {
            deps.push(text(dep, "pkg")?);
        }
        edges.insert(text(node, "id")?, deps);
    }

    let mut seen = HashSet::new();
    let mut stack: Vec<&str> = members.iter().copied().collect();
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        if let Some(deps) = edges.get(id) {
            stack.extend(deps.iter().copied());
        }
        let package = packages
            .get(id)
            .ok_or_else(|| format!("no package for `{id}`"))?;
        inspect_package(package, members.contains(id), audited, findings)?;
    }
    Ok(())
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

    let mut has_build_script = false;
    for target in list(package, "targets")? {
        for kind in list(target, "kind")? {
            has_build_script |= kind.as_str() == Some("custom-build");
        }
    }
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
    find_foreign_files(directory, &label, findings).map_err(|e| e.to_string())
}

fn find_foreign_files(
    directory: &Path,
    label: &str,
    findings: &mut BTreeSet<String>,
) -> std::io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            let name = entry.file_name();
            if name != "target" && name != ".git" {
                find_foreign_files(&path, label, findings)?;
            }
        } else if kind.is_file() {
            let foreign = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| FOREIGN_EXTENSIONS.contains(&e));
            if foreign {
                findings.insert(format!(
                    "{label}: contains {}, which is C, assembly, or a prebuilt library",
                    path.display()
                ));
            }
        }
    }
    Ok(())
}
