//! The network process's TCP/IP stack: one Ethernet interface on the guest's
//! link that ends every TCP connection, and takes every UDP datagram, to any
//! address and port. It routes nothing and opens no socket on the host. Frames
//! come in through [`Stack::receive`] and go out through [`Stack::transmit`].
//! The caller takes or refuses each new [`Flow`]: a refused TCP connection
//! gets a reset, and a refused datagram is dropped, with no answer. It can
//! also hold a TCP connection at its SYN, while it connects to the real
//! destination, and then take or refuse it. It decides what to do with each
//! connection and datagram it takes, and a [`Pipe`] forwards a connection
//! to a stream the caller opened.

#![forbid(unsafe_code)]

mod pipe;
mod resolver;

pub use pipe::{Guard, Pipe};
pub use resolver::{Pool, Resolver};

use std::collections::{BTreeMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Instant;

use smoltcp::iface::{Config as IfaceConfig, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{
    EthernetAddress, EthernetFrame, EthernetProtocol, HardwareAddress, IpAddress, IpCidr,
    IpProtocol, Ipv4Packet, Ipv4Repr, Ipv6ExtHeader, Ipv6ExtHeaderRepr, Ipv6HopByHopHeader,
    Ipv6HopByHopRepr, Ipv6OptionFailureType, Ipv6OptionRepr, Ipv6Packet, Ipv6Repr, TcpControl,
    TcpPacket, TcpRepr, UdpPacket, UdpRepr,
};

/// Bytes each TCP connection buffers in each direction.
const TCP_BUFFER: usize = 64 * 1024;
/// Datagrams, and their bytes, each UDP port buffers in each direction.
const UDP_DATAGRAMS: usize = 32;
const UDP_BUFFER: usize = 64 * 1024;
/// The TCP connections the guest may have started but not finished opening,
/// whether the stack took or the caller holds them. Each one taken costs
/// two [`TCP_BUFFER`]s.
const HALF_OPEN: usize = 64;
/// The destination ports the guest may send UDP to at once. Each one costs
/// two [`UDP_BUFFER`]s.
const UDP_PORTS: usize = 32;
/// How long a forgotten connection may wait to send its reset.
const FORGET_WAIT: std::time::Duration = std::time::Duration::from_secs(10);
/// The largest Ethernet frame on the link, without its check sequence.
const MAX_FRAME: usize = 1514;

/// The stack's place on the guest's link.
pub struct Config {
    pub mac: [u8; 6],
    pub ipv4: (Ipv4Addr, u8),
    pub ipv6: (Ipv6Addr, u8),
    /// Seeds the initial sequence numbers and ports the stack picks.
    pub seed: u64,
}

/// A connection the stack accepted, for [`Stack::recv`] and [`Stack::send`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConnectionId(SocketHandle);

/// A TCP connection from the guest, and the destination it asked for.
#[derive(Debug)]
pub struct Connection {
    pub id: ConnectionId,
    pub source: SocketAddr,
    pub destination: SocketAddr,
}

/// What the guest opens, for the caller to take or refuse: a TCP connection,
/// at its SYN, or one UDP datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Tcp {
        source: SocketAddr,
        destination: SocketAddr,
    },
    Udp {
        source: SocketAddr,
        destination: SocketAddr,
    },
}

/// What the caller makes of a new [`Flow`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Take,
    Refuse,
    /// Neither yet: the SYN of a TCP connection waits, unanswered, for
    /// [`Stack::release`]. The guest's copies of it go unanswered too, and
    /// aren't asked about. A datagram held is dropped.
    Hold,
}

/// A UDP datagram from the guest, and the destination it was sent to.
#[derive(Debug)]
pub struct Datagram {
    pub source: SocketAddr,
    pub destination: SocketAddr,
    pub payload: Vec<u8>,
}

pub struct Stack {
    mac: EthernetAddress,
    iface: Interface,
    sockets: SocketSet<'static>,
    link: Link,
    start: Instant,
    /// TCP sockets that listen for, or are still opening, a guest connection.
    opening: Vec<SocketHandle>,
    accepted: VecDeque<Connection>,
    /// The UDP socket for each destination port the guest has sent to.
    udp: BTreeMap<u16, SocketHandle>,
    /// The SYN of each connection the caller holds.
    held: Vec<(Flow, Vec<u8>)>,
    /// TCP sockets the caller is done with, and the first poll since. Each
    /// is freed once it has sent the reset that an abort queued, or after a
    /// while if it never can.
    forgotten: Vec<(SocketHandle, Option<Instant>)>,
}

impl Stack {
    pub fn new(config: Config) -> Stack {
        let start = Instant::now();
        let mac = EthernetAddress(config.mac);
        let mut link = Link::default();
        let mut iface_config = IfaceConfig::new(HardwareAddress::Ethernet(mac));
        iface_config.random_seed = config.seed;
        let mut iface = Interface::new(iface_config, &mut link, SmolInstant::from_millis(0));
        iface.update_ip_addrs(|addrs| {
            let (v4, v4_prefix) = config.ipv4;
            let (v6, v6_prefix) = config.ipv6;
            // Two addresses fit smoltcp's default IFACE_MAX_ADDR_COUNT.
            addrs.push(IpCidr::new(IpAddress::Ipv4(v4), v4_prefix)).ok();
            addrs.push(IpCidr::new(IpAddress::Ipv6(v6), v6_prefix)).ok();
        });
        // Take every destination as the stack's own, so a connection to any
        // address ends here.
        iface.set_any_ip(true);
        Stack {
            mac,
            iface,
            sockets: SocketSet::new(Vec::new()),
            link,
            start,
            opening: Vec::new(),
            accepted: VecDeque::new(),
            udp: BTreeMap::new(),
            held: Vec::new(),
            forgotten: Vec::new(),
        }
    }

    /// Takes one Ethernet frame from the guest, and handles it. If it opens a
    /// [`Flow`], `take` decides whether the stack takes it, as
    /// [`Stack::receive_holding`] says.
    pub fn receive(&mut self, frame: &[u8], now: Instant, take: impl FnOnce(&Flow) -> bool) {
        self.receive_holding(frame, now, |flow| match take(flow) {
            true => Verdict::Take,
            false => Verdict::Refuse,
        });
    }

    /// Takes one Ethernet frame from the guest, and handles it. If it opens a
    /// [`Flow`], `decide` says what the stack makes of it. A flow past the
    /// stack's limits on half-open connections and UDP ports is dropped,
    /// unasked.
    pub fn receive_holding(
        &mut self,
        frame: &[u8],
        now: Instant,
        decide: impl FnOnce(&Flow) -> Verdict,
    ) {
        match self.classify(frame) {
            Seen::Drop => return,
            Seen::Other => {}
            // A SYN sent again is no new flow.
            Seen::Flow(Flow::Tcp {
                source,
                destination,
            }) if self.holds(source, destination) => {}
            Seen::Flow(flow) if self.held.iter().any(|(held, _)| *held == flow) => return,
            // The guest sends a SYN again, and a datagram is best effort.
            Seen::Flow(flow) if self.full(&flow) => return,
            Seen::Flow(flow) => match (decide(&flow), flow) {
                (Verdict::Take, Flow::Tcp { destination, .. }) => self.listen(destination),
                (Verdict::Take, Flow::Udp { destination, .. }) => self.bind(destination.port()),
                (Verdict::Hold, Flow::Tcp { .. }) => {
                    self.held.push((flow, frame.to_vec()));
                    return;
                }
                // With no socket to take it, smoltcp answers a SYN with a
                // reset.
                (Verdict::Refuse, Flow::Tcp { .. }) => {}
                // smoltcp would answer it with a port unreachable message.
                (Verdict::Refuse | Verdict::Hold, Flow::Udp { .. }) => return,
            },
        }
        self.link.rx.push_back(frame.to_vec());
        self.poll(now);
    }

    /// Takes or refuses a connection the caller held. Its SYN is handled
    /// now, as if it had just come.
    pub fn release(&mut self, flow: Flow, take: bool, now: Instant) {
        let Some(at) = self.held.iter().position(|(held, _)| *held == flow) else {
            return;
        };
        let (_, frame) = self.held.remove(at);
        if let (true, Flow::Tcp { destination, .. }) = (take, flow) {
            self.listen(destination);
        }
        self.link.rx.push_back(frame);
        self.poll(now);
    }

    /// Runs the stack's timers, and sends what its sockets have queued.
    pub fn poll(&mut self, now: Instant) {
        let millis = i64::try_from(now.saturating_duration_since(self.start).as_millis())
            .unwrap_or(i64::MAX);
        self.iface.poll(
            SmolInstant::from_millis(millis),
            &mut self.link,
            &mut self.sockets,
        );
        let sockets = &mut self.sockets;
        self.forgotten.retain_mut(|(handle, since)| {
            let handle = *handle;
            let since = *since.get_or_insert(now);
            let socket = sockets.get::<tcp::Socket>(handle);
            // smoltcp keeps the endpoint of an aborted socket until its
            // reset has gone out, which waits for the guest's address.
            let resetting =
                socket.state() == tcp::State::Closed && socket.remote_endpoint().is_some();
            let keep = resetting && now.saturating_duration_since(since) < FORGET_WAIT;
            if !keep {
                sockets.remove(handle);
            }
            keep
        });
        let sockets = &mut self.sockets;
        let accepted = &mut self.accepted;
        self.opening.retain(|&handle| {
            let socket = sockets.get::<tcp::Socket>(handle);
            match socket.state() {
                tcp::State::SynReceived => true,
                // A socket that took no SYN, or went back to listening when
                // its connection was reset, would take a SYN nobody asked
                // about.
                tcp::State::Listen | tcp::State::Closed => {
                    sockets.remove(handle);
                    false
                }
                _ => {
                    if let (Some(local), Some(remote)) =
                        (socket.local_endpoint(), socket.remote_endpoint())
                    {
                        accepted.push_back(Connection {
                            id: ConnectionId(handle),
                            source: remote.into(),
                            destination: local.into(),
                        });
                    }
                    false
                }
            }
        });
    }

    /// How long the stack can wait for a frame before its timers need
    /// [`Stack::poll`], or `None` if they never do.
    pub fn delay(&mut self, now: Instant) -> Option<std::time::Duration> {
        let millis = i64::try_from(now.saturating_duration_since(self.start).as_millis())
            .unwrap_or(i64::MAX);
        self.iface
            .poll_delay(SmolInstant::from_millis(millis), &self.sockets)
            .map(|delay| std::time::Duration::from_micros(delay.total_micros()))
    }

    /// How many TCP and UDP sockets the stack keeps.
    pub fn sockets(&self) -> usize {
        self.sockets.iter().count()
    }

    /// How many SYNs the caller holds.
    pub fn held(&self) -> usize {
        self.held.len()
    }

    /// The next Ethernet frame for the guest.
    pub fn transmit(&mut self) -> Option<Vec<u8>> {
        self.link.tx.pop_front()
    }

    /// The next connection the guest opened.
    pub fn accept(&mut self) -> Option<Connection> {
        self.accepted.pop_front()
    }

    /// Reads what the guest sent on a connection, and returns how many bytes.
    pub fn recv(&mut self, id: ConnectionId, buf: &mut [u8]) -> usize {
        self.sockets
            .get_mut::<tcp::Socket>(id.0)
            .recv_slice(buf)
            .unwrap_or(0)
    }

    /// Queues bytes for the guest on a connection, and returns how many fit.
    /// They go out at the next [`Stack::poll`].
    pub fn send(&mut self, id: ConnectionId, bytes: &[u8]) -> usize {
        self.sockets
            .get_mut::<tcp::Socket>(id.0)
            .send_slice(bytes)
            .unwrap_or(0)
    }

    /// Sends what is queued on a connection, then a FIN.
    pub fn close(&mut self, id: ConnectionId) {
        self.sockets.get_mut::<tcp::Socket>(id.0).close();
    }

    /// Resets a connection.
    pub fn abort(&mut self, id: ConnectionId) {
        self.sockets.get_mut::<tcp::Socket>(id.0).abort();
    }

    /// Whether the guest has closed its half of a connection, or reset it,
    /// and everything it sent has been read.
    pub fn received_all(&self, id: ConnectionId) -> bool {
        !self.sockets.get::<tcp::Socket>(id.0).may_recv()
    }

    /// Whether a connection has ended on both sides, or was reset. The
    /// caller then forgets it.
    pub fn ended(&self, id: ConnectionId) -> bool {
        matches!(
            self.sockets.get::<tcp::Socket>(id.0).state(),
            tcp::State::Closed | tcp::State::TimeWait
        )
    }

    /// Frees a connection, which the caller must not use again, once the
    /// next [`Stack::poll`] has sent what it queued.
    pub fn forget(&mut self, id: ConnectionId) {
        self.forgotten.push((id.0, None));
    }

    /// Resets and frees a connection that the stack took for `flow` but
    /// hasn't accepted yet, because the guest never finished opening it.
    pub fn abandon(&mut self, flow: Flow) {
        let Flow::Tcp {
            source,
            destination,
        } = flow
        else {
            return;
        };
        let sockets = &mut self.sockets;
        let forgotten = &mut self.forgotten;
        self.opening.retain(|&handle| {
            let socket = sockets.get_mut::<tcp::Socket>(handle);
            let ours = socket.local_endpoint().map(SocketAddr::from) == Some(destination)
                && socket.remote_endpoint().map(SocketAddr::from) == Some(source);
            if ours {
                socket.abort();
                forgotten.push((handle, None));
            }
            !ours
        });
    }

    /// Frees the socket that takes datagrams to `port`, so the next one
    /// there is a new flow to decide.
    pub fn unbind(&mut self, port: u16) {
        if let Some(handle) = self.udp.remove(&port) {
            self.sockets.remove(handle);
        }
    }

    /// The next datagram the guest sent.
    pub fn recv_datagram(&mut self) -> Option<Datagram> {
        for (&port, &handle) in &self.udp {
            let socket = self.sockets.get_mut::<udp::Socket>(handle);
            if let Ok((payload, meta)) = socket.recv() {
                let Some(local) = meta.local_address else {
                    continue;
                };
                return Some(Datagram {
                    source: meta.endpoint.into(),
                    destination: SocketAddr::new(IpAddr::from(local), port),
                    payload: payload.to_vec(),
                });
            }
        }
        None
    }

    /// Queues `payload` for the guest as one UDP datagram from `from` to
    /// `to`, on a port the guest has sent to. It goes out at the next
    /// [`Stack::poll`]. Returns whether it was queued.
    pub fn send_datagram(&mut self, from: SocketAddr, to: SocketAddr, payload: &[u8]) -> bool {
        let Some(&handle) = self.udp.get(&from.port()) else {
            return false;
        };
        let meta = udp::UdpMetadata {
            local_address: Some(from.ip().into()),
            ..to.into()
        };
        self.sockets
            .get_mut::<udp::Socket>(handle)
            .send_slice(payload, meta)
            .is_ok()
    }

    /// What `frame` is to the stack. Only a frame the stack can decide on
    /// reaches smoltcp: a flow sent to the stack, or a frame that opens
    /// none. It parses each header, and checks each checksum, as smoltcp
    /// does, so a frame smoltcp would refuse opens no flow. It reads past
    /// one hop-by-hop header, and drops what smoltcp drops for its
    /// addresses. A frame that hides its transport, such as behind any
    /// other IPv6 extension header or in a fragment, or a flow sent to every
    /// station, is dropped, because smoltcp would take it without asking.
    fn classify(&self, frame: &[u8]) -> Seen {
        let Ok(ethernet) = EthernetFrame::new_checked(frame) else {
            return Seen::Drop;
        };
        let to = ethernet.dst_addr();
        let ours = to == self.mac;
        if !ours && !to.is_broadcast() && !to.is_multicast() {
            return Seen::Drop;
        }
        let checksums = self.link.capabilities().checksum;
        let (source, destination, protocol, payload): (IpAddr, IpAddr, IpProtocol, &[u8]) =
            match ethernet.ethertype() {
                EthernetProtocol::Arp => return Seen::Other,
                EthernetProtocol::Ipv4 => {
                    let Ok(ip) = Ipv4Packet::new_checked(ethernet.payload()) else {
                        return Seen::Drop;
                    };
                    let Ok(repr) = Ipv4Repr::parse(&ip, &checksums) else {
                        return Seen::Drop;
                    };
                    if ip.more_frags() || ip.frag_offset() != 0 {
                        return Seen::Drop;
                    }
                    (
                        IpAddr::V4(repr.src_addr),
                        IpAddr::V4(repr.dst_addr),
                        repr.next_header,
                        ip.payload(),
                    )
                }
                EthernetProtocol::Ipv6 => {
                    let Ok(ip) = Ipv6Packet::new_checked(ethernet.payload()) else {
                        return Seen::Drop;
                    };
                    let Ok(repr) = Ipv6Repr::parse(&ip) else {
                        return Seen::Drop;
                    };
                    let (protocol, payload) = match repr.next_header {
                        IpProtocol::HopByHop => match behind_hop_by_hop(ip.payload()) {
                            Some(behind) => behind,
                            None => return Seen::Drop,
                        },
                        protocol => (protocol, ip.payload()),
                    };
                    (
                        IpAddr::V6(repr.src_addr),
                        IpAddr::V6(repr.dst_addr),
                        protocol,
                        payload,
                    )
                }
                _ => return Seen::Drop,
            };
        // smoltcp drops a packet from an address that isn't unicast, except
        // an IPv4 packet from no address. It drops a TCP segment from or to
        // no address.
        if !self.unicast(source) && source != IpAddr::V4(Ipv4Addr::UNSPECIFIED) {
            return Seen::Drop;
        }
        match protocol {
            // Neighbor discovery, and the stack's own ICMP.
            IpProtocol::Icmp | IpProtocol::Icmpv6 => Seen::Other,
            IpProtocol::Tcp
                if ours && !source.is_unspecified() && !destination.is_unspecified() =>
            {
                let tcp = TcpPacket::new_checked(payload).and_then(|tcp| {
                    TcpRepr::parse(&tcp, &source.into(), &destination.into(), &checksums)
                });
                match tcp {
                    Ok(tcp) if tcp.control == TcpControl::Syn && tcp.ack_number.is_none() => {
                        Seen::Flow(Flow::Tcp {
                            source: SocketAddr::new(source, tcp.src_port),
                            destination: SocketAddr::new(destination, tcp.dst_port),
                        })
                    }
                    // smoltcp gives a segment of no connection a reset.
                    Ok(_) => Seen::Other,
                    Err(_) => Seen::Drop,
                }
            }
            IpProtocol::Udp if ours => {
                let udp = UdpPacket::new_checked(payload).and_then(|udp| {
                    UdpRepr::parse(&udp, &source.into(), &destination.into(), &checksums)
                });
                match udp {
                    Ok(udp) => Seen::Flow(Flow::Udp {
                        source: SocketAddr::new(source, udp.src_port),
                        destination: SocketAddr::new(destination, udp.dst_port),
                    }),
                    Err(_) => Seen::Drop,
                }
            }
            _ => Seen::Drop,
        }
    }

    /// Whether smoltcp calls `address` unicast: not one for many stations,
    /// such as the broadcast address of the stack's IPv4 subnet, and not
    /// unspecified.
    fn unicast(&self, address: IpAddr) -> bool {
        match address {
            IpAddr::V4(address) => {
                let subnet = self.iface.ip_addrs().iter().any(
                    |cidr| matches!(cidr, IpCidr::Ipv4(cidr) if cidr.broadcast() == Some(address)),
                );
                !(address.is_broadcast()
                    || address.is_multicast()
                    || address.is_unspecified()
                    || subnet)
            }
            IpAddr::V6(address) => !(address.is_multicast() || address.is_unspecified()),
        }
    }

    /// Whether the stack holds all it may of what a new `flow` would need.
    fn full(&self, flow: &Flow) -> bool {
        match flow {
            Flow::Tcp { .. } => self.opening.len() + self.held.len() >= HALF_OPEN,
            Flow::Udp { destination, .. } => {
                self.udp.len() >= UDP_PORTS && !self.udp.contains_key(&destination.port())
            }
        }
    }

    /// Whether a connection the stack is still opening is the one from
    /// `source` to `destination`.
    fn holds(&self, source: SocketAddr, destination: SocketAddr) -> bool {
        self.opening.iter().any(|&handle| {
            let socket = self.sockets.get::<tcp::Socket>(handle);
            socket.local_endpoint().map(SocketAddr::from) == Some(destination)
                && socket.remote_endpoint().map(SocketAddr::from) == Some(source)
        })
    }

    /// Opens a socket that takes the next SYN to `destination`. The stack
    /// polls before it returns, so the SYN is the one it was opened for.
    fn listen(&mut self, destination: SocketAddr) {
        let buffer = || tcp::SocketBuffer::new(vec![0; TCP_BUFFER]);
        let mut socket = tcp::Socket::new(buffer(), buffer());
        if socket.listen(destination).is_ok() {
            self.opening.push(self.sockets.add(socket));
        }
    }

    /// Makes sure a socket takes datagrams to `port` on any address.
    fn bind(&mut self, port: u16) {
        if port == 0 || self.udp.contains_key(&port) {
            return;
        }
        let buffer = || {
            udp::PacketBuffer::new(
                vec![udp::PacketMetadata::EMPTY; UDP_DATAGRAMS],
                vec![0; UDP_BUFFER],
            )
        };
        let mut socket = udp::Socket::new(buffer(), buffer());
        if socket.bind(port).is_ok() {
            self.udp.insert(port, self.sockets.add(socket));
        }
    }
}

/// The protocol behind the hop-by-hop header that starts `payload`, and its
/// bytes, as smoltcp reads them. `None` if smoltcp would discard the packet,
/// because the header is malformed, or holds an option it must not skip.
fn behind_hop_by_hop(payload: &[u8]) -> Option<(IpProtocol, &[u8])> {
    let header = Ipv6ExtHeader::new_checked(payload).ok()?;
    let repr = Ipv6ExtHeaderRepr::parse(&header).ok()?;
    let options = Ipv6HopByHopHeader::new_checked(repr.data).ok()?;
    let options = Ipv6HopByHopRepr::parse(&options).ok()?;
    let discarded = options.options.iter().any(|option| {
        matches!(option, Ipv6OptionRepr::Unknown { type_, .. }
            if Ipv6OptionFailureType::from(*type_) != Ipv6OptionFailureType::Skip)
    });
    if discarded {
        return None;
    }
    let rest = payload.get(repr.header_len() + repr.data.len()..)?;
    Some((repr.next_header, rest))
}

/// What a frame from the guest is to the stack.
enum Seen {
    /// It opens a flow, for the caller to take or refuse.
    Flow(Flow),
    /// It opens none, and goes to smoltcp.
    Other,
    /// It goes nowhere.
    Drop,
}

/// The stack's side of the link: frames from the guest, and frames for it.
#[derive(Default)]
struct Link {
    rx: VecDeque<Vec<u8>>,
    tx: VecDeque<Vec<u8>>,
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
        let frame = self.rx.pop_front()?;
        Some((RxToken(frame), TxToken(&mut self.tx)))
    }

    fn transmit(&mut self, _: SmolInstant) -> Option<TxToken<'_>> {
        Some(TxToken(&mut self.tx))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = MAX_FRAME;
        caps
    }
}
