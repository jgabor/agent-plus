//! The network process's loop: it passes frames between the link and the
//! stack, answers the guest's queries, and opens a connection of its own
//! for each flow the rules allow, on one thread.

use std::fs::File;
use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixDatagram;
use std::rc::Rc;
use std::time::{Duration, Instant};

use agentfile::Network;
use net::{Blocked, Connection, YOUR_HOST, proxied};
use netstack::{Flow, Pipe, Resolver, Stack, Verdict};
use vmm::net::MAX_FRAME;

use super::RECEIVE_WAIT;
use super::blocked::Reporter;
use super::lookup::Asker;

/// How long a connection waits for its lookup, and then for each address.
const LOOKUP_WAIT: Duration = Duration::from_secs(15);
const CONNECT_WAIT: Duration = Duration::from_secs(15);
/// How long an open connection waits for the guest to finish its handshake.
const ACCEPT_WAIT: Duration = Duration::from_secs(30);
/// How long a UDP flow lasts with no datagram either way.
const UDP_IDLE: Duration = Duration::from_secs(60);
/// The datagrams a UDP flow keeps while it waits for its lookup.
const UDP_WAITING: usize = 16;
/// The frames read from the link before the rest of the loop runs.
const FRAMES_AT_ONCE: usize = 64;

/// What the guest connects to, before the rules decide.
enum Target {
    /// A name from the pool, which `ap run` looks up.
    Name(String),
    /// Addresses to try in order, and the name they are attributed to.
    Addresses(Option<String>, Vec<IpAddr>),
}

/// A TCP connection the stack holds at its SYN.
struct Opening {
    flow: Flow,
    name: Option<String>,
    since: Instant,
    step: Step,
}

enum Step {
    /// Waits for the answer with this id.
    Asking(u32),
    /// Waits for `socket` to connect, with `rest` to try after it.
    Connecting {
        socket: OwnedFd,
        rest: Vec<IpAddr>,
        since: Instant,
    },
}

/// A connection open to its destination, which waits for the stack to
/// accept the guest's end.
struct Waiting {
    flow: Flow,
    upstream: TcpStream,
    since: Instant,
}

struct UdpFlow {
    guest: SocketAddr,
    destination: SocketAddr,
    name: Option<String>,
    used: Instant,
    state: UdpState,
}

enum UdpState {
    Asking(u32, Vec<Vec<u8>>),
    Open(UdpSocket),
    /// Nothing it may reach answered, so its datagrams are dropped.
    Failed,
}

pub struct Proxy {
    /// The rules, the harness's hosts, and where lookups go, or `None` when
    /// every flow is refused.
    policy: Option<(Network, Vec<String>, Asker)>,
    opening: Vec<Opening>,
    waiting: Vec<Waiting>,
    pipes: Vec<Pipe>,
    flows: Vec<UdpFlow>,
    /// Where it reports what it blocks, if anywhere.
    reporter: Option<Rc<Reporter>>,
}

impl Proxy {
    /// A proxy that decides each flow by `network`, with `harness` as the
    /// harness's hosts, and asks for lookups on `lookups`.
    pub fn new(network: Network, harness: Vec<String>, lookups: UnixDatagram) -> Proxy {
        Proxy {
            policy: Some((network, harness, Asker::new(lookups))),
            ..Proxy::refusing()
        }
    }

    /// A proxy that refuses every flow but the resolver's.
    pub fn refusing() -> Proxy {
        Proxy {
            policy: None,
            opening: Vec::new(),
            waiting: Vec::new(),
            pipes: Vec::new(),
            flows: Vec::new(),
            reporter: None,
        }
    }

    /// This proxy, reporting each host it blocks to `reporter`.
    pub(super) fn reporting(self, reporter: Rc<Reporter>) -> Proxy {
        Proxy {
            reporter: Some(reporter),
            ..self
        }
    }

    /// Passes frames between `link` and `stack` until the link's other end
    /// closes.
    pub fn serve(
        &mut self,
        link: &File,
        stack: &mut Stack,
        resolver: &mut Resolver,
    ) -> io::Result<()> {
        // One byte more than a frame, so that a longer message is seen.
        let mut buffer = vec![0; MAX_FRAME + 1];
        loop {
            let now = Instant::now();
            let wait = stack.delay(now).unwrap_or(RECEIVE_WAIT).min(RECEIVE_WAIT);
            let ready = self.wait(link.as_raw_fd(), wait)?;
            let now = Instant::now();
            for _ in 0..FRAMES_AT_ONCE {
                match recv(link.as_raw_fd(), &mut buffer)? {
                    None => break,
                    Some(0) => return Ok(()),
                    Some(len) if len > MAX_FRAME => {}
                    Some(len) => {
                        stack.receive_holding(&buffer[..len], now, |flow| {
                            self.decide(flow, resolver, now)
                        });
                    }
                }
            }
            self.answers(stack, now)?;
            self.connects(stack, &ready, now);
            self.accept(stack, now);
            self.datagrams(stack, resolver, now);
            self.pipes.retain_mut(|pipe| pipe.pump(stack));
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

    /// Waits up to `wait` for the link, a lookup, or an upstream socket, and
    /// returns the connecting sockets that are ready.
    fn wait(&self, link: i32, wait: Duration) -> io::Result<Vec<i32>> {
        let event = |fd: i32, events: i16| libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        let mut fds = vec![event(link, libc::POLLIN)];
        if let Some((_, _, asker)) = &self.policy {
            fds.push(event(asker.fd(), libc::POLLIN));
        }
        let connecting = fds.len();
        for opening in &self.opening {
            if let Step::Connecting { socket, .. } = &opening.step {
                fds.push(event(socket.as_raw_fd(), libc::POLLOUT));
            }
        }
        for pipe in &self.pipes {
            let mut events = 0;
            if pipe.wants_to_read() {
                events |= libc::POLLIN;
            }
            if pipe.wants_to_write() {
                events |= libc::POLLOUT;
            }
            fds.push(event(pipe.upstream().as_raw_fd(), events));
        }
        for flow in &self.flows {
            if let UdpState::Open(socket) = &flow.state {
                fds.push(event(socket.as_raw_fd(), libc::POLLIN));
            }
        }
        let millis = i32::try_from(wait.as_millis()).unwrap_or(i32::MAX);
        // SAFETY: `fds` holds `fds.len()` entries, which outlive the call.
        let polled = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, millis) };
        if polled < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        Ok(fds[connecting..]
            .iter()
            .filter(|fd| fd.revents != 0)
            .map(|fd| fd.fd)
            .collect())
    }

    /// What the stack makes of a flow the guest opens.
    fn decide(&mut self, flow: &Flow, resolver: &Resolver, now: Instant) -> Verdict {
        if resolver.takes(flow) {
            return Verdict::Take;
        }
        let (guest, destination, tcp) = match *flow {
            Flow::Tcp {
                source,
                destination,
            } => (source, destination, true),
            Flow::Udp {
                source,
                destination,
            } => (source, destination, false),
        };
        let reporter = self.reporter.as_deref();
        let Some((network, harness, asker)) = &mut self.policy else {
            // A connection to the pool's address is to no host, since the
            // resolver answers no name.
            let pooled = matches!(destination.ip(), IpAddr::V4(v4) if resolver.owns(v4));
            if !pooled {
                report(reporter, None, destination.ip(), destination.port());
            }
            return Verdict::Refuse;
        };
        if !tcp {
            let known = self
                .flows
                .iter()
                .find(|known| known.guest == guest && known.destination == destination);
            if let Some(known) = known {
                return match known.state {
                    UdpState::Failed => Verdict::Refuse,
                    _ => Verdict::Take,
                };
            }
        }
        let target = match destination.ip() {
            IpAddr::V4(v4) if resolver.owns(v4) => match resolver.name_of(v4) {
                Some(name) if name == YOUR_HOST => Target::Addresses(
                    Some(name.to_owned()),
                    vec![Ipv4Addr::LOCALHOST.into(), Ipv6Addr::LOCALHOST.into()],
                ),
                Some(name) => Target::Name(name.to_owned()),
                // An address the resolver never handed out.
                None => return Verdict::Refuse,
            },
            address => Target::Addresses(None, vec![address]),
        };
        let hosts: Vec<&str> = harness.iter().map(String::as_str).collect();
        let (name, step) = match target {
            Target::Name(name) => {
                if !net::could_allow(network, &hosts, &name) {
                    report(reporter, Some(&name), destination.ip(), destination.port());
                    return Verdict::Refuse;
                }
                match asker.ask(&name) {
                    Ok(id) => (Some(name), Ok(id)),
                    Err(_) => return Verdict::Refuse,
                }
            }
            Target::Addresses(name, addresses) => {
                let allowed = allowed(
                    network,
                    &hosts,
                    name.as_deref(),
                    destination.port(),
                    &addresses,
                );
                if allowed.is_empty() {
                    report(reporter, name.as_deref(), addresses[0], destination.port());
                    return Verdict::Refuse;
                }
                if let Some(reporter) = reporter {
                    reporter.decided(true, name.as_deref(), allowed[0], destination.port());
                }
                (name, Err(allowed))
            }
        };
        let port = destination.port();
        if tcp {
            let step = match step {
                Ok(id) => Step::Asking(id),
                Err(addresses) => match connecting(addresses, port, now) {
                    Some(step) => step,
                    None => return Verdict::Refuse,
                },
            };
            self.opening.push(Opening {
                flow: *flow,
                name,
                since: now,
                step,
            });
            return Verdict::Hold;
        }
        let state = match step {
            Ok(id) => UdpState::Asking(id, Vec::new()),
            Err(addresses) => match open_udp(&addresses, port) {
                Some(socket) => UdpState::Open(socket),
                None => return Verdict::Refuse,
            },
        };
        self.flows.push(UdpFlow {
            guest,
            destination,
            name,
            used: now,
            state,
        });
        Verdict::Take
    }

    /// Takes the lookups that `ap run` answered.
    fn answers(&mut self, stack: &mut Stack, now: Instant) -> io::Result<()> {
        let Some((network, harness, asker)) = &mut self.policy else {
            return Ok(());
        };
        let hosts: Vec<&str> = harness.iter().map(String::as_str).collect();
        while let Some((id, addresses)) = asker.answer()? {
            if let Some(at) = (self.opening.iter())
                .position(|opening| matches!(opening.step, Step::Asking(asked) if asked == id))
            {
                let opening = &mut self.opening[at];
                let port = flow_destination(&opening.flow).port();
                let allowed = allowed(network, &hosts, opening.name.as_deref(), port, &addresses);
                answered(
                    self.reporter.as_deref(),
                    opening.name.as_deref(),
                    port,
                    &addresses,
                    &allowed,
                );
                match connecting(allowed, port, now) {
                    Some(step) => opening.step = step,
                    None => {
                        let opening = self.opening.remove(at);
                        stack.release(opening.flow, false, now);
                    }
                }
                continue;
            }
            let flow = (self.flows.iter_mut())
                .find(|flow| matches!(flow.state, UdpState::Asking(asked, _) if asked == id));
            if let Some(flow) = flow {
                let port = flow.destination.port();
                let allowed = allowed(network, &hosts, flow.name.as_deref(), port, &addresses);
                answered(
                    self.reporter.as_deref(),
                    flow.name.as_deref(),
                    port,
                    &addresses,
                    &allowed,
                );
                let waiting = match std::mem::replace(&mut flow.state, UdpState::Failed) {
                    UdpState::Asking(_, waiting) => waiting,
                    _ => Vec::new(),
                };
                if let Some(socket) = open_udp(&allowed, port) {
                    for datagram in waiting {
                        let _ = socket.send(&datagram);
                    }
                    flow.state = UdpState::Open(socket);
                }
            }
        }
        // Lookups that never came back.
        self.opening.retain(|opening| {
            let late = matches!(opening.step, Step::Asking(_))
                && now.duration_since(opening.since) > LOOKUP_WAIT;
            if late {
                stack.release(opening.flow, false, now);
            }
            !late
        });
        Ok(())
    }

    /// Moves on each connection whose socket is `ready`, or that has waited
    /// too long.
    fn connects(&mut self, stack: &mut Stack, ready: &[i32], now: Instant) {
        let mut at = 0;
        while at < self.opening.len() {
            let opening = &mut self.opening[at];
            let Step::Connecting {
                socket,
                rest,
                since,
            } = &mut opening.step
            else {
                at += 1;
                continue;
            };
            let fd = socket.as_raw_fd();
            let done = ready.contains(&fd);
            if !done && now.duration_since(*since) < CONNECT_WAIT {
                at += 1;
                continue;
            }
            let connected = done.then(|| socket_error(fd)).and_then(Result::ok) == Some(0);
            if connected {
                let opening = self.opening.remove(at);
                let Step::Connecting { socket, .. } = opening.step else {
                    unreachable!()
                };
                stack.release(opening.flow, true, now);
                self.waiting.push(Waiting {
                    flow: opening.flow,
                    upstream: TcpStream::from(socket),
                    since: now,
                });
                continue;
            }
            let port = flow_destination(&opening.flow).port();
            match connecting(std::mem::take(rest), port, now) {
                Some(step) => {
                    opening.step = step;
                    at += 1;
                }
                None => {
                    let opening = self.opening.remove(at);
                    stack.release(opening.flow, false, now);
                }
            }
        }
    }

    /// Pairs each connection the stack accepted with its open upstream.
    fn accept(&mut self, stack: &mut Stack, now: Instant) {
        while let Some(connection) = stack.accept() {
            let paired = self.waiting.iter().position(|waiting| {
                waiting.flow
                    == Flow::Tcp {
                        source: connection.source,
                        destination: connection.destination,
                    }
            });
            match paired {
                Some(at) => {
                    let waiting = self.waiting.remove(at);
                    self.pipes.push(Pipe::new(connection.id, waiting.upstream));
                }
                None => {
                    stack.abort(connection.id);
                    stack.forget(connection.id);
                }
            }
        }
        // The guest never finished its handshake.
        self.waiting.retain(|waiting| {
            let late = now.duration_since(waiting.since) >= ACCEPT_WAIT;
            if late {
                stack.abandon(waiting.flow);
            }
            !late
        });
    }

    /// Answers the guest's queries, and passes its other datagrams, and the
    /// answers to them.
    fn datagrams(&mut self, stack: &mut Stack, resolver: &mut Resolver, now: Instant) {
        while let Some(datagram) = stack.recv_datagram() {
            if resolver.serves(datagram.destination) {
                resolver.answer_datagram(stack, &datagram);
                continue;
            }
            let flow = self.flows.iter_mut().find(|flow| {
                flow.guest == datagram.source && flow.destination == datagram.destination
            });
            let Some(flow) = flow else { continue };
            flow.used = now;
            match &mut flow.state {
                UdpState::Open(socket) => {
                    let _ = socket.send(&datagram.payload);
                }
                UdpState::Asking(_, waiting) if waiting.len() < UDP_WAITING => {
                    waiting.push(datagram.payload)
                }
                _ => {}
            }
        }
        let mut buffer = [0; 65536];
        for flow in &mut self.flows {
            let UdpState::Open(socket) = &flow.state else {
                continue;
            };
            while let Ok(len) = socket.recv(&mut buffer) {
                flow.used = now;
                stack.send_datagram(flow.destination, flow.guest, &buffer[..len]);
            }
        }
        let (live, idle): (Vec<_>, Vec<_>) =
            (self.flows.drain(..)).partition(|flow| now.duration_since(flow.used) < UDP_IDLE);
        self.flows = live;
        // A port that no flow uses any longer gives its buffers back.
        for flow in idle {
            let port = flow.destination.port();
            let used = (self.flows.iter()).any(|other| other.destination.port() == port);
            if !used && port != resolver.port() {
                stack.unbind(port);
            }
        }
    }
}

/// Reports to `reporter`, if there is one, how the rules decided a
/// connection to `name` on `port` once its lookup found `addresses`, of
/// which it may connect to `allowed`. A name with no addresses is no
/// decision.
fn answered(
    reporter: Option<&Reporter>,
    name: Option<&str>,
    port: u16,
    addresses: &[IpAddr],
    allowed: &[IpAddr],
) {
    match (reporter, allowed.first(), addresses.first()) {
        (Some(reporter), Some(&address), _) => reporter.decided(true, name, address, port),
        (_, None, Some(&address)) => report(reporter, name, address, port),
        _ => {}
    }
}

/// Reports to `reporter`, if there is one, a connection it blocked.
fn report(reporter: Option<&Reporter>, name: Option<&str>, address: IpAddr, port: u16) {
    if let Some(reporter) = reporter {
        reporter.report(Blocked::Connection {
            name: name.map(str::to_owned),
            address,
            port,
        });
    }
}

fn flow_destination(flow: &Flow) -> SocketAddr {
    match *flow {
        Flow::Tcp { destination, .. } | Flow::Udp { destination, .. } => destination,
    }
}

/// The addresses of `addresses` that the proxy may connect to on `port`,
/// for a connection attributed to `name`.
fn allowed(
    network: &Network,
    harness: &[&str],
    name: Option<&str>,
    port: u16,
    addresses: &[IpAddr],
) -> Vec<IpAddr> {
    addresses
        .iter()
        .copied()
        .filter(|&address| {
            let connection = Connection {
                name,
                address,
                port,
                request: None,
            };
            proxied(network, harness, &connection).allows()
        })
        .collect()
}

/// Starts connecting to the first of `addresses` that takes a connect, and
/// keeps the rest to try after it.
fn connecting(mut addresses: Vec<IpAddr>, port: u16, now: Instant) -> Option<Step> {
    while !addresses.is_empty() {
        let address = addresses.remove(0);
        if let Ok(socket) = open(SocketAddr::new(address, port), libc::SOCK_STREAM) {
            return Some(Step::Connecting {
                socket,
                rest: addresses,
                since: now,
            });
        }
    }
    None
}

/// A UDP socket connected to the first of `addresses` that takes one.
fn open_udp(addresses: &[IpAddr], port: u16) -> Option<UdpSocket> {
    addresses.iter().find_map(|&address| {
        open(SocketAddr::new(address, port), libc::SOCK_DGRAM)
            .ok()
            .map(UdpSocket::from)
    })
}

/// A non-blocking socket of `kind` that connects to `to`. A TCP connect
/// goes on after this returns.
fn open(to: SocketAddr, kind: libc::c_int) -> io::Result<OwnedFd> {
    let (family, address, length) = sockaddr(to);
    // SAFETY: a plain call, whose result is checked.
    let fd = unsafe { libc::socket(family, kind | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is new, and nothing else owns it.
    let socket = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: `address` holds `length` bytes of a socket address.
    let connected = unsafe { libc::connect(fd, (&raw const address).cast(), length) };
    if connected != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(error);
        }
    }
    Ok(socket)
}

/// `address` as the socket address that `connect` takes.
fn sockaddr(address: SocketAddr) -> (libc::c_int, libc::sockaddr_storage, libc::socklen_t) {
    // SAFETY: an all-zero `sockaddr_storage` is valid.
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    match address {
        SocketAddr::V4(v4) => {
            let raw = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: v4.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from(*v4.ip()).to_be(),
                },
                sin_zero: [0; 8],
            };
            // SAFETY: `sockaddr_storage` is larger than `sockaddr_in`.
            unsafe { (&raw mut storage).cast::<libc::sockaddr_in>().write(raw) };
            (
                libc::AF_INET,
                storage,
                size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        }
        SocketAddr::V6(v6) => {
            let raw = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as libc::sa_family_t,
                sin6_port: v6.port().to_be(),
                sin6_flowinfo: 0,
                sin6_addr: libc::in6_addr {
                    s6_addr: v6.ip().octets(),
                },
                sin6_scope_id: 0,
            };
            // SAFETY: `sockaddr_storage` is larger than `sockaddr_in6`.
            unsafe { (&raw mut storage).cast::<libc::sockaddr_in6>().write(raw) };
            (
                libc::AF_INET6,
                storage,
                size_of::<libc::sockaddr_in6>() as libc::socklen_t,
            )
        }
    }
}

/// The error a connecting socket ended with, or 0 if it connected.
fn socket_error(fd: i32) -> io::Result<i32> {
    let mut error: libc::c_int = 0;
    let mut length = size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `error` and `length` outlive the call, and `length` is the
    // size of `error`.
    let got = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            (&raw mut error).cast(),
            &mut length,
        )
    };
    match got {
        0 => Ok(error),
        _ => Err(io::Error::last_os_error()),
    }
}

/// Reads one message from `fd` without waiting: `None` if none has come,
/// and `Some(0)` once the other end closed.
pub(super) fn recv(fd: i32, buffer: &mut [u8]) -> io::Result<Option<usize>> {
    loop {
        // SAFETY: `buffer` has room for the bytes the call writes.
        let got = unsafe {
            libc::recv(
                fd,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                libc::MSG_DONTWAIT,
            )
        };
        if got >= 0 {
            return Ok(Some(got as usize));
        }
        let error = io::Error::last_os_error();
        match error.kind() {
            io::ErrorKind::Interrupted => {}
            io::ErrorKind::WouldBlock => return Ok(None),
            _ if hung_up(&error) => return Ok(Some(0)),
            _ => return Err(error),
        }
    }
}

/// Sends one message on `fd`.
pub(super) fn send(fd: i32, message: &[u8]) -> io::Result<()> {
    // SAFETY: `message` holds the bytes the call reads.
    let sent = unsafe {
        libc::send(
            fd,
            message.as_ptr().cast(),
            message.len(),
            libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
        )
    };
    match sent {
        0.. => Ok(()),
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
