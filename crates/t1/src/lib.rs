//! Every check that needs `/dev/kvm` lives in this package, so that T0 can
//! leave them out.

use std::io::{self, ErrorKind};
use std::net::{TcpListener, UdpSocket};
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

/// A TCP listener on `127.0.0.1` and on `::1`, and a UDP socket on
/// `127.0.0.1`, all on one port.
pub fn one_port() -> (TcpListener, TcpListener, UdpSocket) {
    one_port_after(|| TcpListener::bind("127.0.0.1:0"))
}

/// Binds the rest on the port of the first TCP listener that `first` binds.
/// The kernel promised that port to the first listener alone, so when
/// another socket holds it for one of the rest, this tries another. It keeps
/// each port it skips until it returns, so that it never tries one twice.
fn one_port_after(
    mut first: impl FnMut() -> io::Result<TcpListener>,
) -> (TcpListener, TcpListener, UdpSocket) {
    let mut skipped = Vec::new();
    for _ in 0..100 {
        let tcp4 = first().unwrap();
        let port = tcp4.local_addr().unwrap().port();
        let rest = TcpListener::bind(("::1", port))
            .and_then(|tcp6| Ok((tcp6, UdpSocket::bind(("127.0.0.1", port))?)));
        match rest {
            Ok((tcp6, udp4)) => return (tcp4, tcp6, udp4),
            Err(error) if error.kind() == ErrorKind::AddrInUse => skipped.push(tcp4),
            Err(error) => panic!("binding port {port}: {error}"),
        }
    }
    panic!("another socket held each of {} ports", skipped.len());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_port_skips_a_port_another_socket_holds() {
        // The first listener's port, which this test holds for UDP too.
        let (first, _taken) = loop {
            let first = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = first.local_addr().unwrap().port();
            if let Ok(taken) = UdpSocket::bind(("127.0.0.1", port)) {
                break (first, taken);
            }
        };
        let port = first.local_addr().unwrap().port();
        let mut first = Some(first);
        let (tcp4, tcp6, udp4) = one_port_after(|| match first.take() {
            Some(first) => Ok(first),
            None => TcpListener::bind("127.0.0.1:0"),
        });
        let chosen = tcp4.local_addr().unwrap().port();
        assert_ne!(chosen, port);
        assert_eq!(tcp6.local_addr().unwrap().port(), chosen);
        assert_eq!(udp4.local_addr().unwrap().port(), chosen);
    }
}
