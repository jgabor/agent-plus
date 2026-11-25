//! The network process: a process of its own for each run, which runs the
//! guest's network.
//!
//! The process that runs the guest can't make a socket, so it passes the
//! guest's Ethernet frames here, over a socket that `ap run` makes before
//! either process is confined. This process runs the stack from
//! [`netstack`] as the guest's gateway and resolver, and ends each of the
//! guest's TCP connections and UDP flows there. It answers the guest's DNS
//! queries itself, from the run's `NETWORK` rules and its harness's hosts.
//!
//! Every other flow is decided by [`net::proxied`]. A refused connection
//! gets a reset, and a refused datagram is dropped. An allowed one gets a
//! connection of its own to its real destination: the address the guest
//! named, the addresses that `ap run` looks up for a name from the pool,
//! or your host's loopback for [`net::YOUR_HOST`]. The guest's SYN waits
//! until that connection opens, so a destination that refuses looks refused
//! to the guest. The bytes then pass both ways unchanged.
//!
//! A TCP connection that a name decided gets the name check of
//! [`net::NameCheck`] instead. The stack takes it first, and the proxy
//! connects only once the guest's first bytes agree with the name, or the
//! guest has sent nothing for a while. Every later plain HTTP request on it
//! is checked too, and one that disagrees resets the connection.
//!
//! A TCP connection to a host with URL rules gets neither. The proxy
//! connects first, and then reads every request on it, as [`decrypt`]
//! says, decrypting TLS with the run's certificate authority.
//!
//! Before it confines itself, it makes the run's certificate authority, as
//! [`Authority`] says, and sends `ap run` the authority's certificate, in
//! PEM, as the first message on its standard output. The key stays here.
//! It reads the roots in the file that `SSL_CERT_FILE` names then too.
//! `ap run` then sends it the run's hidden secrets on that socket, as
//! [`Injection`] says, and it decrypts TLS to their bound hosts too.
//!
//! It confines itself before it reads a frame. It can then make IPv4 and
//! IPv6 sockets, unless its rules can reach nothing, but it can't start a
//! process or open a host file. So it can't read your host's resolver
//! configuration either, and asks `ap run` for each lookup instead, over a
//! socket on its standard output. It reports each host it blocks on that
//! socket too, as [`blocked`] says, for `ap run` to print when the run
//! ends, and each decision it makes, as [`decisions`] says, for `ap run` to
//! keep in the run's log. It ends when the other end of its link closes.

mod authority;
pub mod blocked;
mod decisions;
mod decrypt;
mod lookup;
mod proxy;
mod secrets;

pub use authority::{Authority, Leaf};
pub use blocked::{Reports, report};
pub use decisions::Keeper;
pub use decrypt::{Decryption, roots};
pub use lookup::{Lookups, answer_lookups};
pub use proxy::Proxy;
pub use secrets::{Hidden, Injection};

use std::collections::hash_map::RandomState;
use std::ffi::{OsString, c_int};
use std::fs::File;
use std::hash::BuildHasher;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixDatagram;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command};
use std::rc::Rc;
use std::time::{Duration, SystemTime};

use agentfile::{Destination, Instruction, Network, Sign};
use guest::net;
use netstack::{Config, Pool, Resolver, Stack};

use blocked::Reporter;

/// The `argv[0]` that `ap` starts the network process with.
pub const PROGRAM: &str = "ap-network";
const STDIN: c_int = 0;
const STDOUT: c_int = 1;
/// How long a read waits for a frame before the stack's timers run.
const RECEIVE_WAIT: Duration = Duration::from_millis(50);
/// How long a frame for the guest waits for room, before it is dropped,
/// as on a real link.
const SEND_WAIT: Duration = Duration::from_millis(10);

/// A network process that has started, the VMM's end of its link, the end
/// that its lookups come in on, for [`answer_lookups`], and the run's
/// certificate authority, in PEM.
pub struct Started {
    pub process: Child,
    pub link: OwnedFd,
    pub lookups: OwnedFd,
    pub authority: Vec<u8>,
}

/// How long `ap run` waits for the network process's authority.
const AUTHORITY_WAIT: Duration = Duration::from_secs(10);
/// The longest authority certificate, in PEM, that `ap run` takes.
const LONGEST_AUTHORITY: usize = 4096;

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
/// keeps message boundaries, for [`crate::boot::boot_linked`]. It returns
/// once the process has sent its authority, and been sent `injection`. The
/// process starts without a hidden secret's variable. It ends once every
/// copy of the link is closed.
pub fn start(program: &Path, args: &[OsString], injection: &Injection) -> io::Result<Started> {
    let (mine, theirs) = link()?;
    let (answers, asks) = link()?;
    let mut command = Command::new(program);
    for secret in &injection.secrets {
        command.env_remove(&secret.name);
    }
    let process = command
        .arg0(PROGRAM)
        .args(args)
        .stdin(theirs)
        .stdout(asks)
        .spawn()?;
    let authority = receive_authority(answers.as_fd())?;
    injection.send(answers.as_fd())?;
    Ok(Started {
        process,
        link: mine,
        lookups: answers,
        authority,
    })
}

/// The first message on `lookups`: the authority's certificate. The wait
/// ends with it, so the lookups that follow can wait as long as they need.
fn receive_authority(lookups: BorrowedFd) -> io::Result<Vec<u8>> {
    set_timeout(lookups, libc::SO_RCVTIMEO, AUTHORITY_WAIT)?;
    let mut buffer = vec![0; LONGEST_AUTHORITY + 1];
    // SAFETY: `buffer` has room for the bytes the call writes.
    let read = unsafe {
        libc::recv(
            lookups.as_raw_fd(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            0,
        )
    };
    let read = match read {
        -1 => return Err(io::Error::last_os_error()),
        read => read as usize,
    };
    set_timeout(lookups, libc::SO_RCVTIMEO, Duration::ZERO)?;
    buffer.truncate(read);
    let pem = buffer.starts_with(b"-----BEGIN CERTIFICATE-----")
        && read <= LONGEST_AUTHORITY
        && !buffer.contains(&0);
    match pem {
        true => Ok(buffer),
        false => Err(io::Error::other(
            "the network process sent no certificate authority",
        )),
    }
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

/// The network process's main: its standard input is its link, its
/// standard output is where it asks for lookups, and `args` are the ones
/// [`arguments`] made. It returns the status to exit with.
pub fn main(args: &[OsString]) -> u8 {
    let (network, harness) = match rules(args) {
        Ok(rules) => rules,
        Err(_) => {
            eprintln!("usage: ap-network <harness host>... -- <NETWORK rule>...");
            return 125;
        }
    };
    // Its standard output is the socket it reports on.
    let reporter = Rc::new(Reporter::new(STDOUT, network.clone(), harness.clone()));
    let mut resolver = match resolver_for(&network, &harness, Some(Rc::clone(&reporter))) {
        Ok(resolver) => resolver,
        Err(_) => {
            eprintln!("ap: every private IPv4 block meets an IP rule, so the resolver has no pool");
            return 1;
        }
    };
    // SAFETY: standard input and output are the sockets that `start`
    // passed, and nothing else in this process owns them.
    let (link, lookups) = unsafe {
        (
            File::from(OwnedFd::from_raw_fd(STDIN)),
            UnixDatagram::from(OwnedFd::from_raw_fd(STDOUT)),
        )
    };
    // Made before the filter, which has no call for the random source.
    let authority = match Authority::generate(SystemTime::now()) {
        Ok(authority) => authority,
        Err(error) => {
            eprintln!("ap: can't make the run's certificate authority: {error}");
            return 1;
        }
    };
    if let Err(error) = lookups.send(authority.certificate_pem().as_bytes()) {
        eprintln!("ap: can't send the run's certificate authority: {error}");
        return 1;
    }
    let injection = (lookups.set_read_timeout(Some(AUTHORITY_WAIT)))
        .and_then(|()| Injection::receive(lookups.as_fd()))
        .and_then(|injection| lookups.set_read_timeout(None).map(|()| injection));
    let injection = match injection {
        Ok(injection) => injection,
        Err(error) => {
            eprintln!("ap: can't take the run's hidden secrets: {error}");
            return 1;
        }
    };
    let extra = match std::env::var_os("SSL_CERT_FILE") {
        Some(path) if decrypts(&network, &injection) => roots(path.as_ref()),
        _ => Vec::new(),
    };
    // The key lives as long as the process serves.
    let decryption = match Decryption::new(authority, extra) {
        Ok(decryption) => decryption,
        Err(error) => {
            eprintln!("ap: can't set up decryption: {error}");
            return 1;
        }
    };
    // A report or a question that doesn't fit is lost, rather than hold
    // up the guest's network.
    if let Err(error) = lookups.set_nonblocking(true) {
        eprintln!("ap: can't set up the network process: {error}");
        return 1;
    }
    let mut stack = stack();
    // A proxy that can reach nothing asks for no lookups, so it needs no
    // socket but its link.
    let (mut proxy, confined) = match reaches_nothing(&network, &harness) {
        true => (Proxy::refusing().reporting(reporter), confine(link.as_fd())),
        false => (
            (Proxy::new(network, harness, lookups))
                .decrypting(decryption)
                .injecting(injection)
                .reporting(reporter),
            confine_connecting(link.as_fd()),
        ),
    };
    let served = confined.and_then(|()| proxy.serve(&link, &mut stack, &mut resolver));
    match served {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("ap: the network process stopped: {error}");
            1
        }
    }
}

/// Whether a network process with the `args` that [`arguments`] made asks
/// for lookups, because its rules can reach something.
pub fn asks(args: &[OsString]) -> bool {
    rules(args).is_ok_and(|(network, harness)| !reaches_nothing(&network, &harness))
}

/// Whether `network`, with `harness` as the harness's hosts, can allow no
/// connection at all.
fn reaches_nothing(network: &Network, harness: &[String]) -> bool {
    let hosts: Vec<&str> = harness.iter().map(String::as_str).collect();
    network.default == Sign::Deny
        && network.rules.iter().all(|rule| rule.sign == Sign::Deny)
        && hosts
            .iter()
            .all(|host| !::net::could_allow(network, &hosts, host))
}

/// Whether the proxy may decrypt TLS under `network`, with the hidden
/// secrets of `injection`: to a host with URL rules, a host under a `+` rule
/// with a narrower `-` rule beneath it, or a hidden secret's bound host.
fn decrypts(network: &Network, injection: &Injection) -> bool {
    !injection.secrets.is_empty()
        || (network.rules.iter()).any(|rule| match &rule.destination {
            Destination::Url { .. } => true,
            Destination::Host { name, .. } => ::net::narrowed(network, name),
            Destination::Address { .. } => false,
        })
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

/// The rules and the harness's hosts in the `args` that [`arguments`]
/// made.
fn rules(args: &[OsString]) -> Result<(Network, Vec<String>), Unready> {
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
    Ok((network, harness))
}

/// The guest's resolver, at the gateway's addresses, for the `args` that
/// [`arguments`] made. It answers a name that the rules, the default, or a
/// harness host could allow, from a pool that overlaps no IP rule and not
/// the guest's link.
pub fn resolver(args: &[OsString]) -> Result<Resolver, Unready> {
    let (network, harness) = rules(args)?;
    resolver_for(&network, &harness, None)
}

/// The resolver that [`resolver`] makes, which reports each name it
/// doesn't answer to `reporter`.
fn resolver_for(
    network: &Network,
    harness: &[String],
    reporter: Option<Rc<Reporter>>,
) -> Result<Resolver, Unready> {
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
    let network = network.clone();
    let harness = harness.to_vec();
    Ok(Resolver::new(&at, pool, move |name| {
        let harness: Vec<&str> = harness.iter().map(String::as_str).collect();
        let allows = ::net::could_allow(&network, &harness, name);
        if let (false, Some(reporter)) = (allows, &reporter) {
            reporter.report(::net::Blocked::Lookup(name.to_owned()));
        }
        allows
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
/// on `link`, which can't make a socket. It sets the link's timeouts first,
/// which it can't once confined.
pub fn confine(link: BorrowedFd) -> io::Result<()> {
    set_timeouts(link)?;
    vmm::confine::confine_network(link.as_raw_fd())
}

/// Confines this process as [`confine`] does, except that it can make IPv4
/// and IPv6 sockets, and connect out on them.
pub fn confine_connecting(link: BorrowedFd) -> io::Result<()> {
    set_timeouts(link)?;
    vmm::confine::confine_network_connecting(link.as_raw_fd())
}

/// Passes frames between `link` and `stack` until the link's other end
/// closes. `resolver` answers the guest's queries, and every other flow the
/// guest opens is refused.
pub fn serve(link: &File, stack: &mut Stack, resolver: &mut Resolver) -> io::Result<()> {
    Proxy::refusing().serve(link, stack, resolver)
}

fn set_timeouts(link: BorrowedFd) -> io::Result<()> {
    set_timeout(link, libc::SO_RCVTIMEO, RECEIVE_WAIT)?;
    set_timeout(link, libc::SO_SNDTIMEO, SEND_WAIT)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn decrypts_under(lines: &str) -> bool {
        let instructions = agentfile::parse(&format!("AGENT claude\n{lines}")).unwrap();
        let network = Network::read(&instructions).unwrap();
        decrypts(&network, &Injection::default())
    }

    #[test]
    fn the_roots_of_ssl_cert_file_are_read_where_the_proxy_decrypts() {
        assert!(decrypts_under("NETWORK -\nNETWORK +https://github.com/x\n"));
        assert!(decrypts_under(
            "NETWORK -\nNETWORK +github.com\nNETWORK -gist.github.com\n"
        ));
        assert!(!decrypts_under("NETWORK -\nNETWORK +github.com\n"));
        assert!(!decrypts_under("NETWORK +\nNETWORK -gist.github.com\n"));
    }
}
