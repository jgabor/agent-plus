//! The hidden secrets that the network process puts in requests, by
//! **Hidden** under SECRET in SPEC.md. `ap run` sends them once the process
//! has sent its authority, on the socket that its lookups use, so that a
//! value is never in the process's arguments or its environment.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};

use net::Swap;

/// A hidden secret, whose value stays out of the run process and the guest.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Hidden {
    pub name: String,
    /// The value from your host.
    pub value: Vec<u8>,
    /// What the guest holds instead, random for each run.
    pub placeholder: Vec<u8>,
    /// The hosts it is bound to.
    pub hosts: Vec<String>,
    /// Every request to its hosts must carry it, as for `AUTH key`.
    pub required: bool,
}

/// What the network process puts in requests: the headers the harness's
/// adapter lets it fill, and the hidden secrets.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Injection {
    pub headers: Vec<String>,
    pub secrets: Vec<Hidden>,
}

/// Whether a secret bound to `host` is bound to `name`: in any case, and
/// with or without a trailing dot on either.
pub fn same_host(host: &str, name: &str) -> bool {
    let host = host.strip_suffix('.').unwrap_or(host);
    let name = name.strip_suffix('.').unwrap_or(name);
    host.eq_ignore_ascii_case(name)
}

/// The bytes sent at once.
const CHUNK: usize = 16 * 1024;
/// The most bytes the network process takes.
const LONGEST: usize = 16 * 1024 * 1024;

impl Injection {
    /// Whether a hidden secret is bound to `name`.
    pub fn binds(&self, name: &str) -> bool {
        !self.swaps(name).is_empty()
    }

    /// The placeholders, and their values, of the secrets bound to `name`.
    pub fn swaps(&self, name: &str) -> Vec<Swap<'_>> {
        (self.secrets.iter())
            .filter(|secret| secret.hosts.iter().any(|host| same_host(host, name)))
            .map(|secret| Swap {
                placeholder: &secret.placeholder,
                value: &secret.value,
            })
            .collect()
    }

    /// Whether every request to `name` must carry a placeholder, and gets
    /// refused without one.
    pub fn requires(&self, name: &str) -> bool {
        (self.secrets.iter())
            .filter(|secret| secret.required)
            .any(|secret| secret.hosts.iter().any(|host| same_host(host, name)))
    }

    /// The headers, as [`net::inject`] takes them.
    pub fn header_names(&self) -> Vec<&str> {
        self.headers.iter().map(String::as_str).collect()
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        count(&mut out, self.headers.len());
        for header in &self.headers {
            field(&mut out, header.as_bytes());
        }
        count(&mut out, self.secrets.len());
        for secret in &self.secrets {
            field(&mut out, secret.name.as_bytes());
            field(&mut out, &secret.value);
            field(&mut out, &secret.placeholder);
            count(&mut out, secret.hosts.len());
            for host in &secret.hosts {
                field(&mut out, host.as_bytes());
            }
            out.push(u8::from(secret.required));
        }
        out
    }

    fn decode(mut bytes: &[u8]) -> Option<Injection> {
        let bytes = &mut bytes;
        let text = |bytes: &mut &[u8]| String::from_utf8(take(bytes)?.to_vec()).ok();
        let headers = (0..number(bytes)?)
            .map(|_| text(bytes))
            .collect::<Option<_>>()?;
        let secrets = (0..number(bytes)?)
            .map(|_| {
                Some(Hidden {
                    name: text(bytes)?,
                    value: take(bytes)?.to_vec(),
                    placeholder: take(bytes)?.to_vec(),
                    hosts: (0..number(bytes)?)
                        .map(|_| text(bytes))
                        .collect::<Option<_>>()?,
                    required: flag(bytes)?,
                })
            })
            .collect::<Option<_>>()?;
        bytes.is_empty().then_some(Injection { headers, secrets })
    }

    /// Sends this to the network process on `socket`, which keeps message
    /// boundaries: its length, then its bytes, a piece at a time.
    pub fn send(&self, socket: BorrowedFd) -> io::Result<()> {
        let bytes = self.encode();
        send(socket, &(bytes.len() as u64).to_le_bytes())?;
        bytes
            .chunks(CHUNK)
            .try_for_each(|chunk| send(socket, chunk))
    }

    /// What [`Injection::send`] sent on `socket`, which waits as its
    /// timeout says.
    pub fn receive(socket: BorrowedFd) -> io::Result<Injection> {
        let unreadable = || io::Error::other("the hidden secrets don't read");
        let mut buffer = vec![0; CHUNK];
        let length = receive(socket, &mut buffer)?;
        let length: [u8; 8] = buffer[..length].try_into().map_err(|_| unreadable())?;
        let length = u64::from_le_bytes(length) as usize;
        if length > LONGEST {
            return Err(unreadable());
        }
        let mut bytes = Vec::with_capacity(length);
        while bytes.len() < length {
            let read = receive(socket, &mut buffer)?;
            if read == 0 || bytes.len() + read > length {
                return Err(unreadable());
            }
            bytes.extend_from_slice(&buffer[..read]);
        }
        Injection::decode(&bytes).ok_or_else(unreadable)
    }
}

fn count(out: &mut Vec<u8>, count: usize) {
    out.extend((count as u32).to_le_bytes());
}

fn field(out: &mut Vec<u8>, bytes: &[u8]) {
    count(out, bytes.len());
    out.extend_from_slice(bytes);
}

fn number(bytes: &mut &[u8]) -> Option<usize> {
    let (number, rest) = bytes.split_first_chunk::<4>()?;
    *bytes = rest;
    Some(u32::from_le_bytes(*number) as usize)
}

fn flag(bytes: &mut &[u8]) -> Option<bool> {
    let (&flag, rest) = bytes.split_first()?;
    *bytes = rest;
    match flag {
        0 | 1 => Some(flag == 1),
        _ => None,
    }
}

fn take<'a>(bytes: &mut &'a [u8]) -> Option<&'a [u8]> {
    let length = number(bytes)?;
    let (taken, rest) = (bytes.len() >= length).then(|| bytes.split_at(length))?;
    *bytes = rest;
    Some(taken)
}

fn send(socket: BorrowedFd, message: &[u8]) -> io::Result<()> {
    // SAFETY: `message` is valid for the bytes the call reads.
    let sent = unsafe {
        libc::send(
            socket.as_raw_fd(),
            message.as_ptr().cast(),
            message.len(),
            libc::MSG_NOSIGNAL,
        )
    };
    match sent {
        -1 => Err(io::Error::last_os_error()),
        sent if sent as usize == message.len() => Ok(()),
        _ => Err(io::Error::other(
            "a message to the network process was cut short",
        )),
    }
}

fn receive(socket: BorrowedFd, buffer: &mut [u8]) -> io::Result<usize> {
    loop {
        // SAFETY: `buffer` has room for the bytes the call writes.
        let read = unsafe {
            libc::recv(
                socket.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                0,
            )
        };
        match read {
            -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
            -1 => return Err(io::Error::last_os_error()),
            read => return Ok(read as usize),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_injection_reads_back_as_it_was_sent() {
        let injection = Injection {
            headers: vec!["Authorization".into(), "x-api-key".into()],
            secrets: vec![
                Hidden {
                    name: "STRIPE_API_KEY".into(),
                    value: vec![b'v'; 3 * CHUNK],
                    placeholder: b"0123".to_vec(),
                    hosts: vec!["api.stripe.com".into()],
                    required: false,
                },
                Hidden {
                    hosts: vec!["api.anthropic.com".into()],
                    required: true,
                    ..Hidden::default()
                },
                Hidden::default(),
            ],
        };
        let (mine, theirs) = super::super::link().unwrap();
        use std::os::fd::AsFd;
        injection.send(mine.as_fd()).unwrap();
        assert_eq!(Injection::receive(theirs.as_fd()).unwrap(), injection);
        assert!(injection.binds("API.stripe.com."));
        assert!(!injection.binds("stripe.com"));
        assert!(!injection.requires("api.stripe.com"));
        assert!(injection.requires("API.anthropic.com."));
        // Bytes that stop short don't read.
        let bytes = injection.encode();
        assert_eq!(Injection::decode(&bytes[..bytes.len() - 1]), None);
    }
}
