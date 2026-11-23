//! Fetches the pinned guest kernel for tests, until `ap` can download it.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;

const PIN: &str = include_str!("../../../guest/kernel/pin.toml");

fn pinned(key: &str) -> &'static str {
    crate::pinned(PIN, key)
}

fn digest_matches(path: &Path) -> bool {
    crate::digest_matches(path, pinned("sha256"))
}

/// The release the pinned kernel reports.
pub fn release() -> &'static str {
    pinned("version")
}

/// Returns the path of the pinned `vmlinux`, downloading it if it's missing.
pub fn vmlinux() -> PathBuf {
    vmlinux_in(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/guest"))
}

/// Returns the path of the pinned `vmlinux` in `directory`, downloading it if
/// it's missing. Callers check and download it under a lock, so concurrent
/// ones, in one process or many, fetch it once and never share a part file.
pub fn vmlinux_in(directory: &Path) -> PathBuf {
    let path = directory.join("vmlinux");
    if path.exists() && digest_matches(&path) {
        return path;
    }

    fs::create_dir_all(directory).expect("the kernel's directory should be writable");
    let lock = File::create(directory.join("vmlinux.lock")).unwrap();
    lock.lock().unwrap();
    if path.exists() && digest_matches(&path) {
        return path;
    }
    // Only the lock's holder writes this, and curl replaces what a killed
    // one left.
    let partial = directory.join("vmlinux.part");
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
