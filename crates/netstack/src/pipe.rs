//! Forwards one connection the stack accepted to a TCP stream the caller
//! opened to its real destination. The bytes pass both ways unchanged, a
//! close from either side reaches the other as a close, and an error on
//! the stream resets the guest's connection.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};

use crate::{ConnectionId, Stack};

/// Bytes the pipe holds in each direction.
const BUFFER: usize = 16 * 1024;

pub struct Pipe {
    id: ConnectionId,
    upstream: TcpStream,
    to_upstream: Vec<u8>,
    to_guest: Vec<u8>,
    /// The guest closed its half, and the stream's write half is shut.
    guest_closed: bool,
    /// The stream read to its end.
    upstream_closed: bool,
    /// The guest got its FIN.
    closed: bool,
}

impl Pipe {
    /// A pipe between `id` and `upstream`, which must be connected and
    /// non-blocking.
    pub fn new(id: ConnectionId, upstream: TcpStream) -> Pipe {
        Pipe {
            id,
            upstream,
            to_upstream: Vec::with_capacity(BUFFER),
            to_guest: Vec::with_capacity(BUFFER),
            guest_closed: false,
            upstream_closed: false,
            closed: false,
        }
    }

    /// The stream, for the caller to wait on.
    pub fn upstream(&self) -> &TcpStream {
        &self.upstream
    }

    /// Whether the pipe waits to read the stream.
    pub fn wants_to_read(&self) -> bool {
        !self.upstream_closed && self.to_guest.is_empty()
    }

    /// Whether the pipe waits to write to the stream.
    pub fn wants_to_write(&self) -> bool {
        !self.to_upstream.is_empty()
    }

    /// Moves what it can both ways. Returns whether the connection goes on.
    /// Once it doesn't, the stack has forgotten the connection, and the
    /// stream is the caller's to drop.
    pub fn pump(&mut self, stack: &mut Stack) -> bool {
        match self.moved(stack) {
            Ok(()) if !stack.ended(self.id) => true,
            Ok(()) => {
                stack.forget(self.id);
                false
            }
            Err(_) => {
                stack.abort(self.id);
                stack.forget(self.id);
                false
            }
        }
    }

    fn moved(&mut self, stack: &mut Stack) -> io::Result<()> {
        let mut buffer = [0; BUFFER];
        loop {
            let mut moved = false;
            // From the guest to the stream.
            if self.to_upstream.is_empty() && !self.guest_closed {
                let n = stack.recv(self.id, &mut buffer);
                self.to_upstream.extend_from_slice(&buffer[..n]);
                if n == 0 && stack.received_all(self.id) {
                    self.guest_closed = true;
                    self.upstream.shutdown(Shutdown::Write)?;
                }
            }
            if !self.to_upstream.is_empty() {
                match (&self.upstream).write(&self.to_upstream) {
                    Ok(n) => {
                        self.to_upstream.drain(..n);
                        moved = n > 0;
                    }
                    Err(error) if waited(&error) => {}
                    Err(error) => return Err(error),
                }
            }
            // From the stream to the guest.
            if self.wants_to_read() {
                match (&self.upstream).read(&mut buffer) {
                    Ok(0) => self.upstream_closed = true,
                    Ok(n) => {
                        self.to_guest.extend_from_slice(&buffer[..n]);
                        moved = true;
                    }
                    Err(error) if waited(&error) => {}
                    Err(error) => return Err(error),
                }
            }
            if !self.to_guest.is_empty() {
                let n = stack.send(self.id, &self.to_guest);
                self.to_guest.drain(..n);
                moved |= n > 0;
            }
            if self.upstream_closed && self.to_guest.is_empty() && !self.closed {
                stack.close(self.id);
                self.closed = true;
            }
            if !moved {
                return Ok(());
            }
        }
    }
}

fn waited(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}
