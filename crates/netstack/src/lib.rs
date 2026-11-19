//! The network process's TCP/IP stack: one Ethernet interface on the guest's
//! link that ends every TCP connection, and takes every UDP datagram, to any
//! address and port.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Instant;
use smoltcp as _;

pub struct Config {
    pub mac: [u8; 6],
    pub ipv4: (Ipv4Addr, u8),
    pub ipv6: (Ipv6Addr, u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConnectionId(usize);

pub struct Connection {
    pub id: ConnectionId,
    pub source: SocketAddr,
    pub destination: SocketAddr,
}

pub struct Datagram {
    pub source: SocketAddr,
    pub destination: SocketAddr,
    pub payload: Vec<u8>,
}

pub struct Stack;

impl Stack {
    pub fn new(_config: Config) -> Stack {
        Stack
    }
    pub fn receive(&mut self, _frame: &[u8], _now: Instant) {}
    pub fn poll(&mut self, _now: Instant) {}
    pub fn transmit(&mut self) -> Option<Vec<u8>> {
        None
    }
    pub fn accept(&mut self) -> Option<Connection> {
        None
    }
    pub fn recv(&mut self, _id: ConnectionId, _buf: &mut [u8]) -> usize {
        0
    }
    pub fn send(&mut self, _id: ConnectionId, _bytes: &[u8]) -> usize {
        0
    }
    pub fn recv_datagram(&mut self) -> Option<Datagram> {
        None
    }
}
