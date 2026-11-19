//! A static program for `t1`'s `net` check, run in a guest that `ap`'s init
//! set up. It prints what the guest has of its link, one `name=value` line
//! or one file each, then sends to the gateway over each family. It waits
//! for an answer over IPv4 only, and prints it.

use std::ffi::{c_int, c_ulong};
use std::fs::{self, File};
use std::io;
use std::net::{Ipv4Addr, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd};
use std::time::Duration;

use guest::net;

/// The port the check's peer answers on.
const PORT: u16 = 7;
const WAIT: Duration = Duration::from_secs(10);

const AF_INET: c_int = 2;
const SOCK_DGRAM: c_int = 2;
const SIOCGIFADDR: c_ulong = 0x8915;
const SIOCGIFNETMASK: c_ulong = 0x891b;

unsafe extern "C" {
    fn socket(domain: c_int, kind: c_int, protocol: c_int) -> c_int;
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
}

/// `struct ifreq`, with its union as bytes.
#[repr(C)]
struct InterfaceRequest {
    name: [u8; 16],
    data: [u8; 24],
}

/// The IPv4 address an `ioctl` reads for `eth0`.
fn interface_address(request: c_ulong) -> io::Result<Ipv4Addr> {
    // SAFETY: socket takes no pointers.
    let fd = unsafe { socket(AF_INET, SOCK_DGRAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the kernel just returned this descriptor, and nothing else
    // owns it.
    let socket = unsafe { File::from_raw_fd(fd) };
    let mut ifreq = InterfaceRequest {
        name: [0; 16],
        data: [0; 24],
    };
    ifreq.name[..net::INTERFACE.len()].copy_from_slice(net::INTERFACE.as_bytes());
    // SAFETY: the call reads and writes a whole `struct ifreq`, which lives
    // across it.
    if unsafe { ioctl(socket.as_raw_fd(), request, &raw mut ifreq) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // A `struct sockaddr_in`: the family, the port, then the address.
    let address: [u8; 4] = ifreq.data[4..8].try_into().unwrap();
    Ok(Ipv4Addr::from(address))
}

fn ipv4() -> String {
    match (
        interface_address(SIOCGIFADDR),
        interface_address(SIOCGIFNETMASK),
    ) {
        (Ok(address), Ok(mask)) => format!("{address}/{}", mask.to_bits().count_ones()),
        (Err(error), _) | (_, Err(error)) => error.to_string(),
    }
}

fn file(path: &str) {
    println!("== {path}");
    match fs::read_to_string(path) {
        Ok(contents) => print!("{contents}"),
        Err(error) => println!("{error}"),
    }
    println!("== end");
}

/// Sends to the gateway over IPv6, which only has to leave the guest.
fn send_ipv6() -> String {
    let sent = UdpSocket::bind("[::]:0")
        .and_then(|socket| socket.send_to(b"ping6", (net::IPV6_GATEWAY, PORT)));
    match sent {
        Ok(_) => "sent".to_owned(),
        Err(error) => error.to_string(),
    }
}

/// Sends to the gateway over IPv4, and waits for its answer.
fn exchange_ipv4() -> String {
    let answer = UdpSocket::bind("0.0.0.0:0").and_then(|socket| {
        socket.set_read_timeout(Some(WAIT))?;
        socket.send_to(b"ping", (net::IPV4_GATEWAY, PORT))?;
        let mut buffer = [0; 64];
        let (len, from) = socket.recv_from(&mut buffer)?;
        Ok(format!(
            "{} from {from}",
            String::from_utf8_lossy(&buffer[..len])
        ))
    });
    answer.unwrap_or_else(|error| error.to_string())
}

fn main() {
    println!("ipv4={}", ipv4());
    file("/proc/net/if_inet6");
    file("/proc/net/route");
    file("/proc/net/ipv6_route");
    file("/etc/resolv.conf");
    println!("ipv6_sent={}", send_ipv6());
    println!("answer={}", exchange_ipv4());
}
