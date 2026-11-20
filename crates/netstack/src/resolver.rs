//! The guest's only resolver. It answers each query itself, and never asks
//! upstream. An `A` query for a name the caller could allow gets an address
//! of the name's own from a private pool, and every other query for that
//! name gets an empty answer. A name the caller couldn't allow doesn't
//! exist. The resolver remembers whose each address is, so a connection to
//! one can be attributed to its name.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};

use simple_dns::rdata::{A, RData};
use simple_dns::{CLASS, OPCODE, Packet, PacketFlag, QCLASS, QTYPE, RCODE, ResourceRecord, TYPE};

use crate::{Flow, Stack};

/// The port the resolver answers on.
const PORT: u16 = 53;
/// How long, in seconds, a client may keep an answer. The address stays the
/// name's for the whole run, so this only bounds how often it asks again.
const TTL: u32 = 300;
/// The blocks the pool is taken from, in the order they are tried, as one
/// `/16` of each that is free. The first is RFC 2544's benchmarking block,
/// which no real service uses, and the rest are the shared and private
/// blocks.
const BLOCKS: [([u8; 4], u8); 5] = [
    ([198, 18, 0, 0], 15),
    ([100, 64, 0, 0], 10),
    ([172, 16, 0, 0], 12),
    ([192, 168, 0, 0], 16),
    ([10, 0, 0, 0], 8),
];
const POOL_PREFIX: u8 = 16;

/// The IPv4 block the resolver hands addresses out of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pool {
    network: Ipv4Addr,
    prefix: u8,
}

impl Pool {
    /// The block `network/prefix`. Its first and last addresses are never
    /// handed out, unless it has no others.
    pub fn new(network: Ipv4Addr, prefix: u8) -> Pool {
        let prefix = prefix.min(32);
        Pool {
            network: Ipv4Addr::from_bits(network.to_bits() & mask(prefix)),
            prefix,
        }
    }

    /// The first free `/16` of a private block that overlaps neither the
    /// guest's `link` nor any block in `rules`, or `None` if every one does.
    /// An IPv6 block meets IPv4 addresses only inside the IPv4-mapped block.
    pub fn choose(link: (Ipv4Addr, u8), rules: &[(IpAddr, u8)]) -> Option<Pool> {
        let taken: Vec<(Ipv4Addr, u8)> = rules
            .iter()
            .filter_map(|&(network, prefix)| match network {
                IpAddr::V4(v4) => Some((v4, prefix)),
                IpAddr::V6(v6) if prefix >= 96 => Some((v6.to_ipv4_mapped()?, prefix - 96)),
                IpAddr::V6(_) => None,
            })
            .chain([link])
            .collect();
        BLOCKS.iter().find_map(|&(block, length)| {
            let block = u32::from_be_bytes(block);
            (0..1u32 << (POOL_PREFIX - length)).find_map(|at| {
                let pool = Pool::new(
                    Ipv4Addr::from_bits(block | at << (32 - POOL_PREFIX)),
                    POOL_PREFIX,
                );
                let free = taken.iter().all(|&(network, prefix)| {
                    let shorter = mask(prefix.min(pool.prefix));
                    pool.network.to_bits() & shorter != network.to_bits() & shorter
                });
                free.then_some(pool)
            })
        })
    }

    /// The block, as its first address and its prefix.
    pub fn network(&self) -> (Ipv4Addr, u8) {
        (self.network, self.prefix)
    }

    /// The addresses it hands out, first and last.
    fn range(&self) -> (u32, u32) {
        let first = self.network.to_bits();
        let last = first | !mask(self.prefix);
        match last - first {
            0 | 1 => (first, last),
            _ => (first + 1, last - 1),
        }
    }
}

fn mask(prefix: u8) -> u32 {
    u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0)
}

pub struct Resolver {
    at: Vec<IpAddr>,
    allows: Box<dyn Fn(&str) -> bool>,
    /// The next address to hand out, and the last there is.
    next: u32,
    last: u32,
    addresses: HashMap<String, Ipv4Addr>,
    names: HashMap<Ipv4Addr, String>,
}

impl Resolver {
    /// A resolver on each address in `at`, which answers the names that
    /// `allows` allows from `pool`. `allows` gets each name in lowercase,
    /// without a final dot.
    pub fn new(at: &[IpAddr], pool: Pool, allows: impl Fn(&str) -> bool + 'static) -> Resolver {
        let (next, last) = pool.range();
        Resolver {
            at: at.to_vec(),
            allows: Box::new(allows),
            next,
            last,
            addresses: HashMap::new(),
            names: HashMap::new(),
        }
    }

    /// Whether `flow` is a query to this resolver, for [`Stack::receive`].
    pub fn takes(&self, flow: &Flow) -> bool {
        match flow {
            Flow::Udp { destination, .. } => {
                destination.port() == PORT && self.at.contains(&destination.ip())
            }
            Flow::Tcp { .. } => false,
        }
    }

    /// Answers every query that `stack` took for this resolver.
    pub fn serve(&mut self, stack: &mut Stack) {
        while let Some(datagram) = stack.recv_datagram() {
            if !self.at.contains(&datagram.destination.ip()) || datagram.destination.port() != PORT
            {
                continue;
            }
            if let Some(answer) = self.answer(&datagram.payload) {
                stack.send_datagram(datagram.destination, datagram.source, &answer);
            }
        }
    }

    /// The name whose address `address` is, if the resolver handed it out.
    pub fn name_of(&self, address: Ipv4Addr) -> Option<&str> {
        self.names.get(&address).map(String::as_str)
    }

    /// The answer to `query`, or `None` if it isn't a standard query with
    /// one question.
    pub fn answer(&mut self, query: &[u8]) -> Option<Vec<u8>> {
        let query = Packet::parse(query).ok()?;
        if query.has_flags(PacketFlag::RESPONSE)
            || query.opcode() != OPCODE::StandardQuery
            || query.questions.len() != 1
        {
            return None;
        }
        let question = &query.questions[0];
        let mut answer = Packet::new_reply(query.id());
        answer.set_flags(PacketFlag::AUTHORITATIVE_ANSWER | PacketFlag::RECURSION_AVAILABLE);
        if query.has_flags(PacketFlag::RECURSION_DESIRED) {
            answer.set_flags(PacketFlag::RECURSION_DESIRED);
        }
        answer.questions.push(question.clone());
        let name = host_name(&question.qname).filter(|name| (self.allows)(name));
        match name {
            None => *answer.rcode_mut() = RCODE::NameError,
            Some(name) => {
                let wanted = question.qtype == QTYPE::TYPE(TYPE::A)
                    && matches!(question.qclass, QCLASS::CLASS(CLASS::IN) | QCLASS::ANY);
                if wanted {
                    match self.address(name) {
                        Some(address) => answer.answers.push(ResourceRecord::new(
                            question.qname.clone(),
                            CLASS::IN,
                            TTL,
                            RData::A(A {
                                address: address.to_bits(),
                            }),
                        )),
                        None => *answer.rcode_mut() = RCODE::ServerFailure,
                    }
                }
            }
        }
        answer.build_bytes_vec_compressed().ok()
    }

    /// The address that is `name`'s, handed out now if it has none yet, or
    /// `None` when the pool has run out.
    fn address(&mut self, name: String) -> Option<Ipv4Addr> {
        if let Some(&address) = self.addresses.get(&name) {
            return Some(address);
        }
        if self.next > self.last {
            return None;
        }
        let address = Ipv4Addr::from_bits(self.next);
        self.next += 1;
        self.addresses.insert(name.clone(), address);
        self.names.insert(address, name);
        Some(address)
    }
}

/// `name` in lowercase, without a final dot, if every label is a host name
/// label's letters, digits, hyphens, and underscores. A label that holds a
/// dot, or any other byte, would read as another name.
fn host_name(name: &simple_dns::Name) -> Option<String> {
    let labels = name.get_labels();
    if labels.is_empty() {
        return None;
    }
    let mut text = String::new();
    for label in labels {
        let bytes: &[u8] = label.as_ref();
        if !bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return None;
        }
        if !text.is_empty() {
            text.push('.');
        }
        text.push_str(&String::from_utf8_lossy(bytes).to_ascii_lowercase());
    }
    Some(text)
}
