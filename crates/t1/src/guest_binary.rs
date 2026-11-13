//! Builds programs that run inside a guest, which are static musl binaries.
//! The gate's `cargo test` builds for the GNU target, so tests ask for them.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

const TARGET: &str = "x86_64-unknown-linux-musl";

/// Builds the binary `name` of this package, and returns its path.
pub fn build(name: &str) -> PathBuf {
    build_in("t1", name)
}

/// Builds `ap` itself, which boots as its own guest's init.
pub fn ap() -> PathBuf {
    build_in("ap", "ap")
}

/// Builds the fake harness, which stands in for a coding agent in the guest.
pub fn fake_harness() -> PathBuf {
    build_in("fake-harness", "fake-harness")
}

fn build_in(package: &str, name: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let target_directory = root.join("target/t1-guest");
    let status = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .current_dir(&root)
        .args(["build", "--quiet", "--package", package, "--bin", name])
        .args(["--target", TARGET, "--target-dir"])
        .arg(&target_directory)
        .status()
        .expect("cargo should run");
    assert!(status.success(), "cargo couldn't build {name} for {TARGET}");
    target_directory.join(TARGET).join("debug").join(name)
}
