//! A connection to a host with URL rules, whose every HTTP request the
//! proxy reads, as **Decryption** under NETWORK in SPEC.md says.
//!
//! The proxy has connected to the real destination, and the stack has
//! accepted the guest's end. The guest's first byte says what comes. TLS is
//! decrypted: the guest gets a certificate for the attributed name, which
//! the run's [`Authority`] signs, and the proxy opens TLS of its own to the
//! destination, which must show a certificate that the roots trust for that
//! name. Both offer only `http/1.1`. Anything else is read as plain HTTP.
//! Either way, [`NameCheck::requests`] reads each request, and the rules
//! decide its target. A request they deny never reaches the destination,
//! and the connection ends with a reset. So does traffic that isn't HTTP.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use agentfile::Scheme;
use net::{NameCheck, Request};
use netstack::{ConnectionId, Stack};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::server::Acceptor;
use rustls::{ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection};

use super::Authority;

/// Bytes moved at once in each direction.
const BUFFER: usize = 16 * 1024;
/// The most bytes a client hello may take.
const HELLO_LIMIT: usize = 64 * 1024;
/// The only protocol either side is offered.
const HTTP1: &[u8] = b"http/1.1";
pub struct Decryption {
    authority: Authority,
    client: Arc<ClientConfig>,
}

impl Decryption {
    /// Decrypts with `authority`, and trusts the roots compiled into `ap`,
    /// and `roots`.
    pub fn new(
        authority: Authority,
        roots: Vec<CertificateDer<'static>>,
    ) -> io::Result<Decryption> {
        let mut store = RootCertStore::empty();
        store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        store.add_parsable_certificates(roots);
        let provider = Arc::new(rustls_graviola::default_provider());
        let mut client = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(io::Error::other)?
            .with_root_certificates(store)
            .with_no_client_auth();
        client.alpn_protocols = vec![HTTP1.to_vec()];
        Ok(Decryption {
            authority,
            client: Arc::new(client),
        })
    }

    /// The guest's side of a connection to `name`.
    fn server(&self, name: &str) -> io::Result<Arc<ServerConfig>> {
        let leaf = self.authority.issue(name, SystemTime::now())?;
        let provider = Arc::new(rustls_graviola::default_provider());
        let mut server = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(io::Error::other)?
            .with_no_client_auth()
            .with_single_cert(vec![leaf.certificate], leaf.key)
            .map_err(io::Error::other)?;
        server.alpn_protocols = vec![HTTP1.to_vec()];
        Ok(Arc::new(server))
    }
}

/// The certificates in the file at `path`, in PEM, which `SSL_CERT_FILE`
/// names. A file that can't be read, or isn't a regular file, gives none,
/// and a block that can't be read is left out.
pub fn roots(path: &Path) -> Vec<CertificateDer<'static>> {
    use std::os::unix::fs::OpenOptionsExt;

    // A pipe or a device could hold up the open or the read, so it opens
    // without waiting, and reads only a regular file.
    let file = std::fs::File::options()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path);
    let mut pem = Vec::new();
    let read = file.and_then(|mut file| match file.metadata()?.is_file() {
        true => file.read_to_end(&mut pem),
        false => Err(io::ErrorKind::InvalidInput.into()),
    });
    if read.is_err() {
        return Vec::new();
    }
    CertificateDer::pem_slice_iter(&pem)
        .filter_map(Result::ok)
        .collect()
}

/// Decides a request on a connection: true allows it.
pub(super) type Decide<'a> = dyn FnMut(&net::Connection) -> bool + 'a;

pub(super) struct Decrypted {
    id: ConnectionId,
    upstream: TcpStream,
    name: String,
    address: IpAddr,
    port: u16,
    check: NameCheck,
    stage: Stage,
    /// Bytes for the guest that the stack has no room for yet.
    to_guest: Vec<u8>,
    /// Bytes for the destination that it hasn't taken yet.
    to_upstream: Vec<u8>,
    /// The guest closed its half.
    guest_closed: bool,
    /// The stream's write half is shut.
    shut: bool,
    /// The destination's side was told the guest ended: by TLS's
    /// `close_notify`, once the proxy's handshake is done, so what the rules
    /// allowed goes before it.
    notified: bool,
    /// The stream read to its end.
    upstream_closed: bool,
    /// The guest got its FIN.
    closed: bool,
    /// The rules allowed a request.
    allowed: bool,
}

enum Stage {
    /// Nothing has come from the guest.
    First,
    /// A TLS client hello is coming, and this many bytes of it came.
    Hello(Box<Acceptor>, usize),
    /// TLS, decrypted: the guest's side, and the destination's.
    Tls(Box<ServerConnection>, Box<ClientConnection>),
    Plain,
}

impl Decrypted {
    /// The connection `id`, which the stack accepted, to `name`, whose
    /// destination is `upstream`, connected to `to` and non-blocking.
    pub(super) fn new(
        id: ConnectionId,
        upstream: TcpStream,
        name: &str,
        to: SocketAddr,
    ) -> Decrypted {
        let name = name.to_ascii_lowercase();
        Decrypted {
            id,
            upstream,
            check: NameCheck::requests(&name),
            name: name.trim_end_matches('.').to_owned(),
            address: to.ip(),
            port: to.port(),
            stage: Stage::First,
            to_guest: Vec::new(),
            to_upstream: Vec::new(),
            guest_closed: false,
            shut: false,
            notified: false,
            upstream_closed: false,
            closed: false,
            allowed: false,
        }
    }

    /// The stream, for the caller to wait on.
    pub(super) fn upstream(&self) -> &TcpStream {
        &self.upstream
    }

    /// Whether it waits to read the stream.
    pub(super) fn wants_to_read(&self) -> bool {
        // Nothing but its handshake comes from the destination until the
        // rules allow a request.
        let open = self.allowed;
        let reading = match &self.stage {
            Stage::Tls(guest, client) => {
                client.is_handshaking() || (open && !guest.is_handshaking())
            }
            Stage::Plain => open,
            Stage::First | Stage::Hello(..) => false,
        };
        reading && !self.upstream_closed && self.to_guest.is_empty()
    }

    /// Whether it waits to write to the stream.
    pub(super) fn wants_to_write(&self) -> bool {
        !self.to_upstream.is_empty()
            || matches!(&self.stage, Stage::Tls(_, client) if client.wants_write())
    }

    /// Moves what it can both ways, decrypting with `decryption`, without
    /// which TLS is refused, and asks `decide`
    /// about each request. Returns whether the connection goes on. Once it
    /// doesn't, the stack has forgotten the connection.
    pub(super) fn pump(
        &mut self,
        stack: &mut Stack,
        decryption: Option<&Decryption>,
        decide: &mut Decide,
    ) -> bool {
        match self.moved(stack, decryption, decide) {
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

    fn moved(
        &mut self,
        stack: &mut Stack,
        decryption: Option<&Decryption>,
        decide: &mut Decide,
    ) -> io::Result<()> {
        let mut buffer = [0; BUFFER];
        loop {
            let mut moved = false;
            // From the guest.
            if self.takes_from_guest() {
                let n = stack.recv(self.id, &mut buffer);
                if n > 0 {
                    moved = true;
                    self.guest_sent(&buffer[..n], decryption, decide)?;
                } else if stack.received_all(self.id) {
                    self.guest_ended()?;
                }
            }
            // To the destination.
            if self.guest_closed && !self.notified {
                match &mut self.stage {
                    Stage::Tls(_, client) if client.is_handshaking() => {}
                    Stage::Tls(_, client) => {
                        client.send_close_notify();
                        self.notified = true;
                    }
                    _ => self.notified = true,
                }
            }
            if let Stage::Tls(_, client) = &mut self.stage {
                while self.to_upstream.len() < BUFFER && client.wants_write() {
                    client.write_tls(&mut self.to_upstream)?;
                }
            }
            if !self.to_upstream.is_empty() {
                match (&self.upstream).write(&self.to_upstream) {
                    Ok(n) => {
                        self.to_upstream.drain(..n);
                        moved |= n > 0;
                    }
                    Err(error) if waited(&error) => {}
                    Err(error) => return Err(error),
                }
            }
            let flushed = self.to_upstream.is_empty()
                && !matches!(&self.stage, Stage::Tls(_, client) if client.wants_write());
            // A guest that ends before the rules allow a request gets nothing
            // from the destination, so the connection ends here.
            if self.guest_closed && !self.allowed {
                return Err(refused("the guest ended before any request"));
            }
            if self.guest_closed && self.notified && flushed && !self.shut {
                self.upstream.shutdown(Shutdown::Write)?;
                self.shut = true;
            }
            // From the destination.
            if self.wants_to_read() {
                match (&self.upstream).read(&mut buffer) {
                    // A destination that ends TLS sent its `close_notify`, which
                    // the guest got. One that only closes TCP cut the guest
                    // off, so the guest's side ends the same way.
                    Ok(0) if matches!(&self.stage, Stage::Tls(_, client) if client.is_handshaking()) =>
                    {
                        return Err(io::Error::other("the destination ended its handshake"));
                    }
                    Ok(0) => self.upstream_closed = true,
                    Ok(n) => {
                        moved = true;
                        self.upstream_sent(&buffer[..n])?;
                    }
                    Err(error) if waited(&error) => {}
                    Err(error) => return Err(error),
                }
            }
            // To the guest.
            if let Stage::Tls(guest, _) = &mut self.stage {
                while guest.wants_write() {
                    guest.write_tls(&mut self.to_guest)?;
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

    /// Whether it reads more from the guest: only once what came before has
    /// gone on, and, once the rules allowed a request, only once the
    /// destination's handshake is done. Until then, the check holds at most
    /// a head, so a denied request or a close is seen however long the
    /// destination takes.
    fn takes_from_guest(&self) -> bool {
        let waiting = match &self.stage {
            Stage::Tls(guest, client) => {
                client.wants_write()
                    || (client.is_handshaking() && !guest.is_handshaking() && self.allowed)
            }
            _ => false,
        };
        !self.guest_closed && self.to_upstream.is_empty() && !waiting
    }

    fn guest_sent(
        &mut self,
        mut input: &[u8],
        decryption: Option<&Decryption>,
        decide: &mut Decide,
    ) -> io::Result<()> {
        if let Stage::First = self.stage {
            self.stage = match input[0] {
                0x16 => Stage::Hello(Box::default(), 0),
                _ => Stage::Plain,
            };
        }
        let (name, address, port) = (self.name.clone(), self.address, self.port);
        let mut allows = |scheme, target: &str| {
            decide(&net::Connection {
                name: Some(&name),
                address,
                port,
                request: Some(Request { scheme, target }),
            })
        };
        match &mut self.stage {
            Stage::First => unreachable!(),
            Stage::Plain => {
                let check = &mut self.check;
                let before = self.to_upstream.len();
                let fed = check.feed_deciding(input, &mut self.to_upstream, &mut |target| {
                    allows(Scheme::Http, target)
                });
                self.allowed |= self.to_upstream.len() > before;
                fed.map_err(|_| refused("a request the rules deny"))
            }
            Stage::Hello(acceptor, read) => {
                *read += input.len();
                while !input.is_empty() {
                    if acceptor.read_tls(&mut input)? == 0 {
                        return Err(refused("a client hello that doesn't end"));
                    }
                }
                let accepted = match acceptor.accept() {
                    Ok(None) if *read > HELLO_LIMIT => return Err(refused("a long client hello")),
                    Ok(None) => return Ok(()),
                    Ok(Some(accepted)) => accepted,
                    Err((error, _)) => return Err(io::Error::other(error)),
                };
                let server = accepted.client_hello().server_name().map(str::to_owned);
                if server.is_some_and(|server| {
                    !server
                        .trim_end_matches('.')
                        .eq_ignore_ascii_case(&self.name)
                }) {
                    return Err(refused("a server name that disagrees"));
                }
                let decryption = decryption.ok_or_else(|| refused("TLS it can't decrypt"))?;
                let config = decryption.server(&self.name)?;
                let mut guest = (accepted.into_connection(config))
                    .map_err(|(error, _)| io::Error::other(error))?;
                guest.set_buffer_limit(None);
                let name = ServerName::try_from(self.name.clone()).map_err(io::Error::other)?;
                let mut client = ClientConnection::new(Arc::clone(&decryption.client), name)
                    .map_err(io::Error::other)?;
                client.set_buffer_limit(None);
                self.stage = Stage::Tls(Box::new(guest), Box::new(client));
                self.decrypted(&mut allows)
            }
            Stage::Tls(..) => {
                while !input.is_empty() {
                    let Stage::Tls(guest, _) = &mut self.stage else {
                        unreachable!()
                    };
                    // Nothing more is read once the guest has ended TLS.
                    if guest.read_tls(&mut input)? == 0 {
                        break;
                    }
                    guest.process_new_packets().map_err(io::Error::other)?;
                    self.decrypted(&mut allows)?;
                }
                Ok(())
            }
        }
    }

    /// Passes what the guest's side decrypted, as far as the rules allow.
    fn decrypted(&mut self, allows: &mut dyn FnMut(Scheme, &str) -> bool) -> io::Result<()> {
        let Stage::Tls(guest, client) = &mut self.stage else {
            return Ok(());
        };
        let mut buffer = [0; BUFFER];
        let mut passed = Vec::new();
        loop {
            match guest.reader().read(&mut buffer) {
                Ok(0) if self.check.holding() => {
                    return Err(refused("the guest closed before its check"));
                }
                Ok(0) => {
                    self.guest_closed = true;
                    break;
                }
                Ok(n) => {
                    let mut decide = |target: &str| allows(Scheme::Https, target);
                    let fed = self
                        .check
                        .feed_deciding(&buffer[..n], &mut passed, &mut decide);
                    client.writer().write_all(&passed)?;
                    self.allowed |= !passed.is_empty();
                    passed.clear();
                    fed.map_err(|_| refused("a request the rules deny"))?;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Passes what came from the destination to the guest.
    fn upstream_sent(&mut self, mut input: &[u8]) -> io::Result<()> {
        let Stage::Tls(guest, client) = &mut self.stage else {
            self.to_guest.extend_from_slice(input);
            return Ok(());
        };
        let mut buffer = [0; BUFFER];
        while !input.is_empty() {
            // Nothing more is read once the destination has ended TLS.
            if client.read_tls(&mut input)? == 0 {
                self.upstream_closed = true;
                break;
            }
            client.process_new_packets().map_err(io::Error::other)?;
            loop {
                match client.reader().read(&mut buffer) {
                    Ok(0) => {
                        guest.send_close_notify();
                        break;
                    }
                    // A server that speaks before a request is allowed isn't
                    // HTTP.
                    Ok(_) if !self.allowed => return Err(refused("a server that speaks first")),
                    Ok(n) => guest.writer().write_all(&buffer[..n])?,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(())
    }

    /// The guest closed its half.
    fn guest_ended(&mut self) -> io::Result<()> {
        let unchecked = match &self.stage {
            Stage::First | Stage::Hello(..) => true,
            Stage::Tls(guest, _) => guest.is_handshaking(),
            Stage::Plain => false,
        };
        if unchecked || self.check.holding() {
            return Err(refused("the guest closed before its check"));
        }
        self.guest_closed = true;
        Ok(())
    }
}

fn refused(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, why.to_owned())
}

fn waited(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}
