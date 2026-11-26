//! A second `smoltcp` interface plays the guest. It sends Ethernet frames over
//! an in-memory link to the stack under test, which must end its connections
//! and take its datagrams to any address and port, unless the caller refuses
//! them.

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Instant;

use netstack::{Config, Flow, Stack, Verdict};
use smoltcp::iface::{Config as IfaceConfig, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{self, ChecksumCapabilities, Device, DeviceCapabilities, Medium};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{
    EthernetAddress, EthernetFrame, EthernetProtocol, EthernetRepr, HardwareAddress, Icmpv4Packet,
    Icmpv4Repr, Icmpv6Packet, Icmpv6Repr, IpAddress, IpCidr, IpProtocol, Ipv4Packet, Ipv4Repr,
    Ipv6Packet, Ipv6Repr, TcpPacket, UdpPacket,
};

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
    decide: impl FnMut(&Flow) -> bool,
) -> Vec<Vec<u8>> {
    exchange_rewriting(guest, stack, start, decide, |frame| frame)
}

/// Moves frames both ways until the link is quiet, as [`exchange_deciding`]
/// does, with each frame the guest sends turned into what `rewrite` makes
/// of it.
fn exchange_rewriting(
    guest: &mut Guest,
    stack: &mut Stack,
    start: Instant,
    mut decide: impl FnMut(&Flow) -> bool,
    rewrite: impl Fn(Vec<u8>) -> Vec<u8>,
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
            stack.receive(&rewrite(frame), now, &mut decide);
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

/// The flags of the TCP segment in a frame the stack sent, as `(syn, ack,
/// rst)`, or `None` if it isn't TCP over IPv4.
fn tcp_flags(frame: &[u8]) -> Option<(bool, bool, bool)> {
    let ethernet = EthernetFrame::new_checked(frame).ok()?;
    let ip = Ipv4Packet::new_checked(ethernet.payload()).ok()?;
    let tcp = TcpPacket::new_checked(ip.payload()).ok()?;
    Some((tcp.syn(), tcp.ack(), tcp.rst()))
}

/// A stack that knows the guest's MAC address, from the guest's ARP request
/// for the stack's address, as a real link starts.
fn introduced() -> Stack {
    let mut arp = vec![0xff; 6];
    arp.extend(GUEST_MAC);
    arp.extend([0x08, 0x06, 0, 1, 8, 0, 6, 4, 0, 1]);
    arp.extend(GUEST_MAC);
    arp.extend(GUEST_V4.octets());
    arp.extend([0; 6]);
    arp.extend(STACK_V4.octets());
    let mut stack = stack();
    stack.receive(&arp, Instant::now(), |flow| panic!("ARP opened {flow:?}"));
    assert!(stack.transmit().is_some(), "the stack answers ARP");
    stack
}

/// `syn` turned into the guest's reset of the connection it opens.
fn reset_of(syn: &[u8]) -> Vec<u8> {
    let mut frame = syn.to_vec();
    let mut ethernet = EthernetFrame::new_unchecked(&mut frame[..]);
    let mut ip = Ipv4Packet::new_unchecked(ethernet.payload_mut());
    let (source, destination) = (ip.src_addr(), ip.dst_addr());
    let mut tcp = TcpPacket::new_unchecked(ip.payload_mut());
    tcp.set_seq_number(tcp.seq_number() + 1);
    tcp.set_syn(false);
    tcp.set_rst(true);
    tcp.fill_checksum(&source.into(), &destination.into());
    frame
}

#[test]
fn a_refused_syn_is_reset_after_an_earlier_one_to_its_destination_was_taken() {
    let to = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), 443);
    let syn = syn_frame(to);
    let now = Instant::now();
    let mut stack = introduced();
    stack.receive(&syn, now, allow);
    let answer = stack.transmit().expect("the stack answers the SYN");
    assert_eq!(tcp_flags(&answer), Some((true, true, false)));
    // The guest gives up on that connection before it opens.
    stack.receive(&reset_of(&syn), now, allow);
    while stack.transmit().is_some() {}

    let mut asked = 0;
    stack.receive(&syn, now, |_| {
        asked += 1;
        false
    });
    assert_eq!(asked, 1);
    let answer = stack.transmit().expect("the stack answers the SYN");
    assert_eq!(tcp_flags(&answer), Some((false, true, true)), "not a reset");
    assert!(stack.accept().is_none());
}

#[test]
fn a_retransmitted_syn_of_a_taken_connection_is_not_asked_about_again() {
    let to = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), 443);
    let syn = syn_frame(to);
    let now = Instant::now();
    let mut stack = introduced();
    stack.receive(&syn, now, allow);
    while stack.transmit().is_some() {}
    stack.receive(&syn, now, |flow| panic!("asked again about {flow:?}"));
    // The connection it opened goes on.
    while let Some(frame) = stack.transmit() {
        assert_ne!(tcp_flags(&frame).map(|flags| flags.2), Some(true), "reset");
    }
}

/// The guest's UDP datagram to `to`, as the frame it sends once the stack's
/// address is resolved.
fn datagram_frame(to: SocketAddr) -> Vec<u8> {
    let start = Instant::now();
    let mut guest = Guest::new();
    let mut stack = stack();
    let handle = guest.udp(40000);
    guest
        .sockets
        .get_mut::<udp::Socket>(handle)
        .send_slice(b"query", to)
        .unwrap();
    // Resolves the stack's address, and refuses the datagram.
    exchange_deciding(&mut guest, &mut stack, start, |_| false);
    guest
        .sockets
        .get_mut::<udp::Socket>(handle)
        .send_slice(b"query", to)
        .unwrap();
    let millis = i64::try_from(start.elapsed().as_millis()).unwrap() + 1;
    guest.iface.poll(
        SmolInstant::from_millis(millis),
        &mut guest.link,
        &mut guest.sockets,
    );
    let frame = guest.link.from_guest.pop_front().expect("the guest sends");
    assert_eq!(&frame[..6], &STACK_MAC, "{frame:?}");
    frame
}

/// `frame`, an IPv6 packet, with an empty hop-by-hop header before its TCP
/// or UDP header. The checksum covers neither, so it still holds.
fn with_hop_by_hop(frame: &[u8]) -> Vec<u8> {
    // Padding.
    with_hop_by_hop_option(frame, 1)
}

/// `frame`, an IPv6 packet, with a hop-by-hop header that holds one option
/// of type `option`, and four bytes of data.
fn with_hop_by_hop_option(frame: &[u8], option: u8) -> Vec<u8> {
    let mut out = frame[..14 + 40].to_vec();
    let next = out[14 + 6];
    out[14 + 6] = 0;
    let length = u16::from_be_bytes([out[14 + 4], out[14 + 5]]) + 8;
    out[14 + 4..14 + 6].copy_from_slice(&length.to_be_bytes());
    out.extend([next, 0, option, 4, 0, 0, 0, 0]);
    out.extend(&frame[14 + 40..]);
    out
}

#[test]
fn a_flow_behind_an_ipv6_extension_header_is_never_taken_unasked() {
    let to = SocketAddr::new(IpAddr::V6("2001:db8::9".parse().unwrap()), 53);
    let plain = datagram_frame(to);
    // smoltcp reads past one hop-by-hop header, and no further.
    let hidden = with_hop_by_hop(&with_hop_by_hop(&plain));
    let now = Instant::now();
    let mut stack = stack();
    // An earlier datagram to the port was taken, so a socket holds it.
    stack.receive(&plain, now, allow);
    assert!(stack.recv_datagram().is_some());
    stack.receive(&hidden, now, |_| false);
    assert!(
        stack.recv_datagram().is_none(),
        "a refused datagram was taken"
    );
    assert!(!std::iter::from_fn(|| stack.transmit()).any(|frame| unreachable(&frame)));
}

/// `frame`, behind a hop-by-hop header if it is IPv6 carrying TCP or UDP.
fn behind_hop_by_hop(frame: Vec<u8>) -> Vec<u8> {
    match (&frame[12..14], frame[14 + 6]) {
        ([0x86, 0xdd], 6 | 17) => with_hop_by_hop(&frame),
        _ => frame,
    }
}

/// Opens a connection to `tcp`, and sends a datagram to `udp`, every frame
/// behind a hop-by-hop header. Returns the guest and the stack after, and
/// each flow the stack asked about, which `take` decides.
fn open_behind_hop_by_hop(
    tcp: SocketAddr,
    udp: SocketAddr,
    take: bool,
) -> (Guest, SocketHandle, Stack, Vec<Flow>) {
    let start = Instant::now();
    let mut guest = Guest::new();
    let mut stack = stack();
    let connection = guest.connect(tcp, 49152);
    let socket = guest.udp(40000);
    guest
        .sockets
        .get_mut::<udp::Socket>(socket)
        .send_slice(b"query", udp)
        .unwrap();
    let mut asked = Vec::new();
    exchange_rewriting(
        &mut guest,
        &mut stack,
        start,
        |flow| {
            asked.push(*flow);
            take
        },
        behind_hop_by_hop,
    );
    (guest, connection, stack, asked)
}

#[test]
fn takes_a_flow_behind_a_hop_by_hop_header_when_the_caller_does() {
    let tcp_to = SocketAddr::new(IpAddr::V6("2001:db8::1".parse().unwrap()), 443);
    let udp_to = SocketAddr::new(IpAddr::V6("2001:db8::9".parse().unwrap()), 53);
    let (guest, connection, mut stack, mut asked) = open_behind_hop_by_hop(tcp_to, udp_to, true);
    asked.sort_by_key(|flow| matches!(flow, Flow::Udp { .. }));
    assert_eq!(
        asked,
        [
            Flow::Tcp {
                source: SocketAddr::new(IpAddr::V6(GUEST_V6), 49152),
                destination: tcp_to,
            },
            Flow::Udp {
                source: SocketAddr::new(IpAddr::V6(GUEST_V6), 40000),
                destination: udp_to,
            },
        ]
    );
    assert_eq!(
        guest.sockets.get::<tcp::Socket>(connection).state(),
        tcp::State::Established
    );
    let accepted = stack.accept().expect("the stack accepted the connection");
    assert_eq!(accepted.destination, tcp_to);
    let datagram = stack.recv_datagram().expect("the stack took the datagram");
    assert_eq!(datagram.destination, udp_to);
    assert_eq!(datagram.payload, b"query");
}

#[test]
fn a_flow_behind_a_hop_by_hop_header_that_the_caller_refuses_opens_no_socket() {
    let tcp_to = SocketAddr::new(IpAddr::V6("2001:db8::1".parse().unwrap()), 443);
    let udp_to = SocketAddr::new(IpAddr::V6("2001:db8::9".parse().unwrap()), 53);
    let (guest, connection, mut stack, asked) = open_behind_hop_by_hop(tcp_to, udp_to, false);
    assert!(
        asked.iter().any(|flow| matches!(flow, Flow::Tcp { .. }))
            && asked.iter().any(|flow| matches!(flow, Flow::Udp { .. })),
        "{asked:?}"
    );
    assert_eq!(stack.sockets(), 0);
    assert!(stack.accept().is_none());
    assert!(stack.recv_datagram().is_none());
    // smoltcp resets a SYN that no socket takes.
    assert_eq!(
        guest.sockets.get::<tcp::Socket>(connection).state(),
        tcp::State::Closed
    );
}

#[test]
fn a_flow_behind_an_option_smoltcp_discards_is_never_asked_about() {
    let to = SocketAddr::new(IpAddr::V6("2001:db8::9".parse().unwrap()), 53);
    // An option type smoltcp doesn't know, whose top bits say to discard
    // the packet.
    let frame = with_hop_by_hop_option(&datagram_frame(to), 0x7e);
    let mut stack = stack();
    stack.receive(&frame, Instant::now(), |flow| {
        panic!("asked about {flow:?}")
    });
    assert_eq!(stack.sockets(), 0);
}

#[test]
fn a_flow_sent_to_every_station_is_never_taken_unasked() {
    let to = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 9)), 53);
    let plain = datagram_frame(to);
    let now = Instant::now();
    let mut stack = stack();
    stack.receive(&plain, now, allow);
    assert!(stack.recv_datagram().is_some());
    for station in [[0xff; 6], [0x01, 0x00, 0x5e, 0, 0, 1]] {
        let mut sent = plain.clone();
        sent[..6].copy_from_slice(&station);
        stack.receive(&sent, now, |_| false);
        assert!(stack.recv_datagram().is_none(), "{station:?}");
    }
}

#[test]
fn answers_a_udp_datagram_from_the_address_it_was_sent_to() {
    let start = Instant::now();
    let mut guest = Guest::new();
    let mut stack = stack();
    let handle = guest.udp(40000);
    let to = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 9)), 9000);
    guest
        .sockets
        .get_mut::<udp::Socket>(handle)
        .send_slice(b"ping", to)
        .unwrap();
    exchange(&mut guest, &mut stack, start);
    let datagram = stack.recv_datagram().expect("the stack took the datagram");
    assert!(stack.send_datagram(datagram.destination, datagram.source, b"pong"));
    exchange(&mut guest, &mut stack, start);
    let (payload, meta) = guest
        .sockets
        .get_mut::<udp::Socket>(handle)
        .recv()
        .expect("the guest got the answer");
    assert_eq!(payload, b"pong");
    assert_eq!(SocketAddr::from(meta.endpoint), to);
}

#[test]
fn an_abandoned_connection_is_reset_and_its_next_syn_asked_about_again() {
    let to = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), 443);
    let syn = syn_frame(to);
    let now = Instant::now();
    let mut stack = introduced();
    let mut flows = Vec::new();
    stack.receive(&syn, now, |flow| {
        flows.push(*flow);
        true
    });
    let answer = stack.transmit().expect("the stack answers the SYN");
    assert_eq!(tcp_flags(&answer), Some((true, true, false)));
    // The guest never sends its ACK.
    stack.abandon(flows[0]);
    stack.poll(now);
    let reset = stack.transmit().expect("the stack resets the connection");
    assert_eq!(tcp_flags(&reset).map(|flags| flags.2), Some(true));
    let mut asked = 0;
    stack.receive(&syn, now, |_| {
        asked += 1;
        false
    });
    assert_eq!(asked, 1, "the SYN still belongs to the abandoned socket");
}

#[test]
fn an_unbound_port_takes_no_answer() {
    let start = Instant::now();
    let mut guest = Guest::new();
    let mut stack = stack();
    let handle = guest.udp(40000);
    let to = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 9)), 9000);
    guest
        .sockets
        .get_mut::<udp::Socket>(handle)
        .send_slice(b"ping", to)
        .unwrap();
    exchange(&mut guest, &mut stack, start);
    let datagram = stack.recv_datagram().unwrap();
    stack.unbind(9000);
    stack.poll(Instant::now());
    assert!(!stack.send_datagram(datagram.destination, datagram.source, b"pong"));
}

#[test]
fn a_connection_aborted_and_forgotten_still_sends_its_reset() {
    let start = Instant::now();
    let mut guest = Guest::new();
    let mut stack = stack();
    let to = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), 443);
    guest.connect(to, 49152);
    exchange(&mut guest, &mut stack, start);
    let connection = stack.accept().unwrap();
    // As a pipe does when its destination resets, with nothing more from
    // the guest.
    stack.abort(connection.id);
    stack.forget(connection.id);
    stack.poll(Instant::now());
    let mut resets = 0;
    while let Some(frame) = stack.transmit() {
        resets += usize::from(tcp_flags(&frame).is_some_and(|flags| flags.2));
    }
    assert_eq!(resets, 1);
}

/// The guest's answer to the stack's ARP request for the guest's address.
fn arp_reply() -> Vec<u8> {
    let mut frame = STACK_MAC.to_vec();
    frame.extend(GUEST_MAC);
    frame.extend([0x08, 0x06, 0, 1, 8, 0, 6, 4, 0, 2]);
    frame.extend(GUEST_MAC);
    frame.extend(GUEST_V4.octets());
    frame.extend(STACK_MAC);
    frame.extend(STACK_V4.octets());
    frame
}

#[test]
fn a_forgotten_connection_sends_its_reset_once_the_guest_s_address_is_known_again() {
    let start = Instant::now();
    let mut guest = Guest::new();
    let mut stack = stack();
    let to = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), 443);
    guest.connect(to, 49152);
    exchange(&mut guest, &mut stack, start);
    let connection = stack.accept().unwrap();
    // Long enough that the stack has forgotten the guest's MAC address.
    let later = start + std::time::Duration::from_secs(61);
    stack.abort(connection.id);
    stack.forget(connection.id);
    stack.poll(later);
    let asked = stack.transmit().expect("the stack asks for the guest");
    assert_eq!(&asked[12..14], &[0x08, 0x06], "an ARP request");
    while stack.transmit().is_some() {}
    stack.receive(&arp_reply(), later, |flow| panic!("ARP opened {flow:?}"));
    stack.poll(later);
    let mut resets = 0;
    while let Some(frame) = stack.transmit() {
        resets += usize::from(tcp_flags(&frame).is_some_and(|flags| flags.2));
    }
    assert_eq!(resets, 1);
}

#[test]
fn every_socket_is_freed_after_connections_end_by_fin_and_by_reset() {
    let start = Instant::now();
    let mut guest = Guest::new();
    let mut stack = stack();
    let to = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), 443);
    for round in 0..100u16 {
        let handle = guest.connect(to, 40000 + round);
        exchange(&mut guest, &mut stack, start);
        let connection = stack.accept().expect("the stack accepted the connection");
        let socket = guest.sockets.get_mut::<tcp::Socket>(handle);
        if round % 2 == 0 {
            socket.close();
            exchange(&mut guest, &mut stack, start);
            assert!(stack.received_all(connection.id));
            stack.close(connection.id);
        } else {
            socket.abort();
        }
        exchange(&mut guest, &mut stack, start);
        assert!(stack.ended(connection.id), "round {round}");
        stack.forget(connection.id);
        exchange(&mut guest, &mut stack, start);
        guest.sockets.remove(handle);
    }
    assert_eq!(stack.sockets(), 0);
}

/// `frame`, a TCP or UDP frame, with its segment or datagram changed by
/// `edit`, and its checksum filled, or spoiled if `bad`.
fn edited(frame: &[u8], bad: bool, edit: impl FnOnce(&mut [u8])) -> Vec<u8> {
    let ethernet = EthernetFrame::new_checked(frame).unwrap();
    let (from, to, protocol, start, end): (IpAddress, IpAddress, IpProtocol, usize, usize) =
        match ethernet.ethertype() {
            EthernetProtocol::Ipv4 => {
                let ip = Ipv4Packet::new_checked(ethernet.payload()).unwrap();
                let header = usize::from(ip.header_len());
                let total = usize::from(ip.total_len());
                (
                    ip.src_addr().into(),
                    ip.dst_addr().into(),
                    ip.next_header(),
                    header,
                    total,
                )
            }
            EthernetProtocol::Ipv6 => {
                let ip = Ipv6Packet::new_checked(ethernet.payload()).unwrap();
                let total = 40 + usize::from(ip.payload_len());
                (
                    ip.src_addr().into(),
                    ip.dst_addr().into(),
                    ip.next_header(),
                    40,
                    total,
                )
            }
            other => panic!("not IP: {other}"),
        };
    let mut frame = frame.to_vec();
    let transport = &mut frame[14 + start..14 + end];
    edit(transport);
    match protocol {
        IpProtocol::Udp => {
            let mut udp = UdpPacket::new_unchecked(transport);
            udp.fill_checksum(&from, &to);
            let spoiled = udp.checksum() ^ 0xff;
            if bad {
                udp.set_checksum(spoiled);
            }
        }
        IpProtocol::Tcp => {
            let mut tcp = TcpPacket::new_unchecked(transport);
            tcp.fill_checksum(&from, &to);
            let spoiled = tcp.checksum() ^ 0xff;
            if bad {
                tcp.set_checksum(spoiled);
            }
        }
        other => panic!("not TCP or UDP: {other}"),
    }
    frame
}

/// `frame`, a TCP or UDP frame, from another `source` port and to another
/// `destination` port.
fn with_ports(frame: &[u8], source: u16, destination: u16) -> Vec<u8> {
    edited(frame, false, |transport| {
        transport[..2].copy_from_slice(&source.to_be_bytes());
        transport[2..4].copy_from_slice(&destination.to_be_bytes());
    })
}

#[test]
fn a_flow_smoltcp_would_refuse_to_parse_is_never_asked_about() {
    let tcp_to = [
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), 443),
        SocketAddr::new(IpAddr::V6("2001:db8::1".parse().unwrap()), 443),
    ];
    let udp_to = [
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 9)), 53),
        SocketAddr::new(IpAddr::V6("2001:db8::9".parse().unwrap()), 53),
    ];
    let syns = tcp_to.map(syn_frame);
    let datagrams = udp_to.map(datagram_frame);
    let mut refused = Vec::new();
    for frame in syns.iter().chain(&datagrams) {
        // A version field that says the other IP version.
        let mut wrong = frame.clone();
        wrong[14] ^= 0x20;
        if let [0x08, 0x00] = wrong[12..14] {
            Ipv4Packet::new_unchecked(&mut wrong[14..]).fill_checksum();
        }
        refused.push(("wrong version", wrong));
        refused.push(("bad checksum", edited(frame, true, |_| {})));
        if let [0x08, 0x00] = frame[12..14] {
            let mut header = frame.clone();
            // The IPv4 header checksum.
            header[24] ^= 0xff;
            refused.push(("bad IPv4 header checksum", header));
        }
    }
    for syn in &syns {
        let first_option = |length: u8| {
            edited(syn, false, |tcp| {
                let mut tcp = TcpPacket::new_unchecked(tcp);
                let options = tcp.options_mut();
                assert!(options.len() >= 2, "the SYN has options");
                // A maximum segment size.
                options[..2].copy_from_slice(&[2, length]);
            })
        };
        refused.extend([
            ("MSS of the wrong length", first_option(3)),
            ("option past the header", first_option(0xff)),
            ("SYN from port 0", with_ports(syn, 0, 443)),
            ("SYN to port 0", with_ports(syn, 49152, 0)),
            (
                "SYN with FIN",
                edited(syn, false, |tcp| {
                    TcpPacket::new_unchecked(tcp).set_fin(true)
                }),
            ),
            (
                "SYN with RST",
                edited(syn, false, |tcp| {
                    TcpPacket::new_unchecked(tcp).set_rst(true)
                }),
            ),
        ]);
    }
    for datagram in &datagrams {
        refused.push(("datagram to port 0", with_ports(datagram, 40000, 0)));
    }
    let now = Instant::now();
    // Each frame, as the guest sent it, is asked about.
    for frame in syns.iter().chain(&datagrams) {
        let mut asked = false;
        stack().receive(&edited(frame, false, |_| {}), now, |_| {
            asked = true;
            false
        });
        assert!(asked, "{frame:?}");
    }
    let mut asked = Vec::new();
    for (kind, frame) in refused {
        let mut stack = stack();
        stack.receive(&frame, now, |flow| {
            asked.push((kind, *flow));
            false
        });
        assert_eq!(stack.sockets(), 0, "{kind}");
    }
    assert!(asked.is_empty(), "asked about {asked:#?}");
}

// spec(NETWORK): **Connections at once.** The guest can have at most 64 TCP connections that it has started but not finished opening, and a SYN past that is dropped unanswered, so the guest sends it again.
#[test]
fn half_open_connections_and_held_syns_stay_within_their_limit() {
    let to = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), 443);
    let syn = syn_frame(to);
    let now = Instant::now();
    for verdict in [Verdict::Take, Verdict::Hold] {
        let mut stack = introduced();
        let mut asked = Vec::new();
        for port in 0..200 {
            let frame = with_ports(&syn, 30000 + port, 443);
            stack.receive_holding(&frame, now, |flow| {
                asked.push(*flow);
                verdict
            });
            assert!(stack.sockets() <= 64, "{verdict:?}");
            assert!(stack.held() <= 64, "{verdict:?}");
        }
        assert_eq!(asked.len(), 64, "{verdict:?}");
        while stack.transmit().is_some() {}
        // A SYN past the limit isn't asked about, and gets no answer.
        let frame = with_ports(&syn, 29999, 443);
        stack.receive_holding(&frame, now, |flow| panic!("asked about {flow:?}"));
        assert!(stack.transmit().is_none(), "{verdict:?}");
        // Once one is freed, the next SYN is asked about again.
        match verdict {
            Verdict::Hold => stack.release(asked[0], false, now),
            _ => stack.abandon(asked[0]),
        }
        stack.poll(now);
        let mut again = 0;
        stack.receive_holding(&with_ports(&syn, 29998, 443), now, |_| {
            again += 1;
            Verdict::Refuse
        });
        assert_eq!(again, 1, "{verdict:?}");
    }
}

// spec(NETWORK): The guest can send UDP to at most 32 destination ports at once, and a datagram to another port is dropped.
#[test]
fn udp_ports_stay_within_their_limit() {
    let to = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 9)), 53);
    let datagram = datagram_frame(to);
    let now = Instant::now();
    let mut stack = stack();
    let mut asked = 0;
    for port in 0..100 {
        let frame = with_ports(&datagram, 40000, 1000 + port);
        stack.receive(&frame, now, |_| {
            asked += 1;
            true
        });
        assert!(stack.sockets() <= 32);
    }
    assert_eq!(asked, 32);
    assert_eq!(stack.sockets(), 32);
    // A port already open still takes datagrams.
    while stack.recv_datagram().is_some() {}
    stack.receive(&with_ports(&datagram, 40000, 1000), now, |_| true);
    assert!(stack.recv_datagram().is_some());
    // Once one is freed, a datagram to a new port is asked about again.
    stack.unbind(1001);
    let mut again = 0;
    stack.receive(&with_ports(&datagram, 40000, 2000), now, |_| {
        again += 1;
        true
    });
    assert_eq!(again, 1);
    assert!(stack.recv_datagram().is_some());
}

/// The guest's ICMP or ICMPv6 echo request to `to`, as `ping` sends it.
fn echo_request(to: IpAddr) -> Vec<u8> {
    let data = b"ping";
    let checksums = ChecksumCapabilities::default();
    let (ethertype, ip) = match to {
        IpAddr::V4(to) => {
            let icmp = Icmpv4Repr::EchoRequest {
                ident: 7,
                seq_no: 1,
                data,
            };
            let ip = Ipv4Repr {
                src_addr: GUEST_V4,
                dst_addr: to,
                next_header: IpProtocol::Icmp,
                payload_len: icmp.buffer_len(),
                hop_limit: 64,
            };
            let mut packet = vec![0; ip.buffer_len() + icmp.buffer_len()];
            let mut ipv4 = Ipv4Packet::new_unchecked(&mut packet[..]);
            ip.emit(&mut ipv4, &checksums);
            icmp.emit(
                &mut Icmpv4Packet::new_unchecked(ipv4.payload_mut()),
                &checksums,
            );
            (EthernetProtocol::Ipv4, packet)
        }
        IpAddr::V6(to) => {
            let icmp = Icmpv6Repr::EchoRequest {
                ident: 7,
                seq_no: 1,
                data,
            };
            let ip = Ipv6Repr {
                src_addr: GUEST_V6,
                dst_addr: to,
                next_header: IpProtocol::Icmpv6,
                payload_len: icmp.buffer_len(),
                hop_limit: 64,
            };
            let mut packet = vec![0; ip.buffer_len() + icmp.buffer_len()];
            let mut ipv6 = Ipv6Packet::new_unchecked(&mut packet[..]);
            ip.emit(&mut ipv6);
            let mut echo = Icmpv6Packet::new_unchecked(ipv6.payload_mut());
            icmp.emit(&GUEST_V6, &to, &mut echo, &checksums);
            (EthernetProtocol::Ipv6, packet)
        }
    };
    let ethernet = EthernetRepr {
        src_addr: EthernetAddress(GUEST_MAC),
        dst_addr: EthernetAddress(STACK_MAC),
        ethertype,
    };
    let mut frame = vec![0; ethernet.buffer_len()];
    ethernet.emit(&mut EthernetFrame::new_unchecked(&mut frame[..]));
    frame.extend(ip);
    frame
}

/// Whether `frame` is an ICMP or ICMPv6 echo reply.
fn echo_reply(frame: &[u8]) -> bool {
    match frame[12..14] {
        [0x08, 0x00] => {
            let header = usize::from(frame[14] & 0xf) * 4;
            frame[14 + 9] == 1 && frame[14 + header] == 0
        }
        [0x86, 0xdd] => frame[14 + 6] == 58 && frame[14 + 40] == 129,
        _ => false,
    }
}

// spec(NETWORK): **ICMP** isn't carried, so `ping` gets no reply.
#[test]
fn a_ping_gets_no_reply_from_anywhere() {
    let start = Instant::now();
    let mut stack = stack();
    // A datagram over each protocol first, so that each side knows the
    // other's link address, and the stack could send a reply. A guest
    // waits a second between lookups, so each protocol gets its own.
    for to in ["198.51.100.9:53", "[2001:db8::9]:53"] {
        let to: SocketAddr = to.parse().unwrap();
        let mut guest = Guest::new();
        let handle = guest.udp(40000);
        let socket = guest.sockets.get_mut::<udp::Socket>(handle);
        socket.send_slice(b"query", to).unwrap();
        exchange(&mut guest, &mut stack, start);
        assert!(stack.recv_datagram().is_some(), "{to}");
    }
    // To a destination past the stack, and to the stack itself.
    for to in [
        IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
        IpAddr::V4(STACK_V4),
        IpAddr::V6("2606:4700::1111".parse().unwrap()),
        IpAddr::V6(STACK_V6),
    ] {
        let now = Instant::now();
        stack.receive(&echo_request(to), now, |flow| {
            panic!("a ping to {to} opened {flow:?}")
        });
        stack.poll(now);
        while let Some(frame) = stack.transmit() {
            assert!(!echo_reply(&frame), "a ping to {to} got a reply");
        }
    }
}
