//! The network process's proxy on a host with URL rules, by **Decryption**
//! under NETWORK in SPEC.md. The proxy serves a link, as in a run. A second
//! `smoltcp` interface plays the guest on the other end, and reaches fake
//! destinations on this host's loopback as `host.ap.internal`. Each fake
//! destination notes the request line of every request that reaches it.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::UnixDatagram;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use agentfile::{Network, parse};
use ap::network::{Authority, Decryption, Proxy};
use guest::net;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection};
use smoltcp::iface::{Config as IfaceConfig, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr};

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
        let (mine, theirs) = link();
        let (sent, got) = mpsc::channel();
        let lines = lines.to_owned();
        thread::spawn(move || {
            let instructions = parse(&format!("AGENT claude\n{lines}")).unwrap();
            let network = Network::read(&instructions).unwrap();
            let mut args = vec!["--".into()];
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
            let mut proxy = Proxy::new(network, Vec::new(), lookups).decrypting(decryption);
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

    /// A TLS client for `host.ap.internal`, which trusts only `root`, and
    /// offers `protocols`.
    fn tls_offering(root: &CertificateDer<'static>, protocols: &[&[u8]]) -> ClientConnection {
        let mut roots = RootCertStore::empty();
        roots.add(root.clone()).unwrap();
        let provider = Arc::new(rustls_graviola::default_provider());
        let mut config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = protocols.iter().map(|protocol| protocol.to_vec()).collect();
        let name = ServerName::try_from(NAME).unwrap();
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
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    (authority.certificate().clone(), Arc::new(config))
}

// spec(NETWORK): **Decryption.** The proxy decrypts TLS, with the per-run certificate authority, only where it must.
// spec(NETWORK): on hosts with URL rules, because the rules need the request path
// spec(NETWORK): Until the proxy has its own HTTP/2, it offers only `http/1.1` on a connection it decrypts.
// spec(NETWORK): A request that the rules deny never reaches the server, and the proxy resets the connection it came on.
#[test]
fn a_url_rule_decides_each_request_on_a_connection_the_proxy_decrypts() {
    let (root, config) = server();
    let (port, seen) = destination(Some(config));
    let rules = format!("NETWORK -\nNETWORK +https://{NAME}:{port}/allowed\n");
    let mut guest = Guest::new(&rules, vec![root.clone()], port);

    let reused = guest.connect();
    let mut tls = Guest::tls(&guest.authority.clone());
    let answer = guest.exchange(reused, Some(&mut tls), &get("/allowed/a"));
    assert!(
        answer
            .as_ref()
            .is_ok_and(|a| a.ends_with("\r\n\r\n/allowed/a")),
        "{answer:?}"
    );
    // The guest's side is the run's authority's, and offered only
    // HTTP/1.1.
    assert_eq!(tls.alpn_protocol(), Some(&b"http/1.1"[..]));
    // A second request on the same connection, which the rules deny,
    // resets it.
    let answer = guest.exchange(reused, Some(&mut tls), &get("/denied"));
    assert_eq!(answer, Err("reset".to_owned()));

    let denied = guest.connect();
    let mut tls = Guest::tls(&guest.authority.clone());
    let answer = guest.exchange(denied, Some(&mut tls), &get("/allowed/../denied"));
    assert_eq!(answer, Err("reset".to_owned()));
    // A guest that offers only HTTP/2 gets no protocol it can speak.
    let only_h2 = guest.connect();
    let mut tls = Guest::tls_offering(&guest.authority.clone(), &[b"h2"]);
    let answer = guest.exchange(only_h2, Some(&mut tls), &get("/allowed/h2"));
    assert!(answer.is_err(), "{answer:?}");
    let answer = {
        let other = guest.connect();
        let mut tls = Guest::tls(&guest.authority.clone());
        let named = b"GET /allowed/b HTTP/1.1\r\nHost: example.com\r\n\r\n";
        guest.exchange(other, Some(&mut tls), named)
    };
    assert!(answer.is_err(), "{answer:?}");

    // The destination got the allowed request alone, and was offered only
    // HTTP/1.1 too.
    assert_eq!(settled(&seen), ["GET /allowed/a HTTP/1.1 (http/1.1)"]);
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
    let mut tls = Guest::tls(&guest.authority.clone());
    let both = [get("/allowed/first"), get("/denied")].concat();
    let started = Instant::now();
    assert_eq!(
        guest.exchange(handle, Some(&mut tls), &both),
        Err("reset".to_owned())
    );
    assert!(started.elapsed() < WAIT / 2, "{:?}", started.elapsed());
}

#[test]
fn a_guest_that_ends_before_any_request_leaves_no_connection_open() {
    let (root, config) = server();
    let (port, _) = destination(Some(config));
    let rules = format!("NETWORK -\nNETWORK +https://{NAME}:{port}/allowed\n");
    let mut guest = Guest::new(&rules, vec![root], port);
    let handle = guest.connect();
    let mut tls = Guest::tls(&guest.authority.clone());
    // Its handshake, then its close, and no request.
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
            return;
        }
        thread::sleep(Duration::from_millis(1));
    }
    let state = guest.sockets.get_mut::<tcp::Socket>(handle).state();
    panic!("the proxy kept the connection open: {state}");
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
    let mut tls = Guest::tls(&authority);
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
    let mut tls = Guest::tls(&authority);
    let answer = guest.exchange(handle, Some(&mut tls), &get("/allowed/end"));
    assert!(answer.is_ok(), "{answer:?}");

    // The proxy still serves.
    guest.port = port;
    let handle = guest.connect();
    let mut tls = Guest::tls(&authority);
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
    let mut tls = Guest::tls(&guest.authority.clone());
    let answer = guest.exchange(handle, Some(&mut tls), &get("/allowed"));
    assert!(answer.is_ok(), "{answer:?}");
    // Not, and nothing reaches it.
    let mut guest = Guest::new(&rules, Vec::new(), port);
    let handle = guest.connect();
    let mut tls = Guest::tls(&guest.authority.clone());
    let answer = guest.exchange(handle, Some(&mut tls), &get("/allowed/untrusted"));
    assert!(answer.is_err(), "{answer:?}");
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
