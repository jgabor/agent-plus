//! Builds programs that run inside a guest, which are static musl binaries.
//! The gate's `cargo test` builds for the GNU target, so tests ask for them.
//! Each program builds once in a process, which every test of it shares.

use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, PoisonError};

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
    once(package, name, || cargo_build(package, name))
}

/// The path that `build` returned the first time this process asked for
/// `name` of `package`. Another test that asks meanwhile waits for it.
fn once(package: &str, name: &str, build: impl FnOnce() -> PathBuf) -> PathBuf {
    static BUILT: Mutex<BTreeMap<(String, String), PathBuf>> = Mutex::new(BTreeMap::new());
    // A build that panicked put nothing in the map, so the next ask builds.
    let mut built = BUILT.lock().unwrap_or_else(PoisonError::into_inner);
    let key = (package.to_owned(), name.to_owned());
    built.entry(key).or_insert_with(build).clone()
}

fn cargo_build(package: &str, name: &str) -> PathBuf {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_program_builds_once_in_a_process() {
        let builds = Mutex::new(0);
        let build = || {
            *builds.lock().unwrap() += 1;
            PathBuf::from("built")
        };
        for _ in 0..2 {
            assert_eq!(once("a-package", "a-program", build), Path::new("built"));
            once("a-package", "another-program", build);
        }
        assert_eq!(*builds.lock().unwrap(), 2, "a second ask built it again");
    }
}
