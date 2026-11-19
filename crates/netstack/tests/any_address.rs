//! A second `smoltcp` interface plays the guest. It sends Ethernet frames over
//! an in-memory link to the stack under test, which must end its connections
//! and take its datagrams to any address and port, unless the caller refuses
//! them.

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Instant;

use netstack::{Config, Flow, Stack};
use smoltcp::iface::{Config as IfaceConfig, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr};

const GUEST_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 0x15];
const STACK_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 0x02];
const GUEST_V4: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const STACK_V4: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
const GUEST_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 0x15);
const STACK_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 0x2);

/// One side of the link: frames the guest receives, and frames it sent.
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

struct Guest {
    iface: Interface,
    sockets: SocketSet<'static>,
    link: Link,
}

impl Guest {
    fn new() -> Guest {
        let mut link = Link::default();
        let config = IfaceConfig::new(HardwareAddress::Ethernet(EthernetAddress(GUEST_MAC)));
        let mut iface = Interface::new(config, &mut link, SmolInstant::from_millis(0));
        iface.update_ip_addrs(|addrs| {
            addrs
                .push(IpCidr::new(IpAddress::Ipv4(GUEST_V4), 24))
                .unwrap();
            addrs
                .push(IpCidr::new(IpAddress::Ipv6(GUEST_V6), 64))
                .unwrap();
        });
        iface.routes_mut().add_default_ipv4_route(STACK_V4).unwrap();
        iface.routes_mut().add_default_ipv6_route(STACK_V6).unwrap();
        Guest {
            iface,
            sockets: SocketSet::new(Vec::new()),
            link,
        }
    }

    fn connect(&mut self, to: SocketAddr, from_port: u16) -> SocketHandle {
        let buffer = || tcp::SocketBuffer::new(vec![0; 4096]);
        let mut socket = tcp::Socket::new(buffer(), buffer());
        socket.connect(self.iface.context(), to, from_port).unwrap();
        self.sockets.add(socket)
    }

    fn udp(&mut self, port: u16) -> SocketHandle {
        let buffer = || udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0; 4096]);
        let mut socket = udp::Socket::new(buffer(), buffer());
        socket.bind(port).unwrap();
        self.sockets.add(socket)
    }
}

fn stack() -> Stack {
    Stack::new(Config {
        mac: STACK_MAC,
        ipv4: (STACK_V4, 24),
        ipv6: (STACK_V6, 64),
        seed: 7,
    })
}

/// Takes every flow.
fn allow(_: &Flow) -> bool {
    true
}

/// Moves frames both ways until the link is quiet, with every flow taken.
fn exchange(guest: &mut Guest, stack: &mut Stack, start: Instant) {
    exchange_deciding(guest, stack, start, allow);
}

/// Whether `frame` is an ICMP or ICMPv6 destination unreachable message.
fn unreachable(frame: &[u8]) -> bool {
    match frame[12..14] {
        [0x08, 0x00] => {
            let header = usize::from(frame[14] & 0xf) * 4;
            frame[14 + 9] == 1 && frame[14 + header] == 3
        }
        [0x86, 0xdd] => frame[14 + 6] == 58 && frame[14 + 40] == 1,
        _ => false,
    }
}

/// Moves frames both ways until the link is quiet, and returns the frames
/// the stack sent. `decide` takes or refuses each new flow.
fn exchange_deciding(
    guest: &mut Guest,
    stack: &mut Stack,
    start: Instant,
    mut decide: impl FnMut(&Flow) -> bool,
) -> Vec<Vec<u8>> {
    let mut sent = Vec::new();
    for _ in 0..64 {
        let now = Instant::now();
        let millis = i64::try_from(now.duration_since(start).as_millis()).unwrap();
        guest.iface.poll(
            SmolInstant::from_millis(millis),
            &mut guest.link,
            &mut guest.sockets,
        );
        let mut moved = false;
        while let Some(frame) = guest.link.from_guest.pop_front() {
            stack.receive(&frame, now, &mut decide);
            moved = true;
        }
        stack.poll(now);
        while let Some(frame) = stack.transmit() {
            sent.push(frame.clone());
            guest.link.to_guest.push_back(frame);
            moved = true;
        }
        if !moved {
            return sent;
        }
    }
    panic!("the link never went quiet");
}

/// Connects to `to`, sends `bytes`, and returns what the stack accepted and read.
fn connect_and_send(to: SocketAddr, bytes: &[u8]) -> (SocketAddr, SocketAddr, Vec<u8>) {
    let start = Instant::now();
    let mut guest = Guest::new();
    let mut stack = stack();
    let handle = guest.connect(to, 49152);
    exchange(&mut guest, &mut stack, start);
    assert_eq!(
        guest.sockets.get::<tcp::Socket>(handle).state(),
        tcp::State::Established
    );
    guest
        .sockets
        .get_mut::<tcp::Socket>(handle)
        .send_slice(bytes)
        .unwrap();
    exchange(&mut guest, &mut stack, start);

    let connection = stack.accept().expect("the stack accepted the connection");
    assert!(stack.accept().is_none(), "one connection, accepted once");
    let mut read = vec![0; 1024];
    let n = stack.recv(connection.id, &mut read);
    read.truncate(n);
    (connection.source, connection.destination, read)
}

#[test]
fn accepts_a_tcp_connection_to_any_ipv4_address() {
    let to = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), 443);
    let (source, destination, read) = connect_and_send(to, b"hello over v4");
    assert_eq!(destination, to);
    assert_eq!(source, SocketAddr::new(IpAddr::V4(GUEST_V4), 49152));
    assert_eq!(read, b"hello over v4");
}

#[test]
fn accepts_a_tcp_connection_to_any_ipv6_address() {
    let to = SocketAddr::new(IpAddr::V6("2001:db8::1".parse().unwrap()), 80);
    let (source, destination, read) = connect_and_send(to, b"hello over v6");
    assert_eq!(destination, to);
    assert_eq!(source, SocketAddr::new(IpAddr::V6(GUEST_V6), 49152));
    assert_eq!(read, b"hello over v6");
}

#[test]
fn accepts_two_connections_to_one_destination() {
    let start = Instant::now();
    let mut guest = Guest::new();
    let mut stack = stack();
    let to = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), 443);
    let first = guest.connect(to, 49152);
    let second = guest.connect(to, 49153);
    exchange(&mut guest, &mut stack, start);
    for handle in [first, second] {
        assert_eq!(
            guest.sockets.get::<tcp::Socket>(handle).state(),
            tcp::State::Established
        );
    }
    let mut ports = [
        stack.accept().unwrap().source.port(),
        stack.accept().unwrap().source.port(),
    ];
    ports.sort_unstable();
    assert_eq!(ports, [49152, 49153]);
}

#[test]
fn the_stack_can_answer_a_connection() {
    let start = Instant::now();
    let mut guest = Guest::new();
    let mut stack = stack();
    let to = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), 443);
    let handle = guest.connect(to, 49152);
    exchange(&mut guest, &mut stack, start);
    let connection = stack.accept().unwrap();
    assert_eq!(stack.send(connection.id, b"reply"), 5);
    exchange(&mut guest, &mut stack, start);
    let mut read = [0; 16];
    let n = guest
        .sockets
        .get_mut::<tcp::Socket>(handle)
        .recv_slice(&mut read)
        .unwrap();
    assert_eq!(&read[..n], b"reply");
}

#[test]
fn takes_a_udp_datagram_to_any_address() {
    let start = Instant::now();
    let mut guest = Guest::new();
    let mut stack = stack();
    let handle = guest.udp(40000);
    let to = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 9)), 53);
    guest
        .sockets
        .get_mut::<udp::Socket>(handle)
        .send_slice(b"query", to)
        .unwrap();
    exchange(&mut guest, &mut stack, start);

    let datagram = stack.recv_datagram().expect("the stack took the datagram");
    assert_eq!(datagram.destination, to);
    assert_eq!(
        datagram.source,
        SocketAddr::new(IpAddr::V4(GUEST_V4), 40000)
    );
    assert_eq!(datagram.payload, b"query");
    assert!(stack.recv_datagram().is_none());
}

/// The guest's SYN to `to`, as the frame it puts on the link once ARP has
/// resolved the stack's address.
fn syn_frame(to: SocketAddr) -> Vec<u8> {
    let now = Instant::now();
    let mut guest = Guest::new();
    let mut stack = stack();
    guest.connect(to, 49152);
    guest.iface.poll(
        SmolInstant::from_millis(0),
        &mut guest.link,
        &mut guest.sockets,
    );
    let arp = guest
        .link
        .from_guest
        .pop_front()
        .expect("the guest asks for the gateway");
    stack.receive(&arp, now, allow);
    stack.poll(now);
    guest
        .link
        .to_guest
        .push_back(stack.transmit().expect("the stack answers ARP"));
    guest.iface.poll(
        SmolInstant::from_millis(1),
        &mut guest.link,
        &mut guest.sockets,
    );
    guest
        .link
        .from_guest
        .pop_front()
        .expect("the guest sends its SYN")
}

#[test]
fn answers_only_on_its_own_link() {
    let to = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), 443);
    let syn = syn_frame(to);
    assert_eq!(&syn[..6], &STACK_MAC, "the SYN is addressed to the stack");

    // Addressed to the stack, the SYN gets an answer.
    let now = Instant::now();
    let mut ours = stack();
    ours.receive(&syn, now, allow);
    ours.poll(now);
    assert!(ours.transmit().is_some());

    // Addressed to another station on the link, it gets nothing.
    let mut elsewhere = syn.clone();
    elsewhere[..6].copy_from_slice(&[0x02, 0, 0, 0, 0, 0x99]);
    let mut theirs = stack();
    theirs.receive(&elsewhere, now, allow);
    theirs.poll(now);
    assert!(theirs.transmit().is_none());
    assert!(theirs.accept().is_none());
}

/// Connects to `to` with every flow refused, and returns the flows the
/// stack asked about, and the guest's socket's state.
fn connect_refused(to: SocketAddr) -> (Vec<String>, tcp::State) {
    let start = Instant::now();
    let mut guest = Guest::new();
    let mut stack = stack();
    let handle = guest.connect(to, 49152);
    let mut asked = Vec::new();
    exchange_deciding(&mut guest, &mut stack, start, |flow| {
        asked.push(format!("{flow:?}"));
        false
    });
    assert!(
        stack.accept().is_none(),
        "a refused connection isn't accepted"
    );
    (asked, guest.sockets.get::<tcp::Socket>(handle).state())
}

#[test]
fn a_refused_tcp_connection_to_ipv4_is_reset() {
    let to = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), 443);
    let (asked, state) = connect_refused(to);
    assert_eq!(
        asked,
        [format!(
            "Tcp {{ source: {}:49152, destination: {to} }}",
            GUEST_V4
        )]
    );
    // smoltcp closes a connecting socket that gets a reset.
    assert_eq!(state, tcp::State::Closed);
}

#[test]
fn a_refused_tcp_connection_to_ipv6_is_reset() {
    let to = SocketAddr::new(IpAddr::V6("2001:db8::1".parse().unwrap()), 80);
    let (asked, state) = connect_refused(to);
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(asked[0].starts_with("Tcp "), "{asked:?}");
    assert_eq!(state, tcp::State::Closed);
}

#[test]
fn a_refused_udp_datagram_is_dropped_without_an_answer() {
    for to in [
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 9)), 53),
        SocketAddr::new(IpAddr::V6("2001:db8::9".parse().unwrap()), 443),
    ] {
        let start = Instant::now();
        let mut guest = Guest::new();
        let mut stack = stack();
        let handle = guest.udp(40000);
        // Resolves the stack's address first, so that every frame it sends
        // after that would be an answer to the datagram.
        exchange(&mut guest, &mut stack, start);
        guest
            .sockets
            .get_mut::<udp::Socket>(handle)
            .send_slice(b"query", to)
            .unwrap();
        let mut asked = Vec::new();
        let sent = exchange_deciding(&mut guest, &mut stack, start, |flow| {
            asked.push(format!("{flow:?}"));
            false
        });
        assert!(
            !sent.iter().any(|frame| unreachable(frame)),
            "the stack answered a refused datagram"
        );
        assert!(stack.recv_datagram().is_none());
        assert!(
            asked.iter().any(|flow| flow.starts_with("Udp ")),
            "{asked:?}"
        );
        assert!(guest.sockets.get_mut::<udp::Socket>(handle).recv().is_err());
    }
}
