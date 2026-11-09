//! Fetches the real pinned kernel over the internet.

use std::fs;
use std::path::Path;
use std::process::Command;

use ap::fetch::{self, Client};
use ap::store::Store;

#[test]
fn the_pinned_kernel_downloads_and_matches_its_digest() {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("fetch-store");
    let _ = fs::remove_dir_all(&root);
    let pin = fetch::kernel();

    let path = fetch::fetch(&Store::new(&root), &Client::new(), &pin).unwrap();

    // Hashed by another program, so a bug in `ap`'s check can't hide here.
    let output = Command::new("sha256sum").arg(&path).output().unwrap();
    assert!(output.status.success());
    assert!(output.stdout.starts_with(pin.sha256.as_bytes()));
    fs::remove_dir_all(&root).unwrap();
}
