//! A released `ap` is static musl. When the program break can't grow,
//! musl's allocator maps pages with no access, and makes them readable and
//! writable with `mprotect`. The gate's tests build for glibc, whose
//! allocator never does, so these checks run a probe built for musl, which
//! blocks its own break, confines itself, and allocates.

use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use t1::guest_binary;

/// Runs the probe as `process`, and checks that it lived.
fn lives_without_brk(process: &str) {
    let output = Command::new(guest_binary::build("heap-probe"))
        .arg(process)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{process}: {:?}, signal {:?}\n{}",
        output.status,
        output.status.signal(),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_confined_network_process_allocates_when_brk_fails() {
    lives_without_brk("network");
}

#[test]
fn a_confined_share_server_allocates_when_brk_fails() {
    lives_without_brk("share-server");
}
