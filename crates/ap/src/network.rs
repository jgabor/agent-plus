//! The network process: a process of its own for each run, which runs the
//! guest's network.
//!
//! The process that runs the guest can't make a socket, so it passes the
//! guest's Ethernet frames here, over a socket that `ap run` makes before
//! either process is confined. This process runs the stack from
//! [`netstack`] as the guest's gateway, and ends each of the guest's TCP
//! connections and UDP flows there. It refuses every one: a connection gets
//! a reset, and a datagram is dropped. It confines itself before it reads a
//! frame, so it can't start a process, open a host file, or make a socket.
//! It ends when the other end of its socket closes.

use std::collections::hash_map::RandomState;
use std::ffi::{OsString, c_int};
use std::fs::File;
use std::hash::BuildHasher;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use guest::net;
use netstack::{Config, Stack};
use vmm::net::MAX_FRAME;

/// The `argv[0]` that `ap` starts the network process with.
pub const PROGRAM: &str = "ap-network";
const STDIN: c_int = 0;
/// How long a read waits for a frame before the stack's timers run.
const RECEIVE_WAIT: Duration = Duration::from_millis(50);
/// How long a frame for the guest waits for room, before it is dropped,
/// as on a real link.
const SEND_WAIT: Duration = Duration::from_millis(10);

/// A network process that has started, and the VMM's end of its link.
pub struct Started {
    pub process: Child,
    pub link: OwnedFd,
}

/// Starts `program`, a build of `ap`, as a run's network process. The link
/// it returns is a socket that keeps message boundaries, for
/// [`crate::boot::boot_linked`]. The process ends once every copy of the
/// link is closed.
pub fn start(program: &Path) -> io::Result<Started> {
    let (mine, theirs) = link()?;
    let process = Command::new(program)
        .arg0(PROGRAM)
        .stdin(theirs)
        .stdout(Stdio::null())
        .spawn()?;
    Ok(Started {
        process,
        link: mine,
    })
}

/// A pair of connected sockets that keep message boundaries, and that a
/// program this process starts doesn't inherit.
fn link() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: `fds` has room for the two descriptors the call writes.
    let made = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    };
    if made != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both descriptors are new, and nothing else owns them.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// The network process's main: its standard input is its link. It takes no
/// arguments, and returns the status to exit with.
pub fn main(args: &[OsString]) -> u8 {
    if !args.is_empty() {
        eprintln!("usage: ap-network");
        return 125;
    }
    // SAFETY: standard input is the link that `start` passed, and nothing
    // else in this process owns it.
    let link = File::from(unsafe { OwnedFd::from_raw_fd(STDIN) });
    let mut stack = stack();
    let served = confine(link.as_fd()).and_then(|()| serve(&link, &mut stack));
    match served {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("ap: the network process stopped: {error}");
            1
        }
    }
}

/// The guest's gateway, at the addresses the guest's init gives it.
pub fn stack() -> Stack {
    Stack::new(Config {
        mac: net::GATEWAY_MAC,
        ipv4: (net::IPV4_GATEWAY, net::IPV4_PREFIX),
        ipv6: (net::IPV6_GATEWAY, net::IPV6_PREFIX),
        // Drawn from the system's random source, before the filter.
        seed: RandomState::new().hash_one(0),
    })
}

/// Confines this process for the rest of its life, as the network process
/// on `link`. It sets the link's timeouts first, which it can't once
/// confined.
pub fn confine(link: BorrowedFd) -> io::Result<()> {
    set_timeout(link, libc::SO_RCVTIMEO, RECEIVE_WAIT)?;
    set_timeout(link, libc::SO_SNDTIMEO, SEND_WAIT)?;
    vmm::confine::confine_network(link.as_raw_fd())
}

/// Passes frames between `link` and `stack` until the link's other end
/// closes, and refuses every flow the guest opens.
pub fn serve(link: &File, stack: &mut Stack) -> io::Result<()> {
    // One byte more than a frame, so that a longer message is seen.
    let mut buffer = vec![0; MAX_FRAME + 1];
    loop {
        match (&*link).read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(len) if len > MAX_FRAME => {}
            Ok(len) => stack.receive(&buffer[..len], Instant::now(), |_| false),
            Err(error) if waited(&error) => stack.poll(Instant::now()),
            Err(error) => return Err(error),
        }
        while let Some(frame) = stack.transmit() {
            match (&*link).write(&frame) {
                Ok(_) => {}
                Err(error) if waited(&error) => {}
                Err(error) if hung_up(&error) => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    }
}

fn set_timeout(socket: BorrowedFd, option: c_int, wait: Duration) -> io::Result<()> {
    // A `struct timeval` on x86_64, with glibc and with musl.
    let timeout = [wait.as_secs() as i64, i64::from(wait.subsec_micros())];
    // SAFETY: `timeout` is a `struct timeval`, which outlives the call.
    let set = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            option,
            (&raw const timeout).cast(),
            size_of_val(&timeout) as libc::socklen_t,
        )
    };
    match set {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

/// Whether a socket call only ran out of time, or was interrupted.
fn waited(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}

/// Whether the link's other end has closed.
fn hung_up(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
    )
}
