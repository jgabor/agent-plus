//! The network process's TCP/IP stack: one Ethernet interface on the guest's
//! link that ends every TCP connection, and takes every UDP datagram, to any
//! address and port. It routes nothing and opens no socket on the host. Frames
//! come in through [`Stack::receive`] and go out through [`Stack::transmit`].
//! The caller takes or refuses each new [`Flow`]: a refused TCP connection
//! gets a reset, and a refused datagram is dropped, with no answer. It
//! decides what to do with each connection and datagram it takes.

mod resolver;

pub use resolver::{PORT as RESOLVER_PORT, Pool, Resolver};

use std::collections::{BTreeMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Instant;

use smoltcp::iface::{Config as IfaceConfig, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{
    EthernetAddress, EthernetFrame, EthernetProtocol, HardwareAddress, IpAddress, IpCidr,
    IpProtocol, Ipv4Packet, Ipv6Packet, TcpPacket, UdpPacket,
};

/// Bytes each TCP connection buffers in each direction.
const TCP_BUFFER: usize = 64 * 1024;
/// Datagrams, and their bytes, each UDP port buffers in each direction.
const UDP_DATAGRAMS: usize = 32;
const UDP_BUFFER: usize = 64 * 1024;
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
        }
    }

    /// Takes one Ethernet frame from the guest, and handles it. If it opens a
    /// [`Flow`], `take` decides whether the stack takes it.
    pub fn receive(&mut self, frame: &[u8], now: Instant, take: impl FnOnce(&Flow) -> bool) {
        match self.classify(frame) {
            Seen::Drop => return,
            Seen::Other => {}
            // A SYN sent again is no new flow.
            Seen::Flow(Flow::Tcp {
                source,
                destination,
            }) if self.holds(source, destination) => {}
            Seen::Flow(flow) => match (take(&flow), flow) {
                (true, Flow::Tcp { destination, .. }) => self.listen(destination),
                (true, Flow::Udp { destination, .. }) => self.bind(destination.port()),
                // With no socket to take it, smoltcp answers a SYN with a
                // reset.
                (false, Flow::Tcp { .. }) => {}
                // smoltcp would answer it with a port unreachable message.
                (false, Flow::Udp { .. }) => return,
            },
        }
        self.link.rx.push_back(frame.to_vec());
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
    /// none. A frame that hides its transport, such as behind an IPv6
    /// extension header or in a fragment, or a flow sent to every station,
    /// is dropped, because smoltcp would take it without asking.
    fn classify(&self, frame: &[u8]) -> Seen {
        let Ok(ethernet) = EthernetFrame::new_checked(frame) else {
            return Seen::Drop;
        };
        let to = ethernet.dst_addr();
        let ours = to == self.mac;
        if !ours && !to.is_broadcast() && !to.is_multicast() {
            return Seen::Drop;
        }
        let (source, destination, protocol, payload): (IpAddr, IpAddr, IpProtocol, &[u8]) =
            match ethernet.ethertype() {
                EthernetProtocol::Arp => return Seen::Other,
                EthernetProtocol::Ipv4 => {
                    let Ok(ip) = Ipv4Packet::new_checked(ethernet.payload()) else {
                        return Seen::Drop;
                    };
                    if ip.more_frags() || ip.frag_offset() != 0 {
                        return Seen::Drop;
                    }
                    (
                        IpAddr::V4(ip.src_addr()),
                        IpAddr::V4(ip.dst_addr()),
                        ip.next_header(),
                        ip.payload(),
                    )
                }
                EthernetProtocol::Ipv6 => {
                    let Ok(ip) = Ipv6Packet::new_checked(ethernet.payload()) else {
                        return Seen::Drop;
                    };
                    (
                        IpAddr::V6(ip.src_addr()),
                        IpAddr::V6(ip.dst_addr()),
                        ip.next_header(),
                        ip.payload(),
                    )
                }
                _ => return Seen::Drop,
            };
        match protocol {
            // Neighbor discovery, and the stack's own ICMP.
            IpProtocol::Icmp | IpProtocol::Icmpv6 => Seen::Other,
            IpProtocol::Tcp if ours => match TcpPacket::new_checked(payload) {
                Ok(tcp) if tcp.syn() && !tcp.ack() => Seen::Flow(Flow::Tcp {
                    source: SocketAddr::new(source, tcp.src_port()),
                    destination: SocketAddr::new(destination, tcp.dst_port()),
                }),
                // smoltcp gives a segment of no connection a reset.
                Ok(_) => Seen::Other,
                Err(_) => Seen::Drop,
            },
            IpProtocol::Udp if ours => match UdpPacket::new_checked(payload) {
                Ok(udp) => Seen::Flow(Flow::Udp {
                    source: SocketAddr::new(source, udp.src_port()),
                    destination: SocketAddr::new(destination, udp.dst_port()),
                }),
                Err(_) => Seen::Drop,
            },
            _ => Seen::Drop,
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
