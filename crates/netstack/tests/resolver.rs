//! A second `smoltcp` interface plays the guest, and sends DNS queries over
//! an in-memory link to the stack, whose resolver answers them itself.
//! Queries are written by hand here, so the resolver's parser isn't also
//! the oracle for what it reads.

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Instant;

use netstack::{Config, Pool, Resolver, Stack};
use simple_dns::rdata::RData;
use simple_dns::{Packet, PacketFlag, RCODE};
use smoltcp::iface::{Config as IfaceConfig, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::socket::udp;
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr};

const GUEST_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 0x15];
const STACK_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 0x02];
const GUEST_V4: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const STACK_V4: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
const GUEST_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 0x15);
const STACK_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 0x2);

const A: u16 = 1;
const AAAA: u16 = 28;
const SVCB: u16 = 64;
const HTTPS: u16 = 65;

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

/// The guest, with one UDP socket to query from, and the stack and its
/// resolver on the other end of the link.
struct Net {
    iface: Interface,
    sockets: SocketSet<'static>,
    link: Link,
    socket: SocketHandle,
    stack: Stack,
    resolver: Resolver,
    start: Instant,
}

impl Net {
    /// The stack's resolver allows the names in `allowed`, and hands out
    /// addresses from `pool`.
    fn new(allowed: &'static [&'static str], pool: Pool) -> Net {
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
        let mut sockets = SocketSet::new(Vec::new());
        let buffer = || udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 8], vec![0; 8192]);
        let mut socket = udp::Socket::new(buffer(), buffer());
        socket.bind(40000).unwrap();
        let socket = sockets.add(socket);
        let stack = Stack::new(Config {
            mac: STACK_MAC,
            ipv4: (STACK_V4, 24),
            ipv6: (STACK_V6, 64),
            seed: 7,
        });
        let at = [IpAddr::V4(STACK_V4), IpAddr::V6(STACK_V6)];
        let resolver = Resolver::new(&at, pool, move |name: &str| allowed.contains(&name));
        Net {
            iface,
            sockets,
            link,
            socket,
            stack,
            resolver,
            start: Instant::now(),
        }
    }

    /// Sends `bytes` to `to` from the guest, moves frames until the link is
    /// quiet, and returns what the guest got back.
    fn send(&mut self, to: SocketAddr, bytes: &[u8]) -> Vec<Vec<u8>> {
        self.sockets
            .get_mut::<udp::Socket>(self.socket)
            .send_slice(bytes, to)
            .unwrap();
        for _ in 0..64 {
            let now = Instant::now();
            let millis = i64::try_from(now.duration_since(self.start).as_millis()).unwrap();
            self.iface.poll(
                SmolInstant::from_millis(millis),
                &mut self.link,
                &mut self.sockets,
            );
            let mut moved = false;
            while let Some(frame) = self.link.from_guest.pop_front() {
                let resolver = &self.resolver;
                self.stack
                    .receive(&frame, now, |flow| resolver.takes(flow));
                moved = true;
            }
            self.resolver.serve(&mut self.stack);
            self.stack.poll(now);
            while let Some(frame) = self.stack.transmit() {
                self.link.to_guest.push_back(frame);
                moved = true;
            }
            if !moved {
                let mut got = Vec::new();
                let socket = self.sockets.get_mut::<udp::Socket>(self.socket);
                while let Ok((payload, meta)) = socket.recv() {
                    assert_eq!(SocketAddr::from(meta.endpoint), to, "answered from elsewhere");
                    got.push(payload.to_vec());
                }
                return got;
            }
        }
        panic!("the link never went quiet");
    }

    /// The one answer to `query`, sent to the resolver's IPv4 address.
    fn ask(&mut self, query: &[u8]) -> Vec<u8> {
        let mut got = self.send(SocketAddr::new(IpAddr::V4(STACK_V4), 53), query);
        assert_eq!(got.len(), 1, "one answer to {query:?}");
        got.remove(0)
    }
}

/// A standard query with one question, as a stub resolver writes it.
fn query(id: u16, name: &str, kind: u16) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend(id.to_be_bytes());
    // Recursion desired, and one question.
    bytes.extend([0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
    for label in name.trim_end_matches('.').split('.') {
        bytes.push(u8::try_from(label.len()).unwrap());
        bytes.extend(label.as_bytes());
    }
    bytes.push(0);
    bytes.extend(kind.to_be_bytes());
    bytes.extend(1u16.to_be_bytes());
    bytes
}

/// The answer's code, and the addresses of its `A` records.
fn read(answer: &[u8], id: u16) -> (RCODE, Vec<Ipv4Addr>) {
    let packet = Packet::parse(answer).unwrap();
    assert_eq!(packet.id(), id);
    assert!(packet.has_flags(PacketFlag::RESPONSE));
    assert_eq!(packet.questions.len(), 1, "the question comes back");
    let addresses = packet
        .answers
        .iter()
        .map(|record| match &record.rdata {
            RData::A(a) => Ipv4Addr::from(a.address),
            other => panic!("an answer that isn't an address: {other:?}"),
        })
        .collect();
    (packet.rcode(), addresses)
}

fn pool() -> Pool {
    Pool::new(Ipv4Addr::new(198, 18, 0, 0), 16)
}

fn in_pool(address: Ipv4Addr) -> bool {
    address.octets()[..2] == [198, 18]
}

// spec(NETWORK): The resolver answers each name with its own address from a private pool that `ap` chooses to overlap no IP rule, so `ap` knows which name every address belongs to.
#[test]
fn an_allowed_name_gets_its_own_pool_address_every_time() {
    let mut net = Net::new(&["kagi.com", "api.kagi.com"], pool());
    let (code, first) = read(&net.ask(&query(1, "kagi.com", A)), 1);
    assert_eq!(code, RCODE::NoError);
    assert_eq!(first.len(), 1, "{first:?}");
    assert!(in_pool(first[0]), "{first:?}");
    // The same name, again and in another case, gets the same address.
    assert_eq!(read(&net.ask(&query(2, "kagi.com", A)), 2).1, first);
    assert_eq!(read(&net.ask(&query(3, "KAGI.com.", A)), 3).1, first);
    // Another name gets another.
    let (_, other) = read(&net.ask(&query(4, "api.kagi.com", A)), 4);
    assert_eq!(other.len(), 1);
    assert!(in_pool(other[0]), "{other:?}");
    assert_ne!(other, first);
    // And the resolver knows whose each one is.
    assert_eq!(net.resolver.name_of(first[0]), Some("kagi.com"));
    assert_eq!(net.resolver.name_of(other[0]), Some("api.kagi.com"));
    assert_eq!(net.resolver.name_of(Ipv4Addr::new(198, 18, 9, 9)), None);
}

#[test]
fn the_resolver_answers_on_its_ipv6_address_too() {
    let mut net = Net::new(&["kagi.com"], pool());
    let to = SocketAddr::new(IpAddr::V6(STACK_V6), 53);
    let got = net.send(to, &query(9, "kagi.com", A));
    assert_eq!(got.len(), 1);
    let (code, addresses) = read(&got[0], 9);
    assert_eq!(code, RCODE::NoError);
    assert_eq!(addresses.len(), 1);
}

// spec(NETWORK): It answers a name only if some rule, or the default, could allow a connection to it.
#[test]
fn a_name_no_rule_could_allow_gets_no_address() {
    let mut net = Net::new(&["kagi.com"], pool());
    let (code, addresses) = read(&net.ask(&query(5, "example.com", A)), 5);
    assert_eq!(code, RCODE::NameError);
    assert!(addresses.is_empty());
    // A label that holds a dot is no host name, even when it spells one.
    let mut dotted = query(6, "x", A);
    dotted.splice(12..14, [8u8].into_iter().chain(*b"kagi.com"));
    let (code, addresses) = read(&net.ask(&dotted), 6);
    assert_eq!(code, RCODE::NameError);
    assert!(addresses.is_empty());
}

// spec(NETWORK): The resolver removes HTTPS and SVCB records, so clients can't use Encrypted Client Hello to hide the server name.
#[test]
fn every_query_but_a_gets_an_empty_answer() {
    let mut net = Net::new(&["kagi.com"], pool());
    for (id, kind) in [(10, AAAA), (11, HTTPS), (12, SVCB), (13, 16), (14, 255)] {
        let (code, addresses) = read(&net.ask(&query(id, "kagi.com", kind)), id);
        assert_eq!(code, RCODE::NoError, "type {kind}");
        assert!(addresses.is_empty(), "type {kind}: {addresses:?}");
    }
    // Only an allowed name gets one, though.
    let (code, _) = read(&net.ask(&query(15, "example.com", AAAA)), 15);
    assert_eq!(code, RCODE::NameError);
}

#[test]
fn a_malformed_query_gets_no_answer_and_the_resolver_goes_on() {
    let mut net = Net::new(&["kagi.com"], pool());
    let to = SocketAddr::new(IpAddr::V4(STACK_V4), 53);
    let good = query(20, "kagi.com", A);
    let mut answer = good.clone();
    answer[2] |= 0x80;
    let mut two = good.clone();
    two[5] = 2;
    let mut update = good.clone();
    update[2] |= 5 << 3;
    let broken: [&[u8]; 6] = [
        b"query",
        &good[..good.len() - 1],
        // A label that runs past the end.
        &[0, 1, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 63, b'k'],
        &answer,
        &two,
        &update,
    ];
    for bytes in broken {
        assert_eq!(net.send(to, bytes), Vec::<Vec<u8>>::new(), "{bytes:?}");
    }
    let (code, addresses) = read(&net.ask(&good), 20);
    assert_eq!(code, RCODE::NoError);
    assert_eq!(addresses.len(), 1);
}

#[test]
fn a_query_to_any_other_address_is_not_the_resolver_s() {
    let mut net = Net::new(&["kagi.com"], pool());
    let elsewhere = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 53);
    assert!(net.send(elsewhere, &query(30, "kagi.com", A)).is_empty());
    let other_port = SocketAddr::new(IpAddr::V4(STACK_V4), 5353);
    assert!(net.send(other_port, &query(31, "kagi.com", A)).is_empty());
}

#[test]
fn a_full_pool_fails_new_names_and_keeps_answering_old_ones() {
    // A `/30` has two addresses for hosts.
    let small = Pool::new(Ipv4Addr::new(198, 18, 0, 0), 30);
    let mut net = Net::new(&["a.test", "b.test", "c.test"], small);
    let (_, a) = read(&net.ask(&query(40, "a.test", A)), 40);
    let (_, b) = read(&net.ask(&query(41, "b.test", A)), 41);
    let mut both = [a[0], b[0]];
    both.sort();
    assert_eq!(both, [Ipv4Addr::new(198, 18, 0, 1), Ipv4Addr::new(198, 18, 0, 2)]);
    let (code, addresses) = read(&net.ask(&query(42, "c.test", A)), 42);
    assert_eq!(code, RCODE::ServerFailure);
    assert!(addresses.is_empty());
    assert_eq!(read(&net.ask(&query(43, "a.test", A)), 43).1, a);
}

/// An IPv4 block.
fn v4(network: [u8; 4], prefix: u8) -> (IpAddr, u8) {
    (IpAddr::V4(Ipv4Addr::from(network)), prefix)
}

fn overlaps(pool: &Pool, (network, prefix): (IpAddr, u8)) -> bool {
    let IpAddr::V4(network) = network else {
        return false;
    };
    let (start, length) = pool.network();
    let shorter = prefix.min(length);
    let mask = u32::MAX.checked_shl(32 - u32::from(shorter)).unwrap_or(0);
    start.to_bits() & mask == network.to_bits() & mask
}

// spec(NETWORK): The resolver answers each name with its own address from a private pool that `ap` chooses to overlap no IP rule, so `ap` knows which name every address belongs to.
#[test]
fn the_pool_overlaps_no_ip_rule_and_no_address_on_the_link() {
    let link = (STACK_V4, 24);
    let alone = Pool::choose(link, &[]).unwrap();
    assert!(!overlaps(&alone, v4(STACK_V4.octets(), 24)));
    assert!(alone.network().0.is_private() || alone.network().0.octets()[0] == 198);

    // Rules over the first choices push the pool on.
    let rules = [
        v4([198, 18, 0, 0], 15),
        v4([100, 64, 0, 0], 10),
        v4([172, 16, 0, 5], 32),
    ];
    let pushed = Pool::choose(link, &rules).unwrap();
    for rule in rules {
        assert!(!overlaps(&pushed, rule), "{:?} overlaps {rule:?}", pushed.network());
    }
    // So does the link, wherever it is.
    let on_the_pool = (alone.network().0, 24);
    let moved = Pool::choose(on_the_pool, &[]).unwrap();
    assert!(!overlaps(&moved, (IpAddr::V4(on_the_pool.0), 24)));

    // An IPv6 rule over IPv4-mapped addresses meets IPv4 addresses too.
    let mapped = (IpAddr::V6(Ipv4Addr::new(198, 18, 0, 0).to_ipv6_mapped()), 111);
    let past = Pool::choose(link, &[mapped]).unwrap();
    assert!(!overlaps(&past, v4([198, 18, 0, 0], 15)));
    // Any other IPv6 rule doesn't.
    let other = (IpAddr::V6("2001:db8::".parse().unwrap()), 32);
    assert_eq!(
        Pool::choose(link, &[other]).unwrap().network(),
        alone.network()
    );

    // With every address under a rule, there is no pool.
    assert!(Pool::choose(link, &[v4([0, 0, 0, 0], 0)]).is_none());
}
