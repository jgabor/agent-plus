//! Every check that needs `/dev/kvm` lives in this package, so that T0 can
//! leave them out.

use std::path::Path;
use std::process::Command;

pub mod guest_binary;
pub mod kernel;
pub mod os;
pub mod started;

/// Whether this process has one thread. A filter that a process installs
/// confines every thread of it, so a thread of the test harness would die
/// at its next call that the filter refuses.
pub fn alone() -> bool {
    std::fs::read_dir("/proc/self/task").unwrap().count() == 1
}

/// This host's address on its route out, which isn't a loopback address.
pub fn outward_address() -> std::net::Ipv4Addr {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
    socket
        .connect("192.0.2.1:9")
        .expect("this host has a route out");
    match socket.local_addr().unwrap().ip() {
        std::net::IpAddr::V4(address) => address,
        address => panic!("an IPv4 route out, not {address}"),
    }
}

/// The value of `key` in a `pin.toml`.
fn pinned(pin: &'static str, key: &str) -> &'static str {
    pin.lines()
        .find_map(|line| {
            let (name, value) = line.split_once(" = ")?;
            (name == key).then(|| value.trim_matches('"'))
        })
        .unwrap_or_else(|| panic!("pin.toml has no {key}"))
}

/// Whether the file at `path` has the SHA-256 digest `digest`.
fn digest_matches(path: &Path, digest: &str) -> bool {
    let output = Command::new("sha256sum")
        .arg(path)
        .output()
        .expect("sha256sum should run");
    assert!(output.status.success(), "sha256sum failed on {path:?}");
    output.stdout.starts_with(digest.as_bytes())
}
