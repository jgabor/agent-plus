//! A connection the stack holds at its SYN until the caller has connected
//! to the real destination, and then forwards to it, over an in-memory link
//! from a second `smoltcp` interface that plays the guest. The destination
//! is a listener on this host's loopback.

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use netstack::{Config, ConnectionId, Flow, Pipe, Stack, Verdict};
use smoltcp::iface::{Config as IfaceConfig, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::socket::tcp;
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr};

const GUEST_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 0x15];
const STACK_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 0x02];
const GUEST_V4: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const STACK_V4: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
/// Where the guest connects. The caller decides where that really goes.
const TO: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), 443);
/// How long a test waits for something to happen.
const WAIT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Link {
    to_guest: VecDeque<Vec<u8>>,
    from_guest: VecDeque<Vec<u8>>,
}

struct RxToken(Vec<u8>);
struct TxToken<'a>(&'a mut VecDeque<Vec<u8>>);

impl phy::RxToken for RxToken {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl phy::TxToken for TxToken<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut frame = vec![0; len];
        let result = f(&mut frame);
        self.0.push_back(frame);
        result
    }
}

impl Device for Link {
    type RxToken<'a> = RxToken;
    type TxToken<'a> = TxToken<'a>;

    fn receive(&mut self, _: SmolInstant) -> Option<(RxToken, TxToken<'_>)> {
        let frame = self.to_guest.pop_front()?;
        Some((RxToken(frame), TxToken(&mut self.from_guest)))
    }

    fn transmit(&mut self, _: SmolInstant) -> Option<TxToken<'_>> {
        Some(TxToken(&mut self.from_guest))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = 1514;
        caps
    }
}

/// The guest, the stack, and what the caller holds: the flows the stack
/// held, and the pipe of the one connection.
struct Net {
    start: Instant,
    iface: Interface,
    sockets: SocketSet<'static>,
    link: Link,
    stack: Stack,
    held: Vec<Flow>,
    asked: usize,
    /// The caller's connection to the destination, and the connection the
    /// stack accepted for it, until the pipe takes both.
    upstream: Option<TcpStream>,
    pending: Option<ConnectionId>,
    pipe: Option<Pipe>,
    /// Whether the pipe has said the connection ended.
    ended: bool,
}

impl Net {
    fn new() -> Net {
        let mut link = Link::default();
        let config = IfaceConfig::new(HardwareAddress::Ethernet(EthernetAddress(GUEST_MAC)));
        let mut iface = Interface::new(config, &mut link, SmolInstant::from_millis(0));
        iface.update_ip_addrs(|addrs| {
            addrs
                .push(IpCidr::new(IpAddress::Ipv4(GUEST_V4), 24))
                .unwrap();
        });
        iface.routes_mut().add_default_ipv4_route(STACK_V4).unwrap();
        let stack = Stack::new(Config {
            mac: STACK_MAC,
            ipv4: (STACK_V4, 24),
            ipv6: ("fd00::2".parse().unwrap(), 64),
            seed: 7,
        });
        Net {
            start: Instant::now(),
            iface,
            sockets: SocketSet::new(Vec::new()),
            link,
            stack,
            held: Vec::new(),
            asked: 0,
            upstream: None,
            pending: None,
            pipe: None,
            ended: false,
        }
    }

    fn connect(&mut self) -> SocketHandle {
        let buffer = || tcp::SocketBuffer::new(vec![0; 4096]);
        let mut socket = tcp::Socket::new(buffer(), buffer());
        socket.connect(self.iface.context(), TO, 49152).unwrap();
        self.sockets.add(socket)
    }

    fn guest(&mut self, handle: SocketHandle) -> &mut tcp::Socket<'static> {
        self.sockets.get_mut::<tcp::Socket>(handle)
    }

    /// Moves frames both ways, and pumps the pipe, once. Each new flow is
    /// held.
    fn step(&mut self) {
        let now = Instant::now();
        let millis = i64::try_from(now.duration_since(self.start).as_millis()).unwrap();
        self.iface.poll(
            SmolInstant::from_millis(millis),
            &mut self.link,
            &mut self.sockets,
        );
        while let Some(frame) = self.link.from_guest.pop_front() {
            let (held, asked) = (&mut self.held, &mut self.asked);
            self.stack.receive_holding(&frame, now, |flow| {
                *asked += 1;
                held.push(*flow);
                Verdict::Hold
            });
        }
        if let Some(connection) = self.stack.accept() {
            assert!(self.pipe.is_none(), "one connection");
            assert_eq!(connection.destination, TO);
            self.pending = Some(connection.id);
        }
        if let Some(id) = self.pending.take_if(|_| self.upstream.is_some()) {
            self.pipe = Some(Pipe::new(id, self.upstream.take().unwrap()));
        }
        if let Some(pipe) = &mut self.pipe {
            if !pipe.pump(&mut self.stack) {
                self.pipe = None;
                self.ended = true;
            }
        }
        self.stack.poll(now);
        while let Some(frame) = self.stack.transmit() {
            self.link.to_guest.push_back(frame);
        }
    }

    /// Steps until `done` holds, or fails after a while.
    #[track_caller]
    fn until(&mut self, what: &str, mut done: impl FnMut(&mut Net) -> bool) {
        let deadline = Instant::now() + WAIT;
        while !done(self) {
            assert!(Instant::now() < deadline, "never {what}");
            self.step();
            thread::sleep(Duration::from_millis(1));
        }
    }
}

/// A destination on this host's loopback, and the caller's connection to
/// it, which the pipe takes.
fn destination() -> (TcpListener, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let upstream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    upstream.set_nonblocking(true).unwrap();
    (listener, upstream)
}

/// The guest's connection to `TO`, held, then released and taken, with its
/// pipe to a destination, and the destination's end of it.
fn forwarded() -> (Net, SocketHandle, TcpStream) {
    let mut net = Net::new();
    let handle = net.connect();
    net.until("held the SYN", |net| !net.held.is_empty());
    // While the caller connects, the guest sends its SYN again, and the
    // stack neither asks again nor answers.
    for _ in 0..5 {
        net.step();
    }
    assert_eq!(net.asked, 1);
    assert_eq!(net.guest(handle).state(), tcp::State::SynSent);
    let (listener, upstream) = destination();
    let (server, _) = listener.accept().unwrap();
    server.set_read_timeout(Some(WAIT)).unwrap();
    net.upstream = Some(upstream);
    let flow = net.held[0];
    net.stack.release(flow, true, Instant::now());
    net.until("established", |net| {
        net.guest(handle).state() == tcp::State::Established && net.pipe.is_some()
    });
    (net, handle, server)
}

fn read_guest(net: &mut Net, handle: SocketHandle, want: usize) -> Vec<u8> {
    let mut got = Vec::new();
    net.until("read the guest's bytes", |net| {
        let socket = net.guest(handle);
        if socket.can_recv() {
            let mut buffer = [0; 4096];
            let n = socket.recv_slice(&mut buffer).unwrap();
            got.extend_from_slice(&buffer[..n]);
        }
        got.len() >= want
    });
    got
}

fn read_server(net: &mut Net, server: &mut TcpStream, want: usize) -> Vec<u8> {
    server.set_nonblocking(true).unwrap();
    let mut got = Vec::new();
    net.until("read the server's bytes", |_| {
        let mut buffer = [0; 4096];
        match server.read(&mut buffer) {
            Ok(n) => got.extend_from_slice(&buffer[..n]),
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            Err(error) => panic!("{error}"),
        }
        got.len() >= want
    });
    got
}

/// Whether the server's end has read to the end.
fn server_ended(net: &mut Net, server: &mut TcpStream) {
    server.set_nonblocking(true).unwrap();
    net.until("closed the server's end", |_| {
        let mut buffer = [0; 64];
        match server.read(&mut buffer) {
            Ok(0) => true,
            Ok(n) => panic!("more bytes: {:?}", &buffer[..n]),
            Err(error) if error.kind() == ErrorKind::WouldBlock => false,
            Err(error) => panic!("{error}"),
        }
    });
}

#[test]
fn bytes_pass_both_ways_unchanged() {
    let (mut net, handle, mut server) = forwarded();
    // More than either side buffers at once, so that the pipe must wait for
    // room.
    let up: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let mut sent = 0;
    let mut got = Vec::new();
    net.until("sent every byte up", |net| {
        if sent < up.len() {
            sent += net.guest(handle).send_slice(&up[sent..]).unwrap();
        }
        server.set_nonblocking(true).unwrap();
        let mut buffer = [0; 8192];
        match server.read(&mut buffer) {
            Ok(n) => got.extend_from_slice(&buffer[..n]),
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            Err(error) => panic!("{error}"),
        }
        got.len() == up.len()
    });
    assert!(got == up, "the bytes changed on the way up");

    let down: Vec<u8> = (0..150_000u32).map(|i| (i % 241) as u8).collect();
    let writer = {
        let mut server = server.try_clone().unwrap();
        server.set_nonblocking(false).unwrap();
        let down = down.clone();
        thread::spawn(move || server.write_all(&down).unwrap())
    };
    let got = read_guest(&mut net, handle, down.len());
    writer.join().unwrap();
    assert!(got == down, "the bytes changed on the way down");
}

#[test]
fn a_close_from_the_guest_reaches_the_destination() {
    let (mut net, handle, mut server) = forwarded();
    net.guest(handle).send_slice(b"last words").unwrap();
    net.guest(handle).close();
    assert_eq!(read_server(&mut net, &mut server, 10), b"last words");
    server_ended(&mut net, &mut server);
    // The destination can still answer a half-closed connection, then end
    // it, and the connection ends on both sides.
    server.write_all(b"reply").unwrap();
    assert_eq!(read_guest(&mut net, handle, 5), b"reply");
    drop(server);
    net.until("ended the connection", |net| net.ended);
    assert!(!net.guest(handle).may_recv());
}

#[test]
fn a_close_from_the_destination_reaches_the_guest() {
    let (mut net, handle, mut server) = forwarded();
    server.write_all(b"goodbye").unwrap();
    drop(server);
    assert_eq!(read_guest(&mut net, handle, 7), b"goodbye");
    net.until("closed the guest's receive half", |net| {
        !net.guest(handle).may_recv()
    });
    net.guest(handle).close();
    net.until("ended the connection", |net| net.ended);
}

#[test]
fn a_destination_that_refuses_gets_the_guest_a_reset() {
    let mut net = Net::new();
    let handle = net.connect();
    net.until("held the SYN", |net| !net.held.is_empty());
    // Nothing listens there any longer.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let refused = TcpStream::connect(address).unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::ConnectionRefused);
    let flow = net.held[0];
    net.stack.release(flow, false, Instant::now());
    net.until("reset the guest", |net| {
        net.guest(handle).state() == tcp::State::Closed
    });
    assert!(net.stack.accept().is_none());
    assert_eq!(net.asked, 1);
}
