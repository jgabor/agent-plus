//! The network process: a process of its own for each run, which runs the
//! guest's network.
//!
//! The process that runs the guest can't make a socket, so it passes the
//! guest's Ethernet frames here, over a socket that `ap run` makes before
//! either process is confined. This process runs the stack from
//! [`netstack`] as the guest's gateway and resolver, and ends each of the
//! guest's TCP connections and UDP flows there. It answers the guest's DNS
//! queries itself, from the run's `NETWORK` rules and its harness's hosts,
//! and refuses every other flow: a connection gets a reset, and a datagram
//! is dropped. It confines itself before it reads a frame, so it can't start
//! a process, open a host file, or make a socket. It ends when the other end
//! of its socket closes.

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

use agentfile::{Destination, Instruction, Network};
use guest::net;
use netstack::{Config, Pool, Resolver, Stack};
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

/// Divides the harness's hosts from the `NETWORK` rules in the network
/// process's arguments.
const RULES: &str = "--";

/// The network process's arguments for a run of an agent whose harness has
/// `harness` as its hosts, and whose `instructions` have its `NETWORK`
/// lines.
pub fn arguments(harness: &[&str], instructions: &[Instruction]) -> Vec<OsString> {
    let rules = instructions
        .iter()
        .filter(|instruction| instruction.name == "NETWORK")
        .flat_map(|instruction| &instruction.arguments);
    (harness.iter().map(OsString::from))
        .chain([OsString::from(RULES)])
        .chain(rules.map(OsString::from))
        .collect()
}

/// Starts `program`, a build of `ap`, as a run's network process, with the
/// `args` that [`arguments`] made. The link it returns is a socket that
/// keeps message boundaries, for [`crate::boot::boot_linked`]. The process
/// ends once every copy of the link is closed.
pub fn start(program: &Path, args: &[OsString]) -> io::Result<Started> {
    let (mine, theirs) = link()?;
    let process = Command::new(program)
        .arg0(PROGRAM)
        .args(args)
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

/// The network process's main: its standard input is its link, and `args`
/// are the ones [`arguments`] made. It returns the status to exit with.
pub fn main(args: &[OsString]) -> u8 {
    let mut resolver = match resolver(args) {
        Ok(resolver) => resolver,
        Err(Unready::Usage) => {
            eprintln!("usage: ap-network <harness host>... -- <NETWORK rule>...");
            return 125;
        }
        Err(Unready::NoPool) => {
            eprintln!("ap: every private IPv4 block meets an IP rule, so the resolver has no pool");
            return 1;
        }
    };
    // SAFETY: standard input is the link that `start` passed, and nothing
    // else in this process owns it.
    let link = File::from(unsafe { OwnedFd::from_raw_fd(STDIN) });
    let mut stack = stack();
    let served = confine(link.as_fd()).and_then(|()| serve(&link, &mut stack, &mut resolver));
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

/// The guest's resolver, at the gateway's addresses, for the `args` that
/// [`arguments`] made. It answers a name that the rules, the default, or a
/// harness host could allow, from a pool that overlaps no IP rule and not
/// the guest's link.
pub fn resolver(args: &[OsString]) -> Result<Resolver, Unready> {
    let args: Vec<&str> = (args.iter().map(|arg| arg.to_str()))
        .collect::<Option<_>>()
        .ok_or(Unready::Usage)?;
    let at = (args.iter().position(|arg| *arg == RULES)).ok_or(Unready::Usage)?;
    let harness: Vec<String> = args[..at].iter().map(|host| host.to_string()).collect();
    let lines: Vec<Instruction> = (1..)
        .zip(&args[at + 1..])
        .map(|(line, rule)| Instruction {
            line,
            name: "NETWORK".to_owned(),
            arguments: vec![rule.to_string()],
            text: None,
        })
        .collect();
    let network = Network::read(&lines).map_err(|_| Unready::Usage)?;
    let blocks: Vec<_> = network
        .rules
        .iter()
        .filter_map(|rule| match rule.destination {
            Destination::Address {
                network, prefix, ..
            } => Some((network, prefix)),
            _ => None,
        })
        .collect();
    let pool =
        Pool::choose((net::IPV4_GATEWAY, net::IPV4_PREFIX), &blocks).ok_or(Unready::NoPool)?;
    let at = [net::IPV4_GATEWAY.into(), net::IPV6_GATEWAY.into()];
    Ok(Resolver::new(&at, pool, move |name| {
        let harness: Vec<&str> = harness.iter().map(String::as_str).collect();
        ::net::could_allow(&network, &harness, name)
    }))
}

/// Why there is no resolver.
#[derive(Debug)]
pub enum Unready {
    /// The arguments don't read.
    Usage,
    /// Every private block that the pool could take meets an IP rule.
    NoPool,
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
/// closes. `resolver` answers the guest's queries, and every other flow the
/// guest opens is refused.
pub fn serve(link: &File, stack: &mut Stack, resolver: &mut Resolver) -> io::Result<()> {
    // One byte more than a frame, so that a longer message is seen.
    let mut buffer = vec![0; MAX_FRAME + 1];
    loop {
        match (&*link).read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(len) if len > MAX_FRAME => {}
            Ok(len) => stack.receive(&buffer[..len], Instant::now(), |flow| resolver.takes(flow)),
            Err(error) if waited(&error) => {}
            Err(error) => return Err(error),
        }
        resolver.serve(stack);
        stack.poll(Instant::now());
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
