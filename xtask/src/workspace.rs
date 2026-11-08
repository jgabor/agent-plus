//! Where the workspace is, and what is in it.

use crate::Problem;
use crate::json::{self, Json};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn no_cargo(error: io::Error) -> Problem {
    Problem(format!(
        "couldn't run `cargo`: {error}. Install Rust, or add cargo to PATH"
    ))
}

pub fn root() -> Result<PathBuf, Problem> {
    let output = Command::new("cargo")
        .args(["locate-project", "--workspace", "--message-format", "plain"])
        .output()
        .map_err(no_cargo)?;
    if !output.status.success() {
        return Err(Problem(format!(
            "not inside a Cargo workspace: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let manifest = String::from_utf8_lossy(&output.stdout);
    let manifest = Path::new(manifest.trim());
    let root = manifest
        .parent()
        .ok_or_else(|| Problem(format!("`{}` has no parent", manifest.display())))?;
    Ok(root.to_path_buf())
}

pub struct Package {
    pub name: String,
    pub directory: PathBuf,
}

/// The workspace's own packages, in the order Cargo reports them.
pub fn packages(root: &Path) -> Result<Vec<Package>, Problem> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(root)
        .output()
        .map_err(no_cargo)?;
    if !output.status.success() {
        return Err(Problem(format!(
            "`cargo metadata` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let metadata = json::parse(&String::from_utf8_lossy(&output.stdout))
        .map_err(|e| Problem(format!("couldn't read `cargo metadata` output: {e}")))?;
    let unexpected = || Problem("unexpected `cargo metadata` output".to_string());
    let mut packages = Vec::new();
    for package in metadata
        .get("packages")
        .and_then(Json::as_arr)
        .ok_or_else(unexpected)?
    {
        let name = package
            .get("name")
            .and_then(Json::as_str)
            .ok_or_else(unexpected)?;
        let manifest = package
            .get("manifest_path")
            .and_then(Json::as_str)
            .ok_or_else(unexpected)?;
        let directory = Path::new(manifest).parent().ok_or_else(unexpected)?;
        packages.push(Package {
            name: name.to_string(),
            directory: directory.to_path_buf(),
        });
    }
    Ok(packages)
}
