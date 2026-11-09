//! Fetches the pinned guest kernel for tests, until `ap` can download it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{self, Command};

const PIN: &str = include_str!("../../../guest/kernel/pin.toml");

/// The value of `key` in `pin.toml`.
fn pinned(key: &str) -> &'static str {
    PIN.lines()
        .find_map(|line| {
            let (name, value) = line.split_once(" = ")?;
            (name == key).then(|| value.trim_matches('"'))
        })
        .unwrap_or_else(|| panic!("pin.toml has no {key}"))
}

fn digest_matches(path: &Path) -> bool {
    let output = Command::new("sha256sum")
        .arg(path)
        .output()
        .expect("sha256sum should run");
    assert!(output.status.success(), "sha256sum failed on {path:?}");
    output.stdout.starts_with(pinned("sha256").as_bytes())
}

/// Returns the path of the pinned `vmlinux`, downloading it if it's missing.
pub fn vmlinux() -> PathBuf {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/guest");
    let path = directory.join("vmlinux");
    if path.exists() && digest_matches(&path) {
        return path;
    }

    fs::create_dir_all(&directory).expect("target/guest should be writable");
    // One name per process, so that concurrent test binaries can't mix.
    let partial = directory.join(format!("vmlinux.{}.part", process::id()));
    let status = Command::new("curl")
        .args(["--fail", "--silent", "--show-error", "--location"])
        .args(["--proto", "=https", "--output"])
        .arg(&partial)
        .arg(pinned("url"))
        .status()
        .expect("curl should run");
    assert!(status.success(), "curl couldn't fetch the kernel");
    assert!(
        digest_matches(&partial),
        "the downloaded kernel doesn't match pin.toml"
    );
    fs::rename(&partial, &path).expect("the kernel should move into place");
    path
}
