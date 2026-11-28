//! The network process's proxy on a host with URL rules, by **Decryption**
//! under NETWORK in SPEC.md, on a hidden secret's bound host, by **Hidden**
//! under SECRET, and on a UDP flow, by **UDP** under NETWORK. The proxy
//! serves a link, as in a run. A second `smoltcp` interface plays the guest
//! on the other end, and reaches fake destinations on this host's loopback
//! as `host.ap.internal`. Each fake destination notes the request line of
//! every request that reaches it. Over HTTP/2, the `h2` crate is the guest's
//! client and the fake destination's server, on either end of the proxy.

#![cfg(target_os = "linux")]

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::UnixDatagram;
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::task::Poll;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use agentfile::{Network, parse};
use ap::network::{Authority, Decryption, Hidden, Injection, Proxy};
use guest::net;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection};
use smoltcp::iface::{Config as IfaceConfig, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr};
use tokio::io::{AsyncRead, AsyncWriteExt, DuplexStream, ReadBuf, duplex};

const NAME: &str = "host.ap.internal";
/// How long the guest waits for an answer.
const WAIT: Duration = Duration::from_secs(10);

/// The guest's end of the link, a socket that keeps message boundaries.
struct Link(UnixDatagram);

struct RxToken(Vec<u8>);
struct TxToken<'a>(&'a UnixDatagram);

impl phy::RxToken for RxToken {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl phy::TxToken for TxToken<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut frame = vec![0; len];
        let result = f(&mut frame);
        let _ = self.0.send(&frame);
        result
    }
}

impl Device for Link {
    type RxToken<'a> = RxToken;
    type TxToken<'a> = TxToken<'a>;

    fn receive(&mut self, _: SmolInstant) -> Option<(RxToken, TxToken<'_>)> {
        let mut frame = vec![0; 1600];
        let n = self.0.recv(&mut frame).ok()?;
        frame.truncate(n);
        Some((RxToken(frame), TxToken(&self.0)))
    }

    fn transmit(&mut self, _: SmolInstant) -> Option<TxToken<'_>> {
        Some(TxToken(&self.0))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = 1514;
        caps
    }
}

/// The guest, on a link to a proxy that serves it on a thread of its own.
struct Guest {
    start: Instant,
    iface: Interface,
    sockets: SocketSet<'static>,
    link: Link,
    /// The address the resolver gave `host.ap.internal`.
    host: Ipv4Addr,
    /// The run's certificate authority.
    authority: CertificateDer<'static>,
    port: u16,
}

/// A pair of connected sockets that keep message boundaries.
fn link() -> (OwnedFd, OwnedFd) {
    let mut fds = [0; 2];
    // SAFETY: `fds` has room for the two descriptors the call writes.
    let made = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    };
    assert_eq!(made, 0, "{}", io::Error::last_os_error());
    // SAFETY: both descriptors are new, and nothing else owns them.
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

/// A query for the `A` record of `name`.
fn query(name: &str) -> Vec<u8> {
    let mut query = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in name.split('.') {
        query.push(label.len() as u8);
        query.extend(label.as_bytes());
    }
    query.extend([0, 0, 1, 0, 1]);
    query
}

impl Guest {
    /// A guest whose proxy decides by the `NETWORK` lines of `lines`, and
    /// trusts `roots` as well as the roots compiled into `ap`. `port` is
    /// the fake destination's.
    fn new(lines: &str, roots: Vec<CertificateDer<'static>>, port: u16) -> Guest {
        Guest::injecting(lines, roots, port, Injection::default())
    }

    /// A guest as [`Guest::new`]'s, whose proxy puts the secrets of
    /// `injection` in requests.
    fn injecting(
        lines: &str,
        roots: Vec<CertificateDer<'static>>,
        port: u16,
        injection: Injection,
    ) -> Guest {
        Guest::with_harness(lines, roots, port, injection, Vec::new())
    }

    /// A guest as [`Guest::injecting`]'s, whose harness has `harness` as its
    /// hosts.
    fn with_harness(
        lines: &str,
        roots: Vec<CertificateDer<'static>>,
        port: u16,
        injection: Injection,
        harness: Vec<String>,
    ) -> Guest {
        Guest::configured(lines, roots, port, harness, move |proxy| {
            proxy.injecting(injection)
        })
    }

    /// A guest as [`Guest::with_harness`]'s, whose proxy `configure` sets
    /// up past its rules and decryption.
    fn configured(
        lines: &str,
        roots: Vec<CertificateDer<'static>>,
        port: u16,
        harness: Vec<String>,
        configure: impl FnOnce(Proxy) -> Proxy + Send + 'static,
    ) -> Guest {
        let (mine, theirs) = link();
        let (sent, got) = mpsc::channel();
        let lines = lines.to_owned();
        thread::spawn(move || {
            let instructions = parse(&format!("AGENT claude\n{lines}")).unwrap();
            let network = Network::read(&instructions).unwrap();
            let mut args: Vec<std::ffi::OsString> = (harness.iter()).map(Into::into).collect();
            args.push("--".into());
            args.extend((lines.lines()).map(|line| line.strip_prefix("NETWORK ").unwrap().into()));
            let mut resolver = ap::network::resolver(&args).unwrap();
            let answer = resolver.answer(&query(NAME)).unwrap();
            let host: [u8; 4] = answer[answer.len() - 4..].try_into().unwrap();
            let authority = Authority::generate(SystemTime::now()).unwrap();
            sent.send((Ipv4Addr::from(host), authority.certificate().clone()))
                .unwrap();
            let decryption = Decryption::new(authority, roots).unwrap();
            // Your host's name asks for no lookup.
            let (lookups, _) = UnixDatagram::pair().unwrap();
            let mut proxy = configure(Proxy::new(network, harness, lookups).decrypting(decryption));
            let mut stack = ap::network::stack();
            // It ends once the guest's end closes.
            let _ = proxy.serve(&File::from(theirs), &mut stack, &mut resolver);
        });
        let (host, authority) = got.recv().unwrap();
        let socket = UnixDatagram::from(mine);
        socket.set_nonblocking(true).unwrap();
        let mut link = Link(socket);
        let config = IfaceConfig::new(HardwareAddress::Ethernet(EthernetAddress(net::MAC)));
        let mut iface = Interface::new(config, &mut link, SmolInstant::from_millis(0));
        iface.update_ip_addrs(|addrs| {
            let address = IpCidr::new(IpAddress::Ipv4(net::IPV4), net::IPV4_PREFIX);
            addrs.push(address).unwrap();
        });
        (iface.routes_mut())
            .add_default_ipv4_route(net::IPV4_GATEWAY)
            .unwrap();
        Guest {
            start: Instant::now(),
            iface,
            sockets: SocketSet::new(Vec::new()),
            link,
            host,
            authority,
            port,
        }
    }

    fn poll(&mut self) {
        let millis = self.start.elapsed().as_millis() as i64;
        let now = SmolInstant::from_millis(millis);
        self.iface.poll(now, &mut self.link, &mut self.sockets);
    }

    /// Opens a TCP connection to `host.ap.internal` on the destination's
    /// port.
    fn connect(&mut self) -> SocketHandle {
        let buffer = || tcp::SocketBuffer::new(vec![0; 64 * 1024]);
        let mut socket = tcp::Socket::new(buffer(), buffer());
        let to = SocketAddr::new(IpAddr::V4(self.host), self.port);
        let local = 49152 + self.sockets.iter().count() as u16;
        socket.connect(self.iface.context(), to, local).unwrap();
        self.sockets.add(socket)
    }

    /// A TLS client for `host.ap.internal`, which trusts only `root`, and
    /// offers HTTP/2 and HTTP/1.1.
    fn tls(root: &CertificateDer<'static>) -> ClientConnection {
        Guest::tls_offering(root, &[b"h2", b"http/1.1"])
    }

    /// A TLS client as [`Guest::tls`]'s, which offers only HTTP/1.1.
    fn http1(root: &CertificateDer<'static>) -> ClientConnection {
        Guest::tls_offering(root, &[b"http/1.1"])
    }

    /// A TLS client for `host.ap.internal`, which trusts only `root`, and
    /// offers `protocols`.
    fn tls_offering(root: &CertificateDer<'static>, protocols: &[&[u8]]) -> ClientConnection {
        Guest::tls_for(root, protocols, NAME)
    }

    /// A TLS client for `server`, which trusts only `root`, and offers
    /// `protocols`.
    fn tls_for(
        root: &CertificateDer<'static>,
        protocols: &[&[u8]],
        server: &str,
    ) -> ClientConnection {
        Guest::tls_naming(root, protocols, server, true)
    }

    /// A TLS client as [`Guest::tls`]'s, whose hello names no server.
    fn tls_unnamed(root: &CertificateDer<'static>) -> ClientConnection {
        Guest::tls_naming(root, &[b"h2", b"http/1.1"], NAME, false)
    }

    /// A TLS client for `server`, which trusts only `root`, offers
    /// `protocols`, and names `server` in its hello if `sni`.
    fn tls_naming(
        root: &CertificateDer<'static>,
        protocols: &[&[u8]],
        server: &str,
        sni: bool,
    ) -> ClientConnection {
        let mut roots = RootCertStore::empty();
        roots.add(root.clone()).unwrap();
        let provider = Arc::new(rustls_graviola::default_provider());
        let mut config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = protocols.iter().map(|protocol| protocol.to_vec()).collect();
        config.enable_sni = sni;
        let name = ServerName::try_from(server.to_owned()).unwrap();
        ClientConnection::new(Arc::new(config), name).unwrap()
    }

    /// Sends `bytes` on `handle`, through `tls` if it is given, and returns
    /// what comes back once it is a whole response. An error says how the
    /// connection ended first.
    fn exchange(
        &mut self,
        handle: SocketHandle,
        mut tls: Option<&mut ClientConnection>,
        bytes: &[u8],
    ) -> Result<String, String> {
        let deadline = Instant::now() + WAIT;
        let mut unsent = match &mut tls {
            Some(tls) => {
                tls.writer().write_all(bytes).unwrap();
                Vec::new()
            }
            None => bytes.to_vec(),
        };
        let mut received = Vec::new();
        let mut buffer = [0; 16 * 1024];
        while Instant::now() < deadline {
            self.poll();
            let socket = self.sockets.get_mut::<tcp::Socket>(handle);
            if let Some(tls) = &mut tls {
                while tls.wants_write() {
                    tls.write_tls(&mut unsent).unwrap();
                }
            }
            if socket.can_send() && !unsent.is_empty() {
                let n = socket.send_slice(&unsent).unwrap();
                unsent.drain(..n);
            }
            while socket.can_recv() {
                let n = socket.recv_slice(&mut buffer).unwrap();
                match &mut tls {
                    Some(tls) => {
                        tls.read_tls(&mut &buffer[..n]).unwrap();
                        tls.process_new_packets()
                            .map_err(|error| error.to_string())?;
                        let _ = tls.reader().read_to_end(&mut received);
                    }
                    None => received.extend_from_slice(&buffer[..n]),
                }
            }
            if whole(&received) {
                return Ok(String::from_utf8_lossy(&received).into_owned());
            }
            match socket.state() {
                tcp::State::Closed if received.is_empty() => return Err("reset".to_owned()),
                tcp::State::Closed | tcp::State::CloseWait => {
                    return Err(format!("closed after {}", received.escape_ascii()));
                }
                _ => {}
            }
            thread::sleep(Duration::from_millis(1));
        }
        Err("timed out".to_owned())
    }
}

/// Whether `bytes` hold a whole response, whose length its head gives.
fn whole(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes);
    let Some((head, body)) = text.split_once("\r\n\r\n") else {
        return false;
    };
    let length = (head.lines())
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .and_then(|length| length.parse::<usize>().ok());
    length.is_some_and(|length| body.len() >= length)
}

fn get(path: &str) -> Vec<u8> {
    format!("GET {path} HTTP/1.1\r\nHost: {NAME}\r\n\r\n").into_bytes()
}

/// The request lines that a fake destination received, and, over TLS, the
/// protocol it agreed with each connection.
type Seen = Arc<Mutex<Vec<String>>>;

/// A fake destination on this host's loopback, over TLS with `tls`, or
/// plain. It answers each request with its target.
fn destination(tls: Option<Arc<ServerConfig>>) -> (u16, Seen) {
    destination_after(tls, Duration::ZERO)
}

/// A fake destination as [`destination`]'s, which waits `delay` on each
/// connection before it starts.
fn destination_after(tls: Option<Arc<ServerConfig>>, delay: Duration) -> (u16, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Seen::default();
    let noted = Arc::clone(&seen);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let (tls, noted) = (tls.clone(), Arc::clone(&noted));
            thread::spawn(move || {
                thread::sleep(delay);
                stream.set_read_timeout(Some(WAIT)).unwrap();
                match tls {
                    Some(config) => {
                        let server = ServerConnection::new(config).unwrap();
                        let mut stream = rustls::StreamOwned::new(server, stream);
                        answer(&mut stream, &noted, |stream| {
                            let agreed = stream.conn.alpn_protocol().unwrap_or(b"none");
                            String::from_utf8_lossy(agreed).into_owned()
                        });
                    }
                    None => {
                        let mut stream = stream;
                        answer(&mut stream, &noted, |_| "plain".to_owned());
                    }
                }
            });
        }
    });
    (port, seen)
}

/// Answers each request on `stream`, and notes its request line, and what
/// `how` says of the stream. Bytes that end before a whole head are noted
/// too, as `partial`.
fn answer<S: Read + Write>(stream: &mut S, noted: &Seen, how: impl Fn(&S) -> String) {
    let mut reader = BufReader::new(stream);
    loop {
        let mut head = String::new();
        let mut line = String::new();
        while line != "\r\n" {
            line.clear();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                if !head.is_empty() {
                    noted
                        .lock()
                        .unwrap()
                        .push(format!("partial {}", head.escape_debug()));
                }
                return;
            }
            head.push_str(&line);
        }
        let first = head.lines().next().unwrap_or_default().to_owned();
        let shown = format!("{first} ({})", how(reader.get_ref()));
        noted.lock().unwrap().push(shown);
        let target = first.split(' ').nth(1).unwrap_or_default().to_owned();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{target}",
            target.len()
        );
        let stream = reader.get_mut();
        if stream.write_all(response.as_bytes()).is_err() || stream.flush().is_err() {
            return;
        }
    }
}

/// What the fake destination saw, once nothing more came for a while.
fn settled(seen: &Seen) -> Vec<String> {
    thread::sleep(Duration::from_millis(300));
    seen.lock().unwrap().clone()
}

/// A certificate authority for fake destinations, and a TLS server for
/// `host.ap.internal` with a certificate it signed, which agrees HTTP/2 or
/// HTTP/1.1.
fn server() -> (CertificateDer<'static>, Arc<ServerConfig>) {
    server_offering(&[b"h2", b"http/1.1"])
}

/// A certificate authority and a TLS server as [`server`]'s, which agrees
/// only `protocols`.
fn server_offering(protocols: &[&[u8]]) -> (CertificateDer<'static>, Arc<ServerConfig>) {
    let now = SystemTime::now();
    let authority = Authority::generate(now).unwrap();
    let leaf = authority.issue(NAME, now).unwrap();
    let provider = Arc::new(rustls_graviola::default_provider());
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![leaf.certificate], leaf.key)
        .unwrap();
    config.alpn_protocols = protocols.iter().map(|protocol| protocol.to_vec()).collect();
    (authority.certificate().clone(), Arc::new(config))
}

/// The length of each body an HTTP/2 request sends: past the initial
/// window of 65,535, so the guest sends it as the destination's window
/// opens, which passes unread.
const BODY: usize = 200_000;
/// The bytes each in-memory pipe to an `h2` end holds.
const PIPE: usize = 1 << 20;

/// The body that a request for `path` sends.
fn body_for(path: &str) -> Vec<u8> {
    path.bytes().cycle().take(BODY).collect()
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
}

/// Moves `received`, which came from the other end of `tls`, to `pipe`,
/// which an `h2` end reads, and what that end wrote to `tls`, whose records
/// go on `unsent`. Returns whether `pipe` is still open, or how TLS ended.
async fn relay(
    pipe: &mut DuplexStream,
    tls: &mut rustls::Connection,
    mut received: &[u8],
    unsent: &mut Vec<u8>,
) -> Result<bool, String> {
    // Plaintext is read as each record is, since rustls holds only so much.
    let mut plain = Vec::new();
    let ended = loop {
        match tls.reader().read_to_end(&mut plain) {
            Ok(_) => break true,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error.to_string()),
        }
        if received.is_empty() {
            break false;
        }
        tls.read_tls(&mut received)
            .map_err(|error| error.to_string())?;
        tls.process_new_packets()
            .map_err(|error| error.to_string())?;
    };
    pipe.write_all(&plain)
        .await
        .map_err(|error| error.to_string())?;
    if ended {
        return Err("closed".to_owned());
    }
    // What the `h2` end wrote, without waiting for more.
    let mut buffer = vec![0; 64 * 1024];
    let mut read = ReadBuf::new(&mut buffer);
    let polled = std::future::poll_fn(|context| {
        Poll::Ready(Pin::new(&mut *pipe).poll_read(context, &mut read))
    })
    .await;
    let open = match polled {
        Poll::Ready(Ok(())) if read.filled().is_empty() => {
            tls.send_close_notify();
            false
        }
        Poll::Ready(Ok(())) => {
            tls.writer().write_all(read.filled()).unwrap();
            true
        }
        Poll::Ready(Err(error)) => return Err(error.to_string()),
        Poll::Pending => true,
    };
    while tls.wants_write() {
        tls.write_tls(unsent).unwrap();
    }
    Ok(open)
}

/// What the requests on an HTTP/2 connection got, and how it ended.
struct Asked {
    /// Each answer's body, or why there was none, in the order asked.
    answers: Vec<Result<String, String>>,
    /// The protocol the guest agreed.
    protocol: Option<Vec<u8>>,
    /// The connection was reset.
    reset: bool,
}

impl Guest {
    /// Sends each of `batches` from an `h2` client on `handle`, through
    /// `tls`: each request of a batch at once, with the body that
    /// [`body_for`] gives its path, and the next batch once every request
    /// of this one has its answer.
    fn ask_h2(
        &mut self,
        handle: SocketHandle,
        tls: ClientConnection,
        batches: Vec<Vec<String>>,
    ) -> Asked {
        runtime().block_on(async {
            let (io, pipe) = duplex(PIPE);
            let asking = tokio::spawn(async move {
                let handshake = (h2::client::Builder::new())
                    .handshake::<_, &'static [u8]>(io)
                    .await;
                let (client, connection) = match handshake {
                    Ok(handshake) => handshake,
                    Err(error) => return vec![Err(error.to_string())],
                };
                tokio::spawn(async move {
                    let _ = connection.await;
                });
                let mut answers = Vec::new();
                for batch in batches {
                    let asked: Vec<_> = (batch.into_iter())
                        .map(|url| tokio::spawn(ask(client.clone(), url)))
                        .collect();
                    for answer in asked {
                        answers.push(answer.await.unwrap().map_err(|error| error.to_string()));
                    }
                }
                answers
            });
            let mut pipe = Some(pipe);
            let mut tls = rustls::Connection::Client(tls);
            tls.set_buffer_limit(None);
            let mut unsent = Vec::new();
            let mut buffer = vec![0; 64 * 1024];
            let mut reset = false;
            let deadline = Instant::now() + WAIT;
            while !asking.is_finished() && Instant::now() < deadline {
                self.poll();
                let socket = self.sockets.get_mut::<tcp::Socket>(handle);
                if socket.can_send() && !unsent.is_empty() {
                    let n = socket.send_slice(&unsent).unwrap();
                    unsent.drain(..n);
                }
                let mut received = Vec::new();
                while socket.can_recv() {
                    let n = socket.recv_slice(&mut buffer).unwrap();
                    received.extend_from_slice(&buffer[..n]);
                }
                // The guest never closes first, so only a reset closes it.
                reset |= socket.state() == tcp::State::Closed;
                if let Some(open) = &mut pipe {
                    let goes_on = relay(open, &mut tls, &received, &mut unsent).await;
                    // Without its pipe, the client's connection ends.
                    if reset || goes_on != Ok(true) {
                        pipe = None;
                    }
                }
                tokio::task::yield_now().await;
                thread::sleep(Duration::from_millis(1));
            }
            let answers = match asking.is_finished() {
                true => asking.await.unwrap(),
                false => vec![Err("timed out".to_owned())],
            };
            Asked {
                answers,
                protocol: tls.alpn_protocol().map(<[u8]>::to_vec),
                reset,
            }
        })
    }
}

/// Asks for `url` with the body that [`body_for`] gives its path, and
/// returns the answer's body.
async fn ask(
    client: h2::client::SendRequest<&'static [u8]>,
    url: String,
) -> Result<String, h2::Error> {
    let mut client = client.ready().await?;
    let request = http::Request::post(url).body(()).unwrap();
    let body: &'static [u8] = body_for(request.uri().path()).leak();
    let (response, mut send) = client.send_request(request, false)?;
    send.send_data(body, true)?;
    let mut body = response.await?.into_body();
    let mut answer = Vec::new();
    while let Some(chunk) = body.data().await {
        let chunk = chunk?;
        body.flow_control().release_capacity(chunk.len())?;
        answer.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&answer).into_owned())
}

/// A fake destination over TLS with `config`, which serves HTTP/2 with the
/// `h2` crate, and lets the client send frames, windows, and a header
/// table larger than HTTP/2's first ones. It notes each request in the
/// order it came: its method, its path, and whether its body is the whole
/// of what [`body_for`] gives. It answers each with the path.
fn h2_destination(config: Arc<ServerConfig>) -> (u16, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Seen::default();
    let noted = Arc::clone(&seen);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let (config, noted) = (Arc::clone(&config), Arc::clone(&noted));
            thread::spawn(move || serve_h2(stream, config, noted));
        }
    });
    (port, seen)
}

/// Serves HTTP/2 on `stream`, as [`h2_destination`] says.
fn serve_h2(stream: TcpStream, config: Arc<ServerConfig>, noted: Seen) {
    runtime().block_on(async {
        let (io, pipe) = duplex(PIPE);
        let serving = tokio::spawn(async move {
            let mut builder = h2::server::Builder::new();
            builder
                .max_frame_size(1 << 20)
                .initial_window_size(1 << 20)
                .initial_connection_window_size(1 << 20)
                .header_table_size(1 << 16);
            let Ok(mut connection) = builder.handshake::<_, &'static [u8]>(io).await else {
                return;
            };
            while let Some(Ok((request, respond))) = connection.accept().await {
                let at = {
                    let mut noted = noted.lock().unwrap();
                    noted.push(String::new());
                    noted.len() - 1
                };
                tokio::spawn(answer_h2(request, respond, Arc::clone(&noted), at));
            }
        });
        stream.set_nonblocking(true).unwrap();
        let mut pipe = Some(pipe);
        let mut tls = rustls::Connection::Server(ServerConnection::new(config).unwrap());
        tls.set_buffer_limit(None);
        let mut unsent = Vec::new();
        let mut buffer = vec![0; 64 * 1024];
        let deadline = Instant::now() + WAIT;
        while !serving.is_finished() && Instant::now() < deadline {
            let mut received = Vec::new();
            match (&stream).read(&mut buffer) {
                Ok(0) => pipe = None,
                Ok(n) => received.extend_from_slice(&buffer[..n]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => pipe = None,
            }
            if let Some(open) = &mut pipe
                && relay(open, &mut tls, &received, &mut unsent).await != Ok(true)
            {
                pipe = None;
            }
            while !unsent.is_empty() {
                match (&stream).write(&unsent) {
                    Ok(n) => drop(unsent.drain(..n)),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => unsent.clear(),
                }
            }
            tokio::task::yield_now().await;
            thread::sleep(Duration::from_millis(1));
        }
    });
}

/// Notes `request` at `at`, once its body has come, and answers it.
async fn answer_h2(
    request: http::Request<h2::RecvStream>,
    mut respond: h2::server::SendResponse<&'static [u8]>,
    noted: Seen,
    at: usize,
) {
    let (method, path) = (
        request.method().to_string(),
        request.uri().path().to_owned(),
    );
    let mut body = request.into_body();
    let mut got = Vec::new();
    while let Some(Ok(chunk)) = body.data().await {
        let _ = body.flow_control().release_capacity(chunk.len());
        got.extend_from_slice(&chunk);
    }
    let whole = match got == body_for(&path) {
        true => "whole",
        false => "broken",
    };
    noted.lock().unwrap()[at] = format!("{method} {path} {whole}");
    if let Ok(mut send) = respond.send_response(http::Response::new(()), false) {
        let _ = send.send_data(path.into_bytes().leak(), true);
    }
}

// spec(NETWORK): On a decrypted connection, the proxy verifies the upstream server's certificate and checks every request's host and path, including each request on a reused HTTP/2 connection (`:authority`).
// spec(NETWORK): On a host with URL rules, where it puts in no secret's value, it offers `h2` too, and the guest gets HTTP/2 only when the server chose it.
#[test]
fn each_request_on_a_reused_http2_connection_is_checked() {
    let (root, config) = server();
    let (port, seen) = h2_destination(config);
    let rules = format!("NETWORK -\nNETWORK +https://{NAME}:{port}/allowed\n");
    let mut guest = Guest::new(&rules, vec![root], port);
    let url = |host: &str, path: &str| format!("https://{host}:{port}{path}");
    let paths = ["/allowed/one", "/allowed/two", "/allowed/three"];

    // Three requests at once, then a denied one, on one connection.
    let handle = guest.connect();
    let tls = Guest::tls(&guest.authority.clone());
    let allowed = paths.map(|path| url(NAME, path)).to_vec();
    let asked = guest.ask_h2(handle, tls, vec![allowed, vec![url(NAME, "/denied")]]);
    assert_eq!(asked.protocol.as_deref(), Some(&b"h2"[..]));
    assert_eq!(asked.answers[..3], paths.map(|path| Ok(path.to_owned())));
    assert!(asked.answers[3].is_err(), "{:?}", asked.answers[3]);
    assert!(asked.reset);
    // A request whose `:authority` names another host.
    let handle = guest.connect();
    let tls = Guest::tls(&guest.authority.clone());
    let other = vec![vec![url("example.com", "/allowed/other")]];
    let asked = guest.ask_h2(handle, tls, other);
    assert!(asked.answers[0].is_err(), "{:?}", asked.answers);
    assert!(asked.reset);

    // The allowed requests reached the destination whole and in order, and
    // nothing else did.
    assert_eq!(
        settled(&seen),
        paths.map(|path| format!("POST {path} whole"))
    );
}

#[test]
fn the_guest_gets_http2_only_when_the_server_chose_it() {
    let (root, config) = server_offering(&[b"http/1.1"]);
    let (port, seen) = destination(Some(config));
    let rules = format!("NETWORK -\nNETWORK +https://{NAME}:{port}/allowed\n");
    let mut guest = Guest::new(&rules, vec![root], port);
    let handle = guest.connect();
    let mut tls = Guest::tls(&guest.authority.clone());
    let answer = guest.exchange(handle, Some(&mut tls), &get("/allowed/one"));
    assert!(answer.is_ok(), "{answer:?}");
    assert_eq!(tls.alpn_protocol(), Some(&b"http/1.1"[..]));
    assert_eq!(settled(&seen), ["GET /allowed/one HTTP/1.1 (http/1.1)"]);
}

// spec(NETWORK): **Decryption.** The proxy decrypts TLS, with the per-run certificate authority, only where it must.
// spec(NETWORK): on hosts with URL rules, because the rules need the request path
// spec(NETWORK): A request that the rules deny never reaches the server, and the proxy resets the connection it came on.
#[test]
fn a_url_rule_decides_each_request_on_a_connection_the_proxy_decrypts() {
    let (root, config) = server();
    let (port, seen) = destination(Some(config));
    let rules = format!("NETWORK -\nNETWORK +https://{NAME}:{port}/allowed\n");
    let mut guest = Guest::new(&rules, vec![root.clone()], port);

    let reused = guest.connect();
    let mut tls = Guest::http1(&guest.authority.clone());
    let answer = guest.exchange(reused, Some(&mut tls), &get("/allowed/a"));
    assert!(
        answer
            .as_ref()
            .is_ok_and(|a| a.ends_with("\r\n\r\n/allowed/a")),
        "{answer:?}"
    );
    // The guest's side is the run's authority's.
    assert_eq!(tls.alpn_protocol(), Some(&b"http/1.1"[..]));
    // A second request on the same connection, which the rules deny,
    // resets it.
    let answer = guest.exchange(reused, Some(&mut tls), &get("/denied"));
    assert_eq!(answer, Err("reset".to_owned()));

    let denied = guest.connect();
    let mut tls = Guest::http1(&guest.authority.clone());
    let answer = guest.exchange(denied, Some(&mut tls), &get("/allowed/../denied"));
    assert_eq!(answer, Err("reset".to_owned()));
    // An allowed and a denied request together: the denied one never
    // reaches the destination, and the connection resets.
    let mixed = guest.connect();
    let mut tls = Guest::http1(&guest.authority.clone());
    let both = [get("/allowed/pipelined"), get("/denied/pipelined")].concat();
    let answer = guest.exchange(mixed, Some(&mut tls), &both);
    assert_eq!(answer, Err("reset".to_owned()));
    let answer = {
        let other = guest.connect();
        let mut tls = Guest::http1(&guest.authority.clone());
        let named = b"GET /allowed/b HTTP/1.1\r\nHost: example.com\r\n\r\n";
        guest.exchange(other, Some(&mut tls), named)
    };
    assert!(answer.is_err(), "{answer:?}");

    // The destination got the allowed requests alone, over the HTTP/1.1
    // the guest offered, though it agrees HTTP/2 too. An allowed request
    // ahead of a denied one may or may not have reached it before the
    // reset.
    let pipelined = "GET /allowed/pipelined HTTP/1.1 (http/1.1)";
    let seen: Vec<String> = (settled(&seen).into_iter())
        .filter(|line| line != pipelined)
        .collect();
    assert_eq!(seen, ["GET /allowed/a HTTP/1.1 (http/1.1)"]);
}

#[test]
fn a_destination_that_ends_before_its_handshake_still_resets_a_denied_guest() {
    let (root, _) = server();
    // It takes the connection, and closes it at once.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            drop(stream);
        }
    });
    let rules = format!("NETWORK -\nNETWORK +https://{NAME}:{port}/allowed\n");
    let mut guest = Guest::new(&rules, vec![root], port);
    let handle = guest.connect();
    let mut tls = Guest::http1(&guest.authority.clone());
    let both = [get("/allowed/first"), get("/denied")].concat();
    let started = Instant::now();
    assert_eq!(
        guest.exchange(handle, Some(&mut tls), &both),
        Err("reset".to_owned())
    );
    assert!(started.elapsed() < WAIT / 2, "{:?}", started.elapsed());
}

/// A destination that takes each connection and never answers, and notes
/// `ended` once the proxy closes it.
fn silent() -> (u16, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Seen::default();
    let noted = Arc::clone(&seen);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let noted = Arc::clone(&noted);
            thread::spawn(move || {
                let mut bytes = Vec::new();
                let _ = stream.read_to_end(&mut bytes);
                noted.lock().unwrap().push("ended".to_owned());
            });
        }
    });
    (port, seen)
}

/// Runs the guest's side of the TLS handshake on `handle`.
fn handshake(guest: &mut Guest, handle: SocketHandle, tls: &mut ClientConnection) {
    let deadline = Instant::now() + WAIT;
    let mut buffer = [0; 16 * 1024];
    let mut unsent = Vec::new();
    while tls.is_handshaking() && Instant::now() < deadline {
        guest.poll();
        let socket = guest.sockets.get_mut::<tcp::Socket>(handle);
        while tls.wants_write() {
            tls.write_tls(&mut unsent).unwrap();
        }
        if socket.can_send() && !unsent.is_empty() {
            let n = socket.send_slice(&unsent).unwrap();
            unsent.drain(..n);
        }
        while socket.can_recv() {
            let n = socket.recv_slice(&mut buffer).unwrap();
            tls.read_tls(&mut &buffer[..n]).unwrap();
            tls.process_new_packets().unwrap();
        }
        thread::sleep(Duration::from_millis(1));
    }
    assert!(!tls.is_handshaking());
    // Its last flight goes alone, so the proxy's side finishes before any
    // request comes.
    while tls.wants_write() {
        tls.write_tls(&mut unsent).unwrap();
    }
    send_raw(guest, handle, &unsent);
}

#[test]
fn a_guest_that_ends_before_any_request_leaves_no_connection_open() {
    let (root, config) = server();
    let (answering, _) = destination(Some(config));
    let (quiet, ended) = silent();
    for port in [answering, quiet] {
        let rules = format!("NETWORK -\nNETWORK +https://{NAME}:{port}/allowed\n");
        let mut guest = Guest::new(&rules, vec![root.clone()], port);
        let handle = guest.connect();
        let mut tls = Guest::http1(&guest.authority.clone());
        // Its handshake, then its close, and no request.
        handshake(&mut guest, handle, &mut tls);
        tls.send_close_notify();
        let mut out = Vec::new();
        while tls.wants_write() {
            tls.write_tls(&mut out).unwrap();
        }
        send_raw(&mut guest, handle, &out);
        guest.sockets.get_mut::<tcp::Socket>(handle).close();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            guest.poll();
            if guest.sockets.get_mut::<tcp::Socket>(handle).state() == tcp::State::Closed {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        let state = guest.sockets.get_mut::<tcp::Socket>(handle).state();
        assert_eq!(state, tcp::State::Closed, "on port {port}");
        // The proxy closed the silent destination's end too, while the
        // link is still up.
        if port == quiet {
            assert_eq!(settled(&ended), ["ended"]);
        }
    }
}

#[test]
fn a_request_and_the_guest_s_close_together_reach_a_slow_destination() {
    let (root, config) = server();
    let (port, seen) = destination_after(Some(config), Duration::from_secs(1));
    let rules = format!("NETWORK -\nNETWORK +https://{NAME}:{port}/allowed\n");
    let mut guest = Guest::new(&rules, vec![root], port);
    let handle = guest.connect();
    let mut tls = Guest::http1(&guest.authority.clone());
    handshake(&mut guest, handle, &mut tls);
    // The request and the close in one write, while the proxy's own
    // handshake waits for the destination.
    tls.writer().write_all(&get("/allowed/slow")).unwrap();
    tls.send_close_notify();
    let mut out = Vec::new();
    while tls.wants_write() {
        tls.write_tls(&mut out).unwrap();
    }
    send_raw(&mut guest, handle, &out);
    let deadline = Instant::now() + WAIT;
    while seen.lock().unwrap().is_empty() && Instant::now() < deadline {
        guest.poll();
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(settled(&seen), ["GET /allowed/slow HTTP/1.1 (http/1.1)"]);
}

#[test]
fn a_denied_request_resets_the_guest_while_the_destination_stays_silent() {
    let (root, _) = server();
    let (port, ended) = silent();
    let rules = format!("NETWORK -\nNETWORK +https://{NAME}:{port}/allowed\n");
    let mut guest = Guest::new(&rules, vec![root], port);
    let handle = guest.connect();
    let mut tls = Guest::http1(&guest.authority.clone());
    handshake(&mut guest, handle, &mut tls);
    let answer = guest.exchange(handle, Some(&mut tls), &get("/denied"));
    assert_eq!(answer, Err("reset".to_owned()));
    assert_eq!(settled(&ended), ["ended"]);
}

/// Sends `bytes` as they are on `handle`, and gives the guest a moment.
fn send_raw(guest: &mut Guest, handle: SocketHandle, bytes: &[u8]) {
    let deadline = Instant::now() + WAIT;
    let mut unsent = bytes;
    while !unsent.is_empty() && Instant::now() < deadline {
        guest.poll();
        let socket = guest.sockets.get_mut::<tcp::Socket>(handle);
        if socket.can_send() {
            let n = socket.send_slice(unsent).unwrap();
            unsent = &unsent[n..];
        }
        thread::sleep(Duration::from_millis(1));
    }
    for _ in 0..100 {
        guest.poll();
        thread::sleep(Duration::from_millis(1));
    }
}

/// TLS records after `close_notify`, which nothing reads: more than
/// rustls reads at once.
fn after_close() -> Vec<u8> {
    [0x17, 0x03, 0x03, 0x00, 0x05, 1, 2, 3, 4, 5].repeat(1000)
}

#[test]
fn bytes_after_either_side_ends_tls_hold_up_no_other_connection() {
    let (root, config) = server();
    let (port, seen) = destination(Some(Arc::clone(&config)));
    // A destination that answers, then ends TLS and sends a record more.
    let ending = TcpListener::bind("127.0.0.1:0").unwrap();
    let ending_port = ending.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in ending.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut server = ServerConnection::new(Arc::clone(&config)).unwrap();
            let mut tls = rustls::Stream::new(&mut server, &mut stream);
            let mut line = String::new();
            let _ = BufReader::new(&mut tls).read_line(&mut line);
            let _ = tls.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nend");
            server.send_close_notify();
            let mut bytes = Vec::new();
            while server.wants_write() {
                let _ = server.write_tls(&mut bytes);
            }
            bytes.extend(after_close());
            let _ = stream.write_all(&bytes);
            thread::sleep(Duration::from_secs(1));
        }
    });
    let rules = format!(
        "NETWORK -\nNETWORK +https://{NAME}:{port}/allowed\nNETWORK +https://{NAME}:{ending_port}/allowed\n"
    );
    let mut guest = Guest::new(&rules, vec![root], port);
    let authority = guest.authority.clone();

    // The guest ends TLS, and sends a record more.
    let handle = guest.connect();
    let mut tls = Guest::http1(&authority);
    let answer = guest.exchange(handle, Some(&mut tls), &get("/allowed/a"));
    assert!(answer.is_ok(), "{answer:?}");
    tls.send_close_notify();
    let mut bytes = Vec::new();
    while tls.wants_write() {
        tls.write_tls(&mut bytes).unwrap();
    }
    bytes.extend(after_close());
    send_raw(&mut guest, handle, &bytes);

    // The destination does.
    guest.port = ending_port;
    let handle = guest.connect();
    let mut tls = Guest::http1(&authority);
    let answer = guest.exchange(handle, Some(&mut tls), &get("/allowed/end"));
    assert!(answer.is_ok(), "{answer:?}");

    // The proxy still serves.
    guest.port = port;
    let handle = guest.connect();
    let mut tls = Guest::http1(&authority);
    let answer = guest.exchange(handle, Some(&mut tls), &get("/allowed/b"));
    assert!(answer.is_ok(), "{answer:?}");
    assert_eq!(
        settled(&seen),
        [
            "GET /allowed/a HTTP/1.1 (http/1.1)",
            "GET /allowed/b HTTP/1.1 (http/1.1)"
        ]
    );
}

// spec(NETWORK): A file that can't be read, or isn't a regular file, adds none.
#[test]
fn ssl_cert_file_adds_the_roots_of_a_regular_file_alone() {
    let directory = std::env::temp_dir().join(format!("ap-roots-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).unwrap();
    let authority = Authority::generate(SystemTime::now()).unwrap();
    let file = directory.join("roots.pem");
    let pem = format!("junk\n{}", authority.certificate_pem());
    std::fs::write(&file, pem).unwrap();
    assert_eq!(ap::network::roots(&file), [authority.certificate().clone()]);
    let fifo = directory.join("fifo");
    let path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: `path` is a C string that outlives the call.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    // A pipe with no writer would hold up a read.
    for unread in [directory.join("missing"), directory.clone(), fifo] {
        assert!(
            ap::network::roots(&unread).is_empty(),
            "{}",
            unread.display()
        );
    }
    std::fs::remove_dir_all(&directory).unwrap();
}

// spec(NETWORK): The proxy verifies an upstream server's certificate against the root certificates compiled into `ap`, plus those in the file that `SSL_CERT_FILE` names in your host's environment.
#[test]
fn a_destination_whose_certificate_the_roots_don_t_trust_is_refused() {
    let (root, config) = server();
    let (port, seen) = destination(Some(config));
    let rules = format!("NETWORK -\nNETWORK +https://{NAME}:{port}/allowed\n");
    // Trusted, it answers.
    let mut guest = Guest::new(&rules, vec![root], port);
    let handle = guest.connect();
    let mut tls = Guest::http1(&guest.authority.clone());
    let answer = guest.exchange(handle, Some(&mut tls), &get("/allowed"));
    assert!(answer.is_ok(), "{answer:?}");
    // Not, and nothing reaches it, whether the guest's hello waits for the
    // destination's handshake, as it does when it offers HTTP/2, or not.
    let mut guest = Guest::new(&rules, Vec::new(), port);
    for offered in [&[&b"http/1.1"[..]][..], &[b"h2", b"http/1.1"]] {
        let handle = guest.connect();
        let mut tls = Guest::tls_offering(&guest.authority.clone(), offered);
        let answer = guest.exchange(handle, Some(&mut tls), &get("/allowed/untrusted"));
        assert!(answer.is_err(), "{answer:?}");
    }
    assert_eq!(settled(&seen), ["GET /allowed HTTP/1.1 (http/1.1)"]);
}

#[test]
fn a_host_without_url_rules_is_passed_through_without_decryption() {
    let (root, config) = server();
    let (port, seen) = destination(Some(config));
    let mut guest = Guest::new(&format!("NETWORK -\nNETWORK +{NAME}\n"), Vec::new(), port);
    // The guest sees the destination's own certificate, and agrees HTTP/2
    // with it.
    let handle = guest.connect();
    let mut tls = Guest::tls(&root);
    let answer = guest.exchange(handle, Some(&mut tls), &get("/anything"));
    assert!(answer.is_ok(), "{answer:?}");
    assert_eq!(tls.alpn_protocol(), Some(&b"h2"[..]));
    assert_eq!(settled(&seen), ["GET /anything HTTP/1.1 (h2)"]);
}

// spec(NETWORK): under a broader rule of the opposite sign: with `+github.com` and `-gist.github.com`, on every TLS connection that `+github.com` allows, because a client can reuse one connection for several names.
// spec(NETWORK): TLS that the proxy decrypts and that doesn't carry HTTP is denied, because the proxy can't check the names in it.
// spec(NETWORK): The proxy offers only `http/1.1` on a connection it decrypts for a narrower `-` rule under a `+` rule, to a host with no URL rules, and on any connection it decrypts to put in a secret's value.
#[test]
fn a_plus_rule_with_a_narrower_minus_rule_beneath_it_is_decrypted() {
    let (root, config) = server();
    let (port, seen) = destination(Some(config));
    let rules = format!("NETWORK -\nNETWORK +{NAME}\nNETWORK -gist.{NAME}\n");
    let mut guest = Guest::new(&rules, vec![root.clone()], port);
    // The guest's side is the run's authority's, and offered only HTTP/1.1.
    let reused = guest.connect();
    let mut tls = Guest::tls(&guest.authority.clone());
    let answer = guest.exchange(reused, Some(&mut tls), &get("/a"));
    assert!(
        answer.as_ref().is_ok_and(|a| a.ends_with("\r\n\r\n/a")),
        "{answer:?}"
    );
    assert_eq!(tls.alpn_protocol(), Some(&b"http/1.1"[..]));
    // A request for the narrower name on the same connection never reaches
    // the destination, and resets the connection.
    let gist = format!("GET /b HTTP/1.1\r\nHost: gist.{NAME}\r\n\r\n");
    let answer = guest.exchange(reused, Some(&mut tls), gist.as_bytes());
    assert_eq!(answer, Err("reset".to_owned()));
    // TLS that names no server is decrypted too.
    let unnamed = guest.connect();
    let mut tls = Guest::tls_unnamed(&guest.authority.clone());
    let answer = guest.exchange(unnamed, Some(&mut tls), &get("/c"));
    assert!(answer.is_ok(), "{answer:?}");
    let answer = guest.exchange(unnamed, Some(&mut tls), gist.as_bytes());
    assert_eq!(answer, Err("reset".to_owned()));
    // TLS that doesn't carry HTTP is denied, since its names can't be read.
    let other = guest.connect();
    let mut tls = Guest::tls(&guest.authority.clone());
    let answer = guest.exchange(other, Some(&mut tls), b"a001 LOGIN me secret\r\n\r\n");
    assert!(answer.is_err(), "{answer:?}");
    // Traffic that isn't TLS passes, and plain HTTP gets the name check.
    let (plain, heard) = destination(None);
    let mut guest = Guest::new(&rules, Vec::new(), plain);
    let handle = guest.connect();
    let answer = guest.exchange(handle, None, b"SSH-2.0-OpenSSH_9.9\r\n\r\n");
    assert!(answer.is_ok(), "{answer:?}");
    let handle = guest.connect();
    let answer = guest.exchange(handle, None, &get("/plain"));
    assert!(answer.is_ok(), "{answer:?}");
    let answer = guest.exchange(handle, None, gist.as_bytes());
    assert_eq!(answer, Err("reset".to_owned()));
    assert_eq!(
        settled(&heard),
        ["SSH-2.0-OpenSSH_9.9 (plain)", "GET /plain HTTP/1.1 (plain)"]
    );
    // A server that speaks first, as SSH does, still reaches the guest.
    let (port, _) = greeter();
    let mut guest = Guest::new(&rules, Vec::new(), port);
    let handle = guest.connect();
    let deadline = Instant::now() + WAIT;
    let mut greeted = Vec::new();
    while Instant::now() < deadline && greeted.is_empty() {
        guest.poll();
        let socket = guest.sockets.get_mut::<tcp::Socket>(handle);
        if socket.can_recv() {
            let mut buffer = [0; 256];
            let n = socket.recv_slice(&mut buffer).unwrap();
            greeted.extend_from_slice(&buffer[..n]);
        }
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(greeted, b"220 greeting\r\n");
    assert_eq!(
        settled(&seen),
        ["GET /a HTTP/1.1 (http/1.1)", "GET /c HTTP/1.1 (http/1.1)"]
    );
}

// spec(NETWORK): The proxy offers only `http/1.1` on a connection it decrypts for a narrower `-` rule under a `+` rule, to a host with no URL rules, and on any connection it decrypts to put in a secret's value.
// spec(NETWORK): On a host with URL rules, where it puts in no secret's value, it offers `h2` too, and the guest gets HTTP/2 only when the server chose it.
#[test]
fn a_narrower_url_rule_under_a_plus_rule_leaves_http2_offered() {
    let (root, config) = server();
    let (port, seen) = h2_destination(config);
    let rules = format!("NETWORK -\nNETWORK +{NAME}\nNETWORK -https://{NAME}:{port}/denied\n");
    let mut guest = Guest::new(&rules, vec![root], port);
    // The host has a URL rule, so the guest and the destination agree
    // HTTP/2 through the proxy, as on any host with URL rules.
    let handle = guest.connect();
    let tls = Guest::tls(&guest.authority.clone());
    let allowed = vec![vec![format!("https://{NAME}:{port}/allowed")]];
    let asked = guest.ask_h2(handle, tls, allowed);
    assert_eq!(asked.protocol.as_deref(), Some(&b"h2"[..]));
    assert_eq!(asked.answers, [Ok("/allowed".to_owned())]);
    assert_eq!(settled(&seen), ["POST /allowed whole"]);
}

#[test]
fn a_minus_rule_beside_a_plus_rule_leaves_its_tls_unread() {
    let (root, config) = server();
    let (port, seen) = destination(Some(config));
    let rules = format!("NETWORK -\nNETWORK +{NAME}\nNETWORK -other.ap.internal\n");
    let mut guest = Guest::new(&rules, Vec::new(), port);
    // The guest sees the destination's own certificate, and agrees HTTP/2
    // with it.
    let handle = guest.connect();
    let mut tls = Guest::tls(&root);
    let answer = guest.exchange(handle, Some(&mut tls), &get("/anything"));
    assert!(answer.is_ok(), "{answer:?}");
    assert_eq!(tls.alpn_protocol(), Some(&b"h2"[..]));
    assert_eq!(settled(&seen), ["GET /anything HTTP/1.1 (h2)"]);
}

// spec(NETWORK): On a TLS connection that the proxy doesn't decrypt, it sees the server name but not the requests inside.
// spec(NETWORK): A server you allow can then be asked for other names it serves, including names the policy denies: other names on its certificate, or other sites behind the same shared front, such as a CDN.
#[test]
fn a_server_the_proxy_doesn_t_decrypt_can_be_asked_for_a_denied_name() {
    let (root, config) = server();
    let (port, seen) = destination(Some(config));
    let rules = format!("NETWORK -\nNETWORK +{NAME}\nNETWORK -denied.example\n");
    let mut guest = Guest::new(&rules, Vec::new(), port);
    // The proxy reads the server name: TLS that names another site than
    // the one the guest connected to is reset, before the guest could see
    // any certificate.
    let handle = guest.connect();
    let mut tls = Guest::tls_for(&root, &[b"http/1.1"], "denied.example");
    let answer = guest.exchange(handle, Some(&mut tls), &get("/named"));
    assert_eq!(answer, Err("reset".to_owned()));
    // Inside TLS for the allowed name, a request for the denied site reaches
    // the server, which answers it.
    let handle = guest.connect();
    let mut tls = Guest::tls_offering(&root, &[b"http/1.1"]);
    let inside = b"GET /inside HTTP/1.1\r\nHost: denied.example\r\n\r\n";
    let answer = guest.exchange(handle, Some(&mut tls), inside);
    assert!(
        answer
            .as_ref()
            .is_ok_and(|a| a.ends_with("\r\n\r\n/inside")),
        "{answer:?}"
    );
    assert_eq!(settled(&seen), ["GET /inside HTTP/1.1 (http/1.1)"]);
}

/// Whether a datagram the guest sends to `host.ap.internal` on `port`
/// gets an answer from the echo there.
fn echoed(guest: &mut Guest, port: u16) -> bool {
    let buffer = || udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0; 4096]);
    let mut socket = udp::Socket::new(buffer(), buffer());
    socket.bind(40000).unwrap();
    let handle = guest.sockets.add(socket);
    let to = SocketAddr::new(IpAddr::V4(guest.host), port);
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut sent = false;
    while Instant::now() < deadline {
        guest.poll();
        let socket = guest.sockets.get_mut::<udp::Socket>(handle);
        if !sent && socket.can_send() {
            socket.send_slice(b"ping", to).unwrap();
            sent = true;
        }
        if socket.can_recv() {
            return true;
        }
        thread::sleep(Duration::from_millis(1));
    }
    false
}

// spec(NETWORK): On a host with URL rules, traffic that isn't HTTP is denied.
#[test]
fn udp_is_denied_on_a_host_with_url_rules() {
    let echo = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = echo.local_addr().unwrap().port();
    thread::spawn(move || {
        let mut buffer = [0; 64];
        while let Ok((n, from)) = echo.recv_from(&mut buffer) {
            let _ = echo.send_to(&buffer[..n], from);
        }
    });
    // The host rule allows the flow, until the host has a URL rule too.
    let host = format!("NETWORK -\nNETWORK +{NAME}\n");
    assert!(echoed(&mut Guest::new(&host, Vec::new(), port), port));
    let url = format!("{host}NETWORK -https://{NAME}/secret\n");
    assert!(!echoed(&mut Guest::new(&url, Vec::new(), port), port));
}

/// Set while a test runs again in a user and network namespace of its own,
/// where it may listen on port 443.
const INSIDE: &str = "AP_DECRYPT_TEST_INSIDE";

/// Whether `test` runs in a namespace of its own, where this host's
/// loopback is up. Outside one, it runs `test` again in one, which must
/// pass, and returns false.
fn inside(test: &str) -> bool {
    if std::env::var_os(INSIDE).is_some() {
        let up = Command::new("ip")
            .args(["link", "set", "lo", "up"])
            .status();
        assert!(up.as_ref().is_ok_and(|up| up.success()), "{up:?}");
        return true;
    }
    let output = Command::new("unshare")
        .args(["--user", "--map-root-user", "--net"])
        .arg(std::env::current_exe().unwrap())
        .args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env(INSIDE, "1")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    println!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{:?}", output.status);
    false
}

/// An echo for UDP on this host's loopback, on `port`, or on any port if it
/// is 0. Returns the port.
fn udp_echo(port: u16) -> u16 {
    let echo = std::net::UdpSocket::bind(("127.0.0.1", port)).unwrap();
    let port = echo.local_addr().unwrap().port();
    thread::spawn(move || {
        let mut buffer = [0; 64];
        while let Ok((n, from)) = echo.recv_from(&mut buffer) {
            let _ = echo.send_to(&buffer[..n], from);
        }
    });
    port
}

// spec(NETWORK): QUIC to a host that needs decrypting is blocked, so clients fall back to TCP.
// spec(NETWORK): UDP port 443 is denied to a host the proxy decrypts.
#[test]
fn udp_to_port_443_is_denied_to_a_host_the_proxy_decrypts() {
    if !inside("udp_to_port_443_is_denied_to_a_host_the_proxy_decrypts") {
        return;
    }
    let quic = udp_echo(443);
    let other = udp_echo(0);
    // A host rule alone lets QUIC through.
    let host = format!("NETWORK -\nNETWORK +{NAME}\n");
    assert!(echoed(&mut Guest::new(&host, Vec::new(), quic), quic));
    // Under a `+` rule with a narrower `-` rule beneath it, a host gets no
    // QUIC, and UDP on another port as the rules decide.
    let narrowed = format!("{host}NETWORK -gist.{NAME}\n");
    assert!(!echoed(&mut Guest::new(&narrowed, Vec::new(), quic), quic));
    assert!(echoed(&mut Guest::new(&narrowed, Vec::new(), other), other));
    // A hidden secret's bound host gets none either.
    let mut bound = Guest::injecting(&host, Vec::new(), quic, bound_to(NAME));
    assert!(!echoed(&mut bound, quic));
    // A host with URL rules gets no UDP at all.
    let url = format!("{host}NETWORK -https://{NAME}/secret\n");
    assert!(!echoed(&mut Guest::new(&url, Vec::new(), quic), quic));
}

/// A clock for the proxy that stands still until the test moves it, and
/// that counts how often the proxy reads it.
#[derive(Clone)]
struct Clock {
    start: Instant,
    ahead: Arc<AtomicU64>,
    reads: Arc<AtomicUsize>,
    /// Whether a read waits until the clock moves, and whether one does.
    held: Arc<AtomicBool>,
    waiting: Arc<AtomicBool>,
}

impl Clock {
    fn new() -> Clock {
        Clock {
            start: Instant::now(),
            ahead: Arc::default(),
            reads: Arc::default(),
            held: Arc::default(),
            waiting: Arc::default(),
        }
    }

    /// The time by this clock.
    fn now(&self) -> Instant {
        while self.held.load(Ordering::SeqCst) {
            self.waiting.store(true, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(1));
        }
        let ahead = Duration::from_millis(self.ahead.load(Ordering::SeqCst));
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.start + ahead
    }

    /// Moves this clock `by` forward, and returns once the proxy has taken
    /// a whole turn by the new time.
    fn advance(&self, by: Duration) {
        (self.ahead).fetch_add(by.as_millis() as u64, Ordering::SeqCst);
        self.held.store(false, Ordering::SeqCst);
        let read = self.reads.load(Ordering::SeqCst);
        // A turn reads the clock three times, and one read that began
        // before the move may count after it, so six reads hold a whole
        // turn.
        let deadline = Instant::now() + WAIT;
        while self.reads.load(Ordering::SeqCst) < read + 6 {
            assert!(Instant::now() < deadline, "the proxy took no turn");
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// Stops the proxy at its next read of this clock, and returns once it
    /// waits there. It goes on when the clock moves, by the new time.
    fn hold(&self) {
        self.waiting.store(false, Ordering::SeqCst);
        self.held.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + WAIT;
        while !self.waiting.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "the proxy never read the clock");
            thread::sleep(Duration::from_millis(1));
        }
    }
}

/// A guest's UDP socket on `port`, and a destination's on this host's
/// loopback, which is where the guest sends.
struct Flow {
    handle: SocketHandle,
    server: std::net::UdpSocket,
    to: SocketAddr,
    /// Where the guest's datagrams reach the destination from.
    from: Option<SocketAddr>,
}

impl Flow {
    fn new(guest: &mut Guest, server: std::net::UdpSocket) -> Flow {
        let buffer = || udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0; 4096]);
        let mut socket = udp::Socket::new(buffer(), buffer());
        socket.bind(40000).unwrap();
        let handle = guest.sockets.add(socket);
        let port = server.local_addr().unwrap().port();
        let to = SocketAddr::new(IpAddr::V4(guest.host), port);
        Flow {
            handle,
            server,
            to,
            from: None,
        }
    }

    /// Sends `payload` from the guest, and waits for it at the
    /// destination.
    fn send(&mut self, guest: &mut Guest, payload: &[u8]) {
        let deadline = Instant::now() + WAIT;
        let mut sent = false;
        let mut buffer = [0; 64];
        loop {
            assert!(Instant::now() < deadline, "the datagram never came");
            guest.poll();
            let socket = guest.sockets.get_mut::<udp::Socket>(self.handle);
            if !sent && socket.can_send() {
                socket.send_slice(payload, self.to).unwrap();
                sent = true;
            }
            match self.server.recv_from(&mut buffer) {
                Ok((len, from)) => {
                    assert_eq!(&buffer[..len], payload);
                    // A flow that stayed open reaches the destination from
                    // where it did before.
                    assert_eq!(*self.from.get_or_insert(from), from);
                    return;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("{error}"),
            }
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// Whether `payload`, sent by the destination, reaches the guest within
    /// `wait`.
    fn replies(&mut self, guest: &mut Guest, payload: &[u8], wait: Duration) -> bool {
        self.reply(payload);
        self.received(guest, payload, wait)
    }

    /// Sends `payload` from the destination.
    fn reply(&self, payload: &[u8]) {
        self.server.send_to(payload, self.from.unwrap()).unwrap();
    }

    /// Whether `payload` reaches the guest within `wait`.
    fn received(&mut self, guest: &mut Guest, payload: &[u8], wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        while Instant::now() < deadline {
            guest.poll();
            let socket = guest.sockets.get_mut::<udp::Socket>(self.handle);
            if let Ok((got, _)) = socket.recv() {
                assert_eq!(got, payload);
                return true;
            }
            thread::sleep(Duration::from_millis(1));
        }
        false
    }
}

// spec(NETWORK): A UDP flow closes after 60 seconds with no datagram either way, as it would behind a NAT, and a reply that comes later is dropped.
#[test]
fn a_udp_flow_closes_after_60_seconds_with_no_datagram() {
    let server = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    server.set_nonblocking(true).unwrap();
    let port = server.local_addr().unwrap().port();
    let clock = Clock::new();
    let read = clock.clone();
    let rules = format!("NETWORK -\nNETWORK +{NAME}\n");
    let mut guest = Guest::configured(&rules, Vec::new(), port, Vec::new(), move |proxy| {
        proxy.clocked(move || read.now())
    });
    let mut flow = Flow::new(&mut guest, server);
    let second = Duration::from_secs(1);
    flow.send(&mut guest, b"ping");

    // The guest's datagram keeps the flow open for the replies of the next
    // 60 seconds, and each reply keeps it open as long again.
    clock.advance(Duration::from_secs(59));
    assert!(flow.replies(&mut guest, b"one", WAIT));
    // By now the proxy's stack has forgotten the guest's link address, so
    // this reply waits for the one lookup the stopped clock allows.
    clock.advance(Duration::from_secs(59));
    assert!(flow.replies(&mut guest, b"two", WAIT));

    // Another datagram from the guest keeps it open too, on the same port.
    clock.advance(Duration::from_secs(30));
    flow.send(&mut guest, b"again");
    clock.advance(Duration::from_secs(59));
    assert!(flow.replies(&mut guest, b"three", WAIT));

    // After 60 seconds with no datagram either way, a reply is dropped,
    // even one that is there when the proxy first turns past the limit.
    clock.hold();
    flow.reply(b"late");
    clock.advance(Duration::from_secs(60));
    assert!(!flow.received(&mut guest, b"late", second));
}

/// A plain destination that greets each connection first, as an SMTP
/// server does, and then answers requests as [`destination`]'s does. It
/// notes each connection as `connected`.
fn greeter() -> (u16, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Seen::default();
    let noted = Arc::clone(&seen);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let noted = Arc::clone(&noted);
            thread::spawn(move || {
                noted.lock().unwrap().push("connected".to_owned());
                stream.set_read_timeout(Some(WAIT)).unwrap();
                let _ = stream.write_all(b"220 greeting\r\n");
                answer(&mut stream, &noted, |_| "plain".to_owned());
            });
        }
    });
    (port, seen)
}

/// A destination that reads `before` in plain, and then serves TLS with
/// `config` from the next byte, as a server does after `STARTTLS`. It
/// answers requests as [`destination`]'s does.
fn tls_after(before: &'static [u8], config: Arc<ServerConfig>) -> (u16, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Seen::default();
    let noted = Arc::clone(&seen);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let (config, noted) = (Arc::clone(&config), Arc::clone(&noted));
            thread::spawn(move || {
                stream.set_read_timeout(Some(WAIT)).unwrap();
                let mut plain = vec![0; before.len()];
                if stream.read_exact(&mut plain).is_err() || plain != before {
                    return;
                }
                let server = ServerConnection::new(config).unwrap();
                let mut stream = rustls::StreamOwned::new(server, stream);
                answer(&mut stream, &noted, |_| "tls after plain".to_owned());
            });
        }
    });
    (port, seen)
}

// spec(NETWORK): On a host that the proxy decrypts for a rule, TLS that starts where a connection's text ends, as it does after `STARTTLS`, is denied, because the proxy decrypts TLS only from a connection's first byte.
#[test]
fn tls_where_text_ends_is_denied_on_a_host_the_proxy_decrypts() {
    let (root, config) = server();
    let (port, seen) = tls_after(b"STARTTLS\r\n", config);
    let broader = format!("NETWORK -\nNETWORK +{NAME}\nNETWORK -gist.{NAME}\n");
    let url = format!("NETWORK -\nNETWORK +http://{NAME}:{port}/after\n");
    for rules in [broader, url] {
        let mut guest = Guest::new(&rules, vec![root.clone()], port);
        let handle = guest.connect();
        send_raw(&mut guest, handle, b"STARTTLS\r\n");
        let mut tls = Guest::http1(&root);
        let answer = guest.exchange(handle, Some(&mut tls), &get("/after"));
        assert_eq!(answer, Err("reset".to_owned()), "{rules}");
    }
    assert_eq!(settled(&seen), [] as [String; 0]);
}

// spec(NETWORK): On a host without URL rules, TLS that starts after the control character that ends a connection's text, as PostgreSQL's does, is neither checked nor decrypted, and a client can end its text on purpose with a control character that its server ignores.
#[test]
fn tls_after_text_that_ended_otherwise_is_not_decrypted() {
    const SSL_REQUEST: &[u8] = b"\x00\x00\x00\x08\x04\xd2\x16\x2f";
    let (root, config) = server();
    let (port, seen) = tls_after(SSL_REQUEST, config);
    let rules = format!("NETWORK -\nNETWORK +{NAME}\nNETWORK -gist.{NAME}\n");
    let mut guest = Guest::new(&rules, Vec::new(), port);
    let handle = guest.connect();
    send_raw(&mut guest, handle, SSL_REQUEST);
    // The guest sees the destination's own certificate, and a request for
    // the narrower name reaches it.
    let mut tls = Guest::http1(&root);
    let gist = format!("GET /b HTTP/1.1\r\nHost: gist.{NAME}\r\n\r\n");
    let answer = guest.exchange(handle, Some(&mut tls), gist.as_bytes());
    assert!(
        answer.as_ref().is_ok_and(|a| a.ends_with("\r\n\r\n/b")),
        "{answer:?}"
    );
    assert_eq!(settled(&seen), ["GET /b HTTP/1.1 (tls after plain)"]);
}

// spec(NETWORK): The proxy connects to the server before the first request, but nothing the server sends reaches the guest until the rules allow a request.
// spec(NETWORK): The proxy reads what the guest sends, and what the server answers passes unread.
#[test]
fn the_server_reaches_the_guest_only_once_a_request_is_allowed() {
    let (port, seen) = greeter();
    let rules = format!("NETWORK -\nNETWORK +http://{NAME}:{port}/allowed\n");
    let mut guest = Guest::new(&rules, Vec::new(), port);
    // The guest has sent part of a request, which reads as HTTP.
    let silent = guest.connect();
    send_raw(&mut guest, silent, b"GET /allo");
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        guest.poll();
        let socket = guest.sockets.get_mut::<tcp::Socket>(silent);
        assert!(!socket.can_recv(), "the greeting reached the guest");
        thread::sleep(Duration::from_millis(1));
    }
    let answer = guest.exchange(silent, None, b"w\r\n\r\n");
    assert_eq!(answer, Err("reset".to_owned()));
    let allowed = guest.connect();
    let answer = guest.exchange(allowed, None, &get("/allowed"));
    assert!(
        answer
            .as_ref()
            .is_ok_and(|a| a.starts_with("220 greeting\r\n")),
        "{answer:?}"
    );
    let mut seen = settled(&seen);
    seen.sort();
    assert_eq!(
        seen,
        ["GET /allowed HTTP/1.1 (plain)", "connected", "connected"]
    );
}

// spec(NETWORK): On a host with URL rules, traffic that isn't HTTP is denied.
#[test]
fn traffic_that_isn_t_http_is_denied_on_a_host_with_url_rules() {
    let (port, seen) = destination(None);
    let rules = format!("NETWORK -\nNETWORK +http://{NAME}:{port}/allowed\n");
    let mut guest = Guest::new(&rules, Vec::new(), port);
    let handle = guest.connect();
    let answer = guest.exchange(handle, None, b"SSH-2.0-OpenSSH_9.9\r\n\r\n");
    assert!(answer.is_err(), "{answer:?}");
    // Plain HTTP is read, request by request.
    let handle = guest.connect();
    let answer = guest.exchange(handle, None, &get("/allowed"));
    assert!(answer.is_ok(), "{answer:?}");
    let answer = guest.exchange(handle, None, &get("/denied"));
    assert_eq!(answer, Err("reset".to_owned()));
    assert_eq!(settled(&seen), ["GET /allowed HTTP/1.1 (plain)"]);
}

const PLACEHOLDER: &str = "0123456789abcdef0123456789abcdef";
const VALUE: &str = "real-secret-value";
/// `git:` and the placeholder, then `git:` and the value, in base64.
const BASIC_PLACEHOLDER: &str = "Z2l0OjAxMjM0NTY3ODlhYmNkZWYwMTIzNDU2Nzg5YWJjZGVm";
const BASIC_VALUE: &str = "Z2l0OnJlYWwtc2VjcmV0LXZhbHVl";

/// The run's hidden secret, bound to `host`, with Claude Code's headers.
fn bound_to(host: &str) -> Injection {
    Injection {
        headers: ap::adapter::headers(agentfile::Harness::Claude)
            .iter()
            .map(|header| header.to_string())
            .collect(),
        secrets: vec![Hidden {
            name: "API_TOKEN".to_owned(),
            value: VALUE.as_bytes().to_vec(),
            placeholder: PLACEHOLDER.as_bytes().to_vec(),
            hosts: vec![host.to_owned()],
            required: false,
        }],
    }
}

/// The run's `AUTH key`, as [`bound_to`]'s secret, whose placeholder every
/// request to `host` must carry.
fn key_for(host: &str) -> Injection {
    let mut injection = bound_to(host);
    injection.secrets[0].required = true;
    injection
}

/// A request to `host` that holds no placeholder.
fn without_placeholder(host: &str) -> Vec<u8> {
    format!("GET /bare HTTP/1.1\r\nHost: {host}\r\nX-Api-Key: mine\r\n\r\n").into_bytes()
}

/// A fake destination as [`destination`]'s, which notes the whole of each
/// request: its head, and the body that its `Content-Length` gives.
fn recording(tls: Option<Arc<ServerConfig>>) -> (u16, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Seen::default();
    let noted = Arc::clone(&seen);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let (tls, noted) = (tls.clone(), Arc::clone(&noted));
            thread::spawn(move || {
                stream.set_read_timeout(Some(WAIT)).unwrap();
                match tls {
                    Some(config) => {
                        let server = ServerConnection::new(config).unwrap();
                        record(rustls::StreamOwned::new(server, stream), &noted);
                    }
                    None => record(stream, &noted),
                }
            });
        }
    });
    (port, seen)
}

fn record<S: Read + Write>(stream: S, noted: &Seen) {
    let mut reader = BufReader::new(stream);
    loop {
        let mut request = String::new();
        let mut line = String::new();
        while line != "\r\n" {
            line.clear();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            request.push_str(&line);
        }
        let length = (request.lines())
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .map_or(0, |length| length.parse().unwrap());
        let mut body = vec![0; length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        request.push_str(&String::from_utf8_lossy(&body));
        noted.lock().unwrap().push(request);
        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
        let stream = reader.get_mut();
        if stream.write_all(response).is_err() || stream.flush().is_err() {
            return;
        }
    }
}

/// A request to `host`, which holds the placeholder in each place it could.
fn holding_placeholder(host: &str) -> Vec<u8> {
    let basic = BASIC_PLACEHOLDER;
    format!(
        "POST /path?key={PLACEHOLDER} HTTP/1.1\r\nHost: {host}\r\n\
         Authorization: Basic {basic}\r\nX-Api-Key: {PLACEHOLDER}\r\n\
         X-Other: {PLACEHOLDER}\r\nContent-Length: {}\r\n\r\n{PLACEHOLDER}",
        PLACEHOLDER.len()
    )
    .into_bytes()
}

// spec(SECRET): The egress proxy replaces the placeholder with the real value only under both of these conditions.
// spec(SECRET): on TLS connections to the secret's bound hosts
// spec(SECRET): in request headers on the adapter's allowlist (for example, `Authorization` and `x-api-key`), never in query strings or bodies
// spec(SECRET): In an `Authorization: Basic` header, which git sends over HTTPS, the proxy decodes the credentials, replaces the placeholder in them, and encodes them again.
// spec(SECRET): Injection requires decrypting TLS to the bound hosts (see [Enforcement](#network)).
// spec(NETWORK): The proxy offers only `http/1.1` on a connection it decrypts for a narrower `-` rule under a `+` rule, to a host with no URL rules, and on any connection it decrypts to put in a secret's value.
#[test]
fn a_bound_host_gets_the_value_in_the_allowed_headers_alone() {
    let (root, config) = server();
    let (port, seen) = recording(Some(config));
    let rules = format!("NETWORK -\nNETWORK +{NAME}\n");
    let mut guest = Guest::injecting(&rules, vec![root], port, bound_to(NAME));
    let handle = guest.connect();
    // The guest trusts the run's authority alone, so the proxy decrypted.
    // It offered HTTP/2, as the destination does, and gets HTTP/1.1, since
    // the proxy puts a value in only an HTTP/1.1 request.
    let mut tls = Guest::tls(&guest.authority.clone());
    let answer = guest.exchange(handle, Some(&mut tls), &holding_placeholder(NAME));
    assert!(answer.is_ok(), "{answer:?}");
    assert_eq!(tls.alpn_protocol(), Some(&b"http/1.1"[..]));

    let basic = BASIC_VALUE;
    let seen = settled(&seen);
    assert_eq!(
        seen,
        [format!(
            "POST /path?key={PLACEHOLDER} HTTP/1.1\r\nHost: {NAME}\r\n\
             Authorization: Basic {basic}\r\nX-Api-Key: {VALUE}\r\n\
             X-Other: {PLACEHOLDER}\r\nContent-Length: {}\r\n\r\n{PLACEHOLDER}",
            PLACEHOLDER.len()
        )]
    );
}

// spec(SECRET): A connection counts as one to a bound host only when the name behind the address it connects to (see [NETWORK](#network)), its TLS server name, and each request's `Host` or `:authority` all name that host, under either network default.
// spec(SECRET): On any other connection, the placeholder passes through unchanged.
#[test]
fn the_placeholder_passes_unchanged_anywhere_but_to_the_bound_host() {
    let (root, config) = server();
    let (port, seen) = recording(Some(config.clone()));
    let rules = format!("NETWORK -\nNETWORK +{NAME}\n");

    // A secret bound to another host: the connection isn't decrypted, and
    // the guest sees the destination's own certificate.
    let mut guest = Guest::injecting(&rules, Vec::new(), port, bound_to("api.example.com"));
    let handle = guest.connect();
    let mut tls = Guest::tls_offering(&root, &[b"http/1.1"]);
    let answer = guest.exchange(handle, Some(&mut tls), &holding_placeholder(NAME));
    assert!(answer.is_ok(), "{answer:?}");

    let mut guest = Guest::injecting(&rules, vec![root.clone()], port, bound_to(NAME));
    // A request on the bound host's connection whose `Host` names another
    // host.
    let handle = guest.connect();
    let mut tls = Guest::tls(&guest.authority.clone());
    let answer = guest.exchange(handle, Some(&mut tls), &holding_placeholder("example.com"));
    assert!(answer.is_ok(), "{answer:?}");
    // Plain HTTP to the bound host.
    let (plain, plain_seen) = recording(None);
    let mut plain_guest = Guest::injecting(&rules, Vec::new(), plain, bound_to(NAME));
    let handle = plain_guest.connect();
    let answer = plain_guest.exchange(handle, None, &holding_placeholder(NAME));
    assert!(answer.is_ok(), "{answer:?}");
    // A server name that names another host: the name rule's check refuses
    // it, so nothing reaches the destination.
    let handle = guest.connect();
    let mut tls = Guest::tls_for(&guest.authority.clone(), &[b"http/1.1"], "example.com");
    let answer = guest.exchange(handle, Some(&mut tls), &holding_placeholder(NAME));
    assert!(answer.is_err(), "{answer:?}");

    let all = [settled(&seen), settled(&plain_seen)].concat();
    assert_eq!(all.len(), 3, "{all:?}");
    for request in all {
        assert!(!request.contains(VALUE), "{request}");
        // In the query, `X-Api-Key`, `X-Other`, and the body, and in the
        // `Basic` credentials, still encoded.
        assert_eq!(request.matches(PLACEHOLDER).count(), 4, "{request}");
        assert!(request.contains(BASIC_PLACEHOLDER), "{request}");
    }
}

// spec(SECRET): A bound host matches that exact host only, not its subdomains.
#[test]
fn a_secret_bound_to_a_parent_host_never_reaches_its_subdomain() {
    let (root, config) = server();
    let (port, seen) = recording(Some(config));
    let rules = format!("NETWORK -\nNETWORK +{NAME}\n");
    // `host.ap.internal` is a subdomain of `ap.internal`, and `.internal`.
    for parent in ["ap.internal", "internal"] {
        let mut guest = Guest::injecting(&rules, Vec::new(), port, bound_to(parent));
        let handle = guest.connect();
        // The connection isn't decrypted, so the guest sees the
        // destination's own certificate.
        let mut tls = Guest::tls_offering(&root, &[b"http/1.1"]);
        let answer = guest.exchange(handle, Some(&mut tls), &holding_placeholder(NAME));
        assert!(answer.is_ok(), "{parent}: {answer:?}");
    }
    let seen = settled(&seen);
    assert_eq!(seen.len(), 2, "{seen:?}");
    for request in seen {
        assert!(!request.contains(VALUE), "{request}");
        assert_eq!(request.matches(PLACEHOLDER).count(), 4, "{request}");
        assert!(request.contains(BASIC_PLACEHOLDER), "{request}");
    }
}

// spec(NETWORK): The proxy offers only `http/1.1` on a connection it decrypts for a narrower `-` rule under a `+` rule, to a host with no URL rules, and on any connection it decrypts to put in a secret's value.
#[test]
fn a_bound_host_with_url_rules_gets_the_value_in_the_requests_they_allow() {
    let (root, config) = server();
    let (port, seen) = recording(Some(config));
    let rules = format!("NETWORK -\nNETWORK +https://{NAME}:{port}/path\n");
    let mut guest = Guest::injecting(&rules, vec![root], port, bound_to(NAME));
    let handle = guest.connect();
    // The guest and the destination offer HTTP/2, and the proxy puts the
    // value in an HTTP/1.1 request alone.
    let mut tls = Guest::tls(&guest.authority.clone());
    let answer = guest.exchange(handle, Some(&mut tls), &holding_placeholder(NAME));
    assert!(answer.is_ok(), "{answer:?}");
    assert_eq!(tls.alpn_protocol(), Some(&b"http/1.1"[..]));
    let seen = settled(&seen);
    assert_eq!(seen.len(), 1);
    assert!(
        seen[0].contains(&format!("X-Api-Key: {VALUE}\r\n")),
        "{seen:?}"
    );
}

#[test]
fn a_bound_host_s_server_may_still_speak_first() {
    // A server that speaks first, as SSH does, reaches the guest.
    let (port, _) = greeter();
    let rules = format!("NETWORK -\nNETWORK +{NAME}\n");
    let mut guest = Guest::injecting(&rules, Vec::new(), port, bound_to(NAME));
    let handle = guest.connect();
    let deadline = Instant::now() + WAIT;
    let mut greeted = Vec::new();
    while Instant::now() < deadline && greeted.is_empty() {
        guest.poll();
        let socket = guest.sockets.get_mut::<tcp::Socket>(handle);
        if socket.can_recv() {
            let mut buffer = [0; 256];
            let n = socket.recv_slice(&mut buffer).unwrap();
            greeted.extend_from_slice(&buffer[..n]);
        }
        thread::sleep(Duration::from_millis(1));
    }
    assert!(!greeted.is_empty());
}

#[test]
fn a_guest_that_waits_before_its_hello_still_gets_the_value() {
    let (root, config) = server();
    let (port, seen) = recording(Some(config));
    let rules = format!("NETWORK -\nNETWORK +{NAME}\n");
    let mut guest = Guest::injecting(&rules, vec![root], port, bound_to(NAME));
    let handle = guest.connect();
    // Past the quarter second after which the server may speak first.
    let until = Instant::now() + Duration::from_millis(600);
    while Instant::now() < until {
        guest.poll();
        thread::sleep(Duration::from_millis(1));
    }
    let mut tls = Guest::tls(&guest.authority.clone());
    let answer = guest.exchange(handle, Some(&mut tls), &holding_placeholder(NAME));
    assert!(answer.is_ok(), "{answer:?}");
    let seen = settled(&seen);
    assert!(
        seen[0].contains(&format!("X-Api-Key: {VALUE}\r\n")),
        "{seen:?}"
    );
}

#[test]
fn a_client_hello_that_never_ends_is_reset() {
    let (root, config) = server();
    let (port, _) = recording(Some(config));
    let rules = format!("NETWORK -\nNETWORK +{NAME}\n");
    let mut guest = Guest::injecting(&rules, vec![root], port, bound_to(NAME));
    let handle = guest.connect();
    // The start of a handshake record, and nothing more.
    send_raw(&mut guest, handle, &[0x16, 0x03, 0x01, 0x02, 0x00]);
    let deadline = Instant::now() + Duration::from_secs(45);
    while Instant::now() < deadline {
        guest.poll();
        if guest.sockets.get_mut::<tcp::Socket>(handle).state() == tcp::State::Closed {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("the connection is still open");
}

// spec(AUTH): The proxy swaps in the real value on the hosts the adapter lists for that key, with the same checks as for a hidden secret.
// spec(NETWORK): With `AUTH key`, the proxy allows only requests that carry the key's placeholder on the key's hosts.
#[test]
fn a_key_s_host_takes_only_the_requests_that_carry_its_placeholder() {
    let (root, config) = server();
    let (port, seen) = recording(Some(config));
    let rules = format!("NETWORK -\nNETWORK +{NAME}\n");
    let mut guest = Guest::injecting(&rules, vec![root], port, key_for(NAME));
    let authority = guest.authority.clone();

    // A request that carries the placeholder gets the value. One after it
    // that doesn't is refused, and so is its connection.
    let handle = guest.connect();
    let mut tls = Guest::tls(&authority);
    let answer = guest.exchange(handle, Some(&mut tls), &holding_placeholder(NAME));
    assert!(answer.is_ok(), "{answer:?}");
    let answer = guest.exchange(handle, Some(&mut tls), &without_placeholder(NAME));
    assert!(answer.is_err(), "{answer:?}");

    // On connections of their own, the proxy refuses a request without the
    // placeholder, one whose `Host` names another host, TLS for another
    // server name, and plain HTTP, though each holds the placeholder but
    // the first.
    let mut refused = |request: Vec<u8>, server: Option<&str>| {
        let handle = guest.connect();
        let mut tls = server.map(|server| Guest::tls_for(&authority, &[b"http/1.1"], server));
        let answer = guest.exchange(handle, tls.as_mut(), &request);
        assert!(answer.is_err(), "{answer:?}");
    };
    refused(without_placeholder(NAME), Some(NAME));
    refused(holding_placeholder("example.com"), Some(NAME));
    refused(holding_placeholder(NAME), Some("example.com"));
    refused(holding_placeholder(NAME), None);

    // Only the first request reached the destination.
    let seen = settled(&seen);
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert!(
        seen[0].contains(&format!("X-Api-Key: {VALUE}\r\n")),
        "{seen:?}"
    );
    assert!(seen[0].starts_with("POST /path?"), "{seen:?}");
}

#[test]
fn a_key_s_host_gets_no_udp() {
    let echo = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = echo.local_addr().unwrap().port();
    thread::spawn(move || {
        let mut buffer = [0; 64];
        while let Ok((n, from)) = echo.recv_from(&mut buffer) {
            let _ = echo.send_to(&buffer[..n], from);
        }
    });
    // A hidden secret's bound host still gets UDP on a port that isn't
    // QUIC's. A key's host gets none.
    let rules = format!("NETWORK -\nNETWORK +{NAME}\n");
    let mut bound = Guest::injecting(&rules, Vec::new(), port, bound_to(NAME));
    assert!(echoed(&mut bound, port));
    let mut keyed = Guest::injecting(&rules, Vec::new(), port, key_for(NAME));
    assert!(!echoed(&mut keyed, port));
}

#[test]
fn a_key_s_server_never_speaks_first() {
    // The greeter speaks first, which a bound host's guest hears, as
    // `a_bound_host_s_server_may_still_speak_first` shows. A key's host
    // takes only requests, so nothing reaches the guest.
    let (port, _) = greeter();
    let rules = format!("NETWORK -\nNETWORK +{NAME}\n");
    let mut guest = Guest::injecting(&rules, Vec::new(), port, key_for(NAME));
    let handle = guest.connect();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        guest.poll();
        let socket = guest.sockets.get_mut::<tcp::Socket>(handle);
        assert!(!socket.can_recv(), "the server spoke first");
        thread::sleep(Duration::from_millis(1));
    }
}

// spec(NETWORK): on a hidden secret's bound hosts and on the hosts of `AUTH key`, to inject the value
#[test]
fn a_secret_s_bound_host_and_a_key_s_host_are_decrypted_to_inject_the_value() {
    let (root, config) = server();
    let (port, seen) = recording(Some(config));
    let rules = format!("NETWORK -\nNETWORK +{NAME}\n");
    // Without either, the guest sees the server's own certificate, which
    // the run's authority didn't sign.
    let mut guest = Guest::new(&rules, vec![root.clone()], port);
    let handle = guest.connect();
    let mut tls = Guest::tls(&guest.authority.clone());
    let answer = guest.exchange(handle, Some(&mut tls), &holding_placeholder(NAME));
    assert!(
        answer
            .as_ref()
            .is_err_and(|error| error.contains("certificate")),
        "{answer:?}"
    );
    // With either, the run's authority signs what the guest sees, and the
    // server gets the value.
    for injection in [bound_to(NAME), key_for(NAME)] {
        let mut guest = Guest::injecting(&rules, vec![root.clone()], port, injection);
        let handle = guest.connect();
        let mut tls = Guest::tls(&guest.authority.clone());
        let answer = guest.exchange(handle, Some(&mut tls), &holding_placeholder(NAME));
        assert!(answer.is_ok(), "{answer:?}");
    }
    let seen = settled(&seen);
    assert_eq!(seen.len(), 2, "{seen:?}");
    for request in &seen {
        assert!(
            request.contains(&format!("X-Api-Key: {VALUE}\r\n")),
            "{seen:?}"
        );
    }
}

// spec(NETWORK): These hosts are an exfiltration path: code in the agent can send data to them with a key it brought itself.
// spec(NETWORK): The other harness hosts stay an exfiltration path.
#[test]
fn a_harness_host_takes_a_key_the_agent_brought_itself() {
    // Under `NETWORK -` alone, a harness host is allowed for being one.
    let closed = Network::read(&parse("AGENT claude\nNETWORK -\n").unwrap()).unwrap();
    let connection = ::net::Connection {
        name: Some("api.example.com"),
        address: "203.0.113.9".parse().unwrap(),
        port: 443,
        request: None,
    };
    let decided = ::net::proxied(&closed, &["api.example.com"], &connection);
    assert!(matches!(decided, ::net::Proxied::Harness), "{decided:?}");
    // A loopback destination needs its `+` rule, which plays the harness's
    // allowance here.
    let (root, config) = server();
    let (port, seen) = recording(Some(config));
    let rules = format!("NETWORK -\nNETWORK +{NAME}\n");
    let harness = vec![NAME.to_owned(), "api.example.com".to_owned()];
    let theirs = format!(
        "POST /out HTTP/1.1\r\nHost: {NAME}\r\nX-Api-Key: mine\r\nContent-Length: 4\r\n\r\ndata"
    );
    // Without `AUTH key`, and with a key for another of the harness's
    // hosts, the guest sees the server's own certificate, so the proxy
    // reads nothing, and the guest's own key and data arrive as it sent
    // them.
    for injection in [Injection::default(), key_for("api.example.com")] {
        let mut guest = Guest::with_harness(&rules, Vec::new(), port, injection, harness.clone());
        let handle = guest.connect();
        let mut tls = Guest::tls_offering(&root, &[b"http/1.1"]);
        let answer = guest.exchange(handle, Some(&mut tls), theirs.as_bytes());
        assert!(answer.is_ok(), "{answer:?}");
    }
    assert_eq!(settled(&seen), [theirs.clone(), theirs]);
}

// spec(NETWORK): `AUTH key` is the one case where the proxy decrypts a harness host's traffic.
#[test]
fn a_harness_host_is_decrypted_only_for_its_key() {
    let (root, config) = server();
    let (port, seen) = recording(Some(config));
    let rules = format!("NETWORK -\nNETWORK +{NAME}\n");
    let harness = vec![NAME.to_owned()];
    // Without a key, the guest sees the destination's own certificate.
    let mut guest = Guest::with_harness(
        &rules,
        Vec::new(),
        port,
        Injection::default(),
        harness.clone(),
    );
    let handle = guest.connect();
    let mut tls = Guest::tls_offering(&root, &[b"http/1.1"]);
    let answer = guest.exchange(handle, Some(&mut tls), &holding_placeholder(NAME));
    assert!(answer.is_ok(), "{answer:?}");
    // With one, the run's authority signs what the guest sees.
    let mut guest = Guest::with_harness(&rules, vec![root], port, key_for(NAME), harness);
    let handle = guest.connect();
    let mut tls = Guest::tls(&guest.authority.clone());
    let answer = guest.exchange(handle, Some(&mut tls), &holding_placeholder(NAME));
    assert!(answer.is_ok(), "{answer:?}");
    let seen = settled(&seen);
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert!(!seen[0].contains(VALUE), "{seen:?}");
    assert!(
        seen[1].contains(&format!("X-Api-Key: {VALUE}\r\n")),
        "{seen:?}"
    );
}
