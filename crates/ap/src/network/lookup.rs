//! The lookups that `ap run` makes for the network process, which can't
//! read your host's resolver configuration once it is confined. `ap run`
//! looks each name up through the host's own resolver at the time it is
//! asked, so a run follows the host when it changes networks.
//!
//! Each question is one message: an id of four bytes, then the name. Each
//! answer is one message: the question's id, then each address, as a byte
//! for its family, 4 or 6, and its bytes. A name that doesn't resolve gets
//! no addresses.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixDatagram;
use std::sync::Arc;
use std::thread;

/// The longest name a question holds.
const LONGEST_NAME: usize = 253;
/// The threads that answer at once.
const WORKERS: usize = 4;
/// The most addresses an answer holds.
const MOST_ADDRESSES: usize = 16;
/// The longest message either way.
pub(super) const LONGEST: usize = 4 + MOST_ADDRESSES * 17;

/// Answers the network process's questions on `socket`, the
/// [`super::Started::lookups`] of its start, until the network process
/// ends. A few threads answer at once, each one question at a time, so a
/// slow name doesn't hold up the others, and the network process can't
/// make `ap run` start more.
pub fn answer_lookups(socket: OwnedFd) {
    let socket = Arc::new(UnixDatagram::from(socket));
    for _ in 0..WORKERS {
        let socket = Arc::clone(&socket);
        let started = thread::Builder::new().spawn(move || {
            let mut buffer = [0; 4 + LONGEST_NAME + 1];
            while let Ok(len @ 1..) = socket.recv(&mut buffer) {
                if let Some((id, name)) = read_question(&buffer[..len]) {
                    let _ = socket.send(&write_answer(id, &look_up(&name)));
                }
            }
        });
        // With no thread to answer, a connection to a name fails, as it
        // would with no resolver.
        drop(started);
    }
}

/// The addresses your host's resolver has for `name`, in its order, each
/// once.
fn look_up(name: &str) -> Vec<IpAddr> {
    let mut found: Vec<IpAddr> = Vec::new();
    for address in (name, 0).to_socket_addrs().into_iter().flatten() {
        if !found.contains(&address.ip()) && found.len() < MOST_ADDRESSES {
            found.push(address.ip());
        }
    }
    found
}

pub(super) fn write_question(id: u32, name: &str) -> Option<Vec<u8>> {
    (name.len() <= LONGEST_NAME).then(|| [&id.to_be_bytes()[..], name.as_bytes()].concat())
}

fn read_question(message: &[u8]) -> Option<(u32, String)> {
    let (id, name) = message.split_at_checked(4)?;
    let name = std::str::from_utf8(name).ok()?;
    (name.len() <= LONGEST_NAME && !name.is_empty())
        .then(|| (u32::from_be_bytes(id.try_into().unwrap()), name.to_owned()))
}

fn write_answer(id: u32, addresses: &[IpAddr]) -> Vec<u8> {
    let mut message = id.to_be_bytes().to_vec();
    for address in addresses.iter().take(MOST_ADDRESSES) {
        match address {
            IpAddr::V4(v4) => {
                message.push(4);
                message.extend(v4.octets());
            }
            IpAddr::V6(v6) => {
                message.push(6);
                message.extend(v6.octets());
            }
        }
    }
    message
}

pub(super) fn read_answer(message: &[u8]) -> Option<(u32, Vec<IpAddr>)> {
    let (id, mut rest) = message.split_at_checked(4)?;
    let mut addresses = Vec::new();
    while let Some((&family, after)) = rest.split_first() {
        let (address, after) = match family {
            4 => {
                let (bytes, after) = after.split_at_checked(4)?;
                let bytes: [u8; 4] = bytes.try_into().ok()?;
                (IpAddr::V4(Ipv4Addr::from(bytes)), after)
            }
            6 => {
                let (bytes, after) = after.split_at_checked(16)?;
                let bytes: [u8; 16] = bytes.try_into().ok()?;
                (IpAddr::V6(Ipv6Addr::from(bytes)), after)
            }
            _ => return None,
        };
        addresses.push(address);
        rest = after;
    }
    Some((u32::from_be_bytes(id.try_into().unwrap()), addresses))
}

/// The network process's side: it asks, and takes answers as they come.
pub(super) struct Asker {
    socket: UnixDatagram,
    next: u32,
}

impl Asker {
    pub(super) fn new(socket: UnixDatagram) -> Asker {
        Asker { socket, next: 0 }
    }

    pub(super) fn fd(&self) -> i32 {
        self.socket.as_raw_fd()
    }

    /// Asks for `name`, and returns the id its answer will carry.
    pub(super) fn ask(&mut self, name: &str) -> io::Result<u32> {
        let id = self.next;
        self.next = self.next.wrapping_add(1);
        let question = write_question(id, name)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "name too long"))?;
        super::proxy::send(self.socket.as_raw_fd(), &question)?;
        Ok(id)
    }

    /// The next answer, if one has come.
    pub(super) fn answer(&mut self) -> io::Result<Option<(u32, Vec<IpAddr>)>> {
        let mut buffer = [0; LONGEST];
        match super::proxy::recv(self.socket.as_raw_fd(), &mut buffer)? {
            None => Ok(None),
            Some(0) => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "`ap run` stopped answering lookups",
            )),
            Some(len) => Ok(read_answer(&buffer[..len])),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_question_and_an_answer_read_back() {
        let question = write_question(7, "api.anthropic.com").unwrap();
        assert_eq!(
            read_question(&question),
            Some((7, "api.anthropic.com".to_owned()))
        );
        assert_eq!(write_question(1, &"a".repeat(254)), None);
        assert_eq!(read_question(&[0, 0, 0, 1]), None);
        let addresses: Vec<IpAddr> =
            vec!["192.0.2.1".parse().unwrap(), "2001:db8::1".parse().unwrap()];
        assert_eq!(
            read_answer(&write_answer(9, &addresses)),
            Some((9, addresses))
        );
        assert_eq!(read_answer(&write_answer(3, &[])), Some((3, vec![])));
        assert_eq!(read_answer(&[0, 0, 0, 1, 4, 1, 2]), None);
        assert_eq!(read_answer(&[0, 0, 0, 1, 5]), None);
    }

    #[test]
    fn a_lookup_goes_through_the_host_s_resolver() {
        let found = look_up("localhost");
        assert!(
            found.iter().all(|address| address.is_loopback()) && !found.is_empty(),
            "{found:?}"
        );
        assert_eq!(
            look_up("name.that.does.not.exist.invalid"),
            Vec::<IpAddr>::new()
        );
    }
}
