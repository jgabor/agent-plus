//! A static program for T1, kept in an agent's top layer: it prints what it
//! sees of the guest's root, one `name=value` line each, and leaves a file
//! behind for the next boot to look for. It also does what a harness does to
//! the root: it replaces a directory from a lower layer, runs a `#!` script,
//! and opens an inotify instance, as a file watcher does. Last, it brings up
//! loopback and connects to itself over IPv4 and IPv6, and says whether the
//! kernel has the virtio-net driver and netfilter.

use std::env;
use std::ffi::{c_int, c_short, c_ulong};
use std::fs::{self, File};
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd};
use std::process::Command;

const LEFT_BEHIND: &str = "/left-behind";
const REPLACED: &str = "/srv/replaced";
const SCRIPT: &str = "/bin/script";

const AF_INET: c_int = 2;
const SOCK_DGRAM: c_int = 2;
const SIOCGIFFLAGS: c_ulong = 0x8913;
const SIOCSIFFLAGS: c_ulong = 0x8914;
const IFF_UP: c_short = 0x1;

unsafe extern "C" {
    fn inotify_init1(flags: c_int) -> c_int;
    fn socket(domain: c_int, kind: c_int, protocol: c_int) -> c_int;
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
}

/// `struct ifreq`, with only the flags of its union.
#[repr(C)]
struct InterfaceRequest {
    name: [u8; 16],
    flags: c_short,
    _rest: [u8; 22],
}

fn read(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| format!("unreadable: {error}"))
}

/// `rm -r dir && mkdir dir`, on a directory from a lower layer.
fn remake() -> String {
    match fs::remove_dir_all(REPLACED).and_then(|()| fs::create_dir(REPLACED)) {
        Ok(()) => "true".to_owned(),
        Err(error) => error.to_string(),
    }
}

/// Runs the script, whose `#!` line runs this program again.
fn script() -> String {
    match Command::new(SCRIPT).output() {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout).into(),
        Ok(output) => format!("failed: {}", output.status),
        Err(error) => error.to_string(),
    }
}

fn inotify() -> String {
    // SAFETY: inotify_init1 takes no pointers.
    let fd = unsafe { inotify_init1(0) };
    if fd < 0 {
        return std::io::Error::last_os_error().to_string();
    }
    // SAFETY: the kernel just returned this descriptor, and nothing else owns
    // it.
    drop(unsafe { File::from_raw_fd(fd) });
    "true".to_owned()
}

fn last_error() -> String {
    io::Error::last_os_error().to_string()
}

/// `ip link set lo up`. The kernel gives loopback 127.0.0.1 and ::1 as it
/// comes up.
fn loopback() -> String {
    // SAFETY: socket takes no pointers.
    let fd = unsafe { socket(AF_INET, SOCK_DGRAM, 0) };
    if fd < 0 {
        return last_error();
    }
    // SAFETY: the kernel just returned this descriptor, and nothing else owns
    // it.
    let socket = unsafe { File::from_raw_fd(fd) };
    let mut request = InterfaceRequest {
        name: [0; 16],
        flags: 0,
        _rest: [0; 22],
    };
    request.name[..2].copy_from_slice(b"lo");
    // SAFETY: both calls read and write a whole `struct ifreq`, which lives
    // across them.
    unsafe {
        if ioctl(socket.as_raw_fd(), SIOCGIFFLAGS, &raw mut request) < 0 {
            return last_error();
        }
        request.flags |= IFF_UP;
        if ioctl(socket.as_raw_fd(), SIOCSIFFLAGS, &raw mut request) < 0 {
            return last_error();
        }
    }
    "true".to_owned()
}

/// Listens on `address`, connects to it, and accepts the connection.
fn connect(address: SocketAddr) -> String {
    let listener = match TcpListener::bind(address) {
        Ok(listener) => listener,
        Err(error) => return error.to_string(),
    };
    let result = listener
        .local_addr()
        .and_then(TcpStream::connect)
        .and_then(|_stream| listener.accept());
    match result {
        Ok(_) => "true".to_owned(),
        Err(error) => error.to_string(),
    }
}

fn main() {
    // The kernel runs a script's interpreter with the script's path after it.
    if env::args().nth(1).as_deref() == Some(SCRIPT) {
        print!("ran");
        return;
    }
    let root = read("/proc/mounts")
        .lines()
        .find(|line| line.split(' ').nth(1) == Some("/"))
        .unwrap_or_default()
        .to_owned();
    let exe = fs::read_link("/proc/self/exe")
        .map(|path| path.display().to_string())
        .unwrap_or_else(|error| format!("unreadable: {error}"));
    let found = fs::exists(LEFT_BEHIND).unwrap_or(false);
    let wrote = fs::write(LEFT_BEHIND, "from an earlier boot").is_ok();
    println!("greeting={}", read("/etc/greeting"));
    println!("kept={}", read("/etc/kept"));
    println!("root={root}");
    println!("exe={exe}");
    println!("found={found}");
    println!("wrote={wrote}");
    println!("remade={}", remake());
    println!("script={}", script());
    println!("inotify={}", inotify());
    println!("loopback={}", loopback());
    println!("ipv4={}", connect((Ipv4Addr::LOCALHOST, 0).into()));
    println!("ipv6={}", connect((Ipv6Addr::LOCALHOST, 0).into()));
    let driver = "/sys/bus/virtio/drivers/virtio_net";
    println!("virtio_net={}", fs::exists(driver).unwrap_or(false));
    // The kernel makes this directory whenever it has netfilter, and /proc/net
    // whenever it has networking at all.
    println!("proc_net={}", fs::exists("/proc/net/dev").unwrap_or(false));
    let netfilter = "/proc/net/netfilter";
    println!("netfilter={}", fs::exists(netfilter).unwrap_or(false));
}
