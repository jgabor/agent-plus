//! Downloads pinned files and layers over HTTPS into the store.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ::store::Layers;
use graviola::hashing::sha2::Sha256Context;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

use crate::store::Store;

const KERNEL_PIN: &str = include_str!("../../../guest/kernel/pin.toml");
const OS_PIN: &str = include_str!("../../../guest/os/pin.toml");
const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_HEAD_BYTES: u64 = 64 * 1024;
/// Far above any file `ap` pins, and low enough that a bad server can't fill
/// the disk before the digest check.
const MAX_BODY_BYTES: u64 = 1 << 30;

/// A file to download, and the digest it must have.
pub struct Pin<'a> {
    pub url: &'a str,
    pub sha256: &'a str,
}

/// The guest kernel named in `guest/kernel/pin.toml`.
pub fn kernel() -> Pin<'static> {
    pin(KERNEL_PIN)
}

/// The `OS minimal` layer named in `guest/os/pin.toml`.
pub fn os() -> Pin<'static> {
    pin(OS_PIN)
}

fn pin(toml: &'static str) -> Pin<'static> {
    let value = |key: &str| {
        toml.lines()
            .find_map(|line| {
                let (name, value) = line.split_once(" = ")?;
                (name == key).then(|| value.trim_matches('"'))
            })
            .unwrap_or_else(|| panic!("pin.toml has no {key}"))
    };
    Pin {
        url: value("url"),
        sha256: value("sha256"),
    }
}

/// An HTTPS client that trusts a fixed set of roots.
pub struct Client {
    config: Arc<ClientConfig>,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    /// A client that trusts the web's roots, as compiled into `ap`.
    pub fn new() -> Self {
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Self::with_roots(roots)
    }

    pub fn with_roots(roots: RootCertStore) -> Self {
        let provider = Arc::new(rustls_graviola::default_provider());
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("the provider supports the default TLS versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        Client {
            config: Arc::new(config),
        }
    }

    fn get(&self, url: &str) -> io::Result<Response> {
        let url = Url::parse(url)?;
        let name = ServerName::try_from(url.host.to_owned()).map_err(|_| invalid(url.host))?;
        let connection =
            ClientConnection::new(self.config.clone(), name).map_err(io::Error::other)?;
        let mut stream = StreamOwned::new(connection, connect(url.host, url.port)?);
        write!(
            stream,
            "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: ap/{}\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n",
            url.target,
            url.authority,
            env!("CARGO_PKG_VERSION"),
        )?;
        stream.flush()?;
        Response::read(BufReader::new(stream))
    }
}

/// Returns the stored file for `pin`, downloading it only if it's missing.
pub fn fetch(store: &Store, client: &Client, pin: &Pin) -> io::Result<PathBuf> {
    let path = store.path(pin.sha256)?;
    if path.is_file() {
        return Ok(path);
    }
    store.put(pin.sha256, download(client, pin.url)?)
}

/// Puts the layer `pin` names in `layers` at a zstd level, downloading it
/// only if no layer has its digest yet, and returns whether it downloaded.
/// A download whose digest isn't the pin's is an error, and isn't stored.
pub fn layer(layers: &Layers, client: &Client, pin: &Pin, level: i8) -> io::Result<bool> {
    if layers.level(pin.sha256)?.is_some() {
        return Ok(false);
    }
    let mut data = Vec::new();
    download(client, pin.url)?.read_to_end(&mut data)?;
    let mut hash = Sha256Context::new();
    hash.update(&data);
    let actual: String = hash.finish().iter().map(|b| format!("{b:02x}")).collect();
    if actual != pin.sha256 {
        return Err(wrong_digest(&actual, pin.sha256));
    }
    layers.put(&data, level)?;
    Ok(true)
}

/// The body at `url`, through at most one redirect.
fn download(client: &Client, url: &str) -> io::Result<Body> {
    let mut response = client.get(url)?;
    if let Some(location) = response.redirect() {
        response = client.get(&location?)?;
        if response.redirect().is_some() {
            return Err(bad_response("more than one redirect"));
        }
    }
    response.body()
}

pub(crate) fn wrong_digest(actual: &str, pinned: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("the download has digest {actual}, not the pinned {pinned}"),
    )
}

type Tls = BufReader<StreamOwned<ClientConnection, TcpStream>>;

struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    stream: Tls,
}

impl Response {
    fn read(stream: Tls) -> io::Result<Self> {
        let mut head = stream.take(MAX_HEAD_BYTES);
        let status_line = read_line(&mut head)?;
        let status = status_line
            .strip_prefix("HTTP/1.")
            .and_then(|rest| rest.split(' ').nth(1))
            .and_then(|code| code.parse().ok())
            .ok_or_else(|| bad_response("a malformed status line"))?;
        let mut headers = Vec::new();
        loop {
            let line = read_line(&mut head)?;
            if line.is_empty() {
                break;
            }
            let (name, value) = line
                .split_once(':')
                .ok_or_else(|| bad_response("a malformed header"))?;
            headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
        }
        Ok(Response {
            status,
            headers,
            stream: head.into_inner(),
        })
    }

    fn header(&self, name: &str) -> Option<&str> {
        let (_, value) = self.headers.iter().find(|(key, _)| key == name)?;
        Some(value)
    }

    /// The target of a redirect, if this is one.
    fn redirect(&self) -> Option<io::Result<String>> {
        matches!(self.status, 301 | 302 | 303 | 307 | 308).then(|| {
            self.header("location")
                .map(str::to_owned)
                .ok_or_else(|| bad_response("a redirect with no Location"))
        })
    }

    /// The body, which errors if the connection ends before its full length.
    fn body(self) -> io::Result<Body> {
        if self.status != 200 {
            return Err(bad_response(&format!("status {}", self.status)));
        }
        if self.header("transfer-encoding").is_some() {
            return Err(bad_response("a Transfer-Encoding, which ap doesn't read"));
        }
        let remaining: u64 = self
            .header("content-length")
            .and_then(|length| length.parse().ok())
            .ok_or_else(|| bad_response("no valid Content-Length"))?;
        if remaining > MAX_BODY_BYTES {
            return Err(bad_response("a body that is too large"));
        }
        Ok(Body {
            stream: self.stream,
            remaining,
        })
    }
}

struct Body {
    stream: Tls,
    remaining: u64,
}

impl Read for Body {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 || buffer.is_empty() {
            return Ok(0);
        }
        let limit = buffer
            .len()
            .min(usize::try_from(self.remaining).unwrap_or(usize::MAX));
        let read = self.stream.read(&mut buffer[..limit])?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the download ended early",
            ));
        }
        self.remaining -= read as u64;
        Ok(read)
    }
}

/// One line of the response head, without its line ending.
fn read_line(head: &mut impl BufRead) -> io::Result<String> {
    let mut line = Vec::new();
    head.read_until(b'\n', &mut line)?;
    if !line.ends_with(b"\r\n") {
        return Err(bad_response("a truncated head"));
    }
    line.truncate(line.len() - 2);
    String::from_utf8(line).map_err(|_| bad_response("a head that isn't UTF-8"))
}

fn connect(host: &str, port: u16) -> io::Result<TcpStream> {
    let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{host} has no address"));
    for address in (host, port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&address, TIMEOUT) {
            Ok(stream) => {
                stream.set_read_timeout(Some(TIMEOUT))?;
                stream.set_write_timeout(Some(TIMEOUT))?;
                return Ok(stream);
            }
            Err(error) => last = error,
        }
    }
    Err(last)
}

struct Url<'a> {
    authority: &'a str,
    host: &'a str,
    port: u16,
    /// The path and query.
    target: &'a str,
}

impl<'a> Url<'a> {
    fn parse(url: &'a str) -> io::Result<Self> {
        let rest = url.strip_prefix("https://").ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{url} doesn't use https"),
            )
        })?;
        let slash = rest.find('/').ok_or_else(|| invalid(url))?;
        let (authority, target) = rest.split_at(slash);
        let target = target.split('#').next().unwrap_or(target);
        if !target.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(invalid(url));
        }
        let (host, port) = match authority.split_once(':') {
            Some((host, port)) => (host, port.parse().map_err(|_| invalid(url))?),
            None => (authority, 443),
        };
        Ok(Url {
            authority,
            host,
            port,
            target,
        })
    }
}

fn invalid(url: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{url} isn't a valid URL"),
    )
}

fn bad_response(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("the server sent {what}"),
    )
}
