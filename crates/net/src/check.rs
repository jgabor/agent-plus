//! The name check, by **Name checks** under NETWORK in SPEC.md: what a
//! connection shows without decrypting must agree with the name it is
//! attributed to. It reads the guest's bytes as they come. A TLS client
//! hello must name the attributed name, if it names any. Plain HTTP must
//! name it in every request's `Host` header. Anything else passes on the
//! attribution alone.

use rustls::server::Acceptor;

/// The most bytes a client hello, a request head, a chunk's size line, or
/// a body's trailers may take, and the most the first line may take before
/// it is known what the connection carries.
const HEAD_LIMIT: usize = 64 * 1024;
/// The most headers a request head, or a body's trailers, may have.
const HEADERS: usize = 128;

/// Reads what the guest sends on one connection, and passes the bytes on
/// once they agree with the name.
pub struct NameCheck {
    name: String,
    /// Bytes read and not yet passed.
    held: Vec<u8>,
    state: State,
}

/// The connection showed a name that disagrees, or bytes that can't be
/// read as what they start as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refused;

enum State {
    /// Nothing is known yet.
    First,
    /// A TLS client hello is coming, and the acceptor has read this many of
    /// the bytes held.
    Tls(Box<Acceptor>, usize),
    /// Plain HTTP, at this point of a request.
    Http(Http),
    /// Everything else passes unread.
    Passing,
    /// It refused, and nothing more passes.
    Refused,
}

#[derive(Debug, Clone, Copy)]
enum Http {
    Head,
    /// This many bytes of body are left.
    Body(u64),
    /// A chunked body, before a chunk's size line.
    ChunkSize,
    /// This many bytes of a chunk are left, then its line end.
    ChunkData(u64),
    ChunkEnd,
    /// A chunked body's trailers, and the empty line after them.
    Trailers,
}

impl NameCheck {
    /// A check for a connection attributed to `name`.
    pub fn new(name: &str) -> NameCheck {
        NameCheck {
            name: normal(name.as_bytes()),
            held: Vec::new(),
            state: State::First,
        }
    }

    /// Takes `input`, which the guest sent next, and adds the bytes that may
    /// now pass to `out`. Once it refuses, the connection must end, and
    /// nothing more may pass.
    pub fn feed(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<(), Refused> {
        self.held.extend_from_slice(input);
        let fed = self.read(out);
        if fed.is_err() {
            self.state = State::Refused;
            self.held = Vec::new();
        }
        fed
    }

    /// Passes what it can of the bytes held.
    fn read(&mut self, out: &mut Vec<u8>) -> Result<(), Refused> {
        loop {
            let moved = match &mut self.state {
                State::Passing => {
                    out.append(&mut self.held);
                    return Ok(());
                }
                State::Refused => return Err(Refused),
                State::First => match first(&self.held)? {
                    Some(state) => {
                        self.state = state;
                        true
                    }
                    None => false,
                },
                State::Tls(acceptor, read) => {
                    let agreed = hello(acceptor, read, &self.name, &self.held)?;
                    if agreed {
                        self.state = State::Passing;
                    }
                    agreed
                }
                State::Http(http) => match request(*http, &self.name, &self.held)? {
                    Some((taken, next)) => {
                        out.extend(self.held.drain(..taken));
                        self.state = next;
                        true
                    }
                    None => false,
                },
            };
            if !moved {
                return Ok(());
            }
        }
    }

    /// Whether it holds bytes that it can't pass until more come, or is
    /// inside a request. A connection that ends while it does was cut off
    /// before its check finished.
    pub fn holding(&self) -> bool {
        !self.held.is_empty()
            || match self.state {
                State::Http(Http::Head) | State::First | State::Passing | State::Refused => false,
                State::Tls(..) | State::Http(_) => true,
            }
    }

    /// Whether it has seen a byte.
    pub fn started(&self) -> bool {
        !matches!(self.state, State::First) || !self.held.is_empty()
    }
}

/// What a connection carries, from its first bytes, or `None` if more must
/// come.
fn first(held: &[u8]) -> Result<Option<State>, Refused> {
    let Some(&byte) = held.first() else {
        return Ok(None);
    };
    if byte == 0x16 {
        return Ok(Some(State::Tls(Box::default(), 0)));
    }
    // A server ignores empty lines before a request line.
    let start = held.iter().position(|&byte| byte != b'\r' && byte != b'\n');
    let short = || match held.len() > HEAD_LIMIT {
        true => Err(Refused),
        false => Ok(None),
    };
    let Some(start) = start else {
        return short();
    };
    let line = &held[start..];
    let method = line
        .iter()
        .take_while(|byte| byte.is_ascii_uppercase())
        .count();
    match line.get(method) {
        Some(b' ') if method > 0 => {}
        None => return short(),
        Some(_) => return Ok(Some(State::Passing)),
    }
    let Some(end) = line.iter().position(|&byte| byte == b'\n') else {
        return short();
    };
    // Anything like a version reads as HTTP, so the parser decides it.
    let version = line[..end].windows(6).any(|window| window == b" HTTP/");
    Ok(Some(match version {
        true => State::Http(Http::Head),
        false => State::Passing,
    }))
}

/// Gives `acceptor` the bytes held that it hasn't read, and reads the
/// client hello. Returns whether it agreed, or `false` if more must come.
fn hello(
    acceptor: &mut Acceptor,
    read: &mut usize,
    name: &str,
    held: &[u8],
) -> Result<bool, Refused> {
    let mut unread = &held[*read..];
    while !unread.is_empty() {
        if acceptor.read_tls(&mut unread).map_err(|_| Refused)? == 0 {
            return Err(Refused);
        }
    }
    *read = held.len();
    let accepted = match acceptor.accept() {
        Ok(None) if held.len() > HEAD_LIMIT => return Err(Refused),
        Ok(None) => return Ok(false),
        Ok(Some(accepted)) => accepted,
        Err(_) => return Err(Refused),
    };
    match accepted.client_hello().server_name() {
        Some(server) if normal(server.as_bytes()) != name => Err(Refused),
        _ => Ok(true),
    }
}

/// One step of a request from `held`, at `http`: how many bytes pass, and
/// what comes after them, or `None` if more must come.
fn request(http: Http, name: &str, held: &[u8]) -> Result<Option<(usize, State)>, Refused> {
    let short = || match held.len() > HEAD_LIMIT {
        true => Err(Refused),
        false => Ok(None),
    };
    match http {
        Http::Head => {
            if held.is_empty() {
                return Ok(None);
            }
            let mut headers = [httparse::EMPTY_HEADER; HEADERS];
            let mut parsed = httparse::Request::new(&mut headers);
            match parsed.parse(held).map_err(|_| Refused)? {
                httparse::Status::Partial => short(),
                httparse::Status::Complete(taken) => Ok(Some((taken, head(&parsed, name)?))),
            }
        }
        Http::Body(left) | Http::ChunkData(left) => {
            if held.is_empty() && left > 0 {
                return Ok(None);
            }
            let taken = left.min(held.len() as u64);
            let left = left - taken;
            let after = match (http, left) {
                (Http::Body(_), 0) => Http::Head,
                (Http::Body(_), _) => Http::Body(left),
                (_, 0) => Http::ChunkEnd,
                _ => Http::ChunkData(left),
            };
            Ok(Some((taken as usize, State::Http(after))))
        }
        Http::ChunkSize => match httparse::parse_chunk_size(held).map_err(|_| Refused)? {
            httparse::Status::Partial => short(),
            httparse::Status::Complete((taken, 0)) => {
                Ok(Some((taken, State::Http(Http::Trailers))))
            }
            httparse::Status::Complete((taken, size)) => {
                Ok(Some((taken, State::Http(Http::ChunkData(size)))))
            }
        },
        Http::ChunkEnd => match held {
            [] | [b'\r'] => Ok(None),
            [b'\r', b'\n', ..] => Ok(Some((2, State::Http(Http::ChunkSize)))),
            _ => Err(Refused),
        },
        Http::Trailers => {
            let mut headers = [httparse::EMPTY_HEADER; HEADERS];
            match httparse::parse_headers(held, &mut headers).map_err(|_| Refused)? {
                httparse::Status::Partial => short(),
                httparse::Status::Complete((taken, _)) => {
                    Ok(Some((taken, State::Http(Http::Head))))
                }
            }
        }
    }
}

/// What follows a request whose head is `parsed`, if its `Host` agrees
/// with `name`, and the server would read its body as this reads it.
fn head(parsed: &httparse::Request, name: &str) -> Result<State, Refused> {
    // An absolute target names a host of its own, and CONNECT opens a
    // tunnel to one.
    let target = parsed.path.ok_or(Refused)?;
    let origin = target.starts_with('/') || target == "*";
    if !origin || parsed.method == Some("CONNECT") {
        return Err(Refused);
    }
    let named = |wanted: &'static str| {
        parsed
            .headers
            .iter()
            .filter(move |header| header.name.eq_ignore_ascii_case(wanted))
            .map(|header| header.value.trim_ascii())
    };
    let mut hosts = named("host");
    match (hosts.next(), hosts.next()) {
        (Some(host), None) if host_name(host) == Some(name.to_owned()) => {}
        _ => return Err(Refused),
    }
    let mut encodings = named("transfer-encoding");
    let mut lengths = named("content-length");
    let body = match (encodings.next(), lengths.next()) {
        (Some(encoding), None) if encodings.next().is_none() => {
            match encoding.eq_ignore_ascii_case(b"chunked") {
                true => Http::ChunkSize,
                false => return Err(Refused),
            }
        }
        (None, Some(length)) if lengths.next().is_none() => {
            let digits = !length.is_empty() && length.iter().all(u8::is_ascii_digit);
            let length = std::str::from_utf8(length).ok().filter(|_| digits);
            match length.and_then(|length| length.parse().ok()) {
                Some(0) => Http::Head,
                Some(length) => Http::Body(length),
                None => return Err(Refused),
            }
        }
        (None, None) => Http::Head,
        _ => return Err(Refused),
    };
    // Once the server switches protocols, what follows passes as bytes.
    if named("upgrade").next().is_some() {
        return Ok(State::Passing);
    }
    Ok(State::Http(body))
}

/// The name in a `Host` header's value, without its port, or `None` if the
/// value isn't a name.
fn host_name(value: &[u8]) -> Option<String> {
    let (name, port) = match value.iter().rposition(|&byte| byte == b':') {
        Some(at) => (&value[..at], Some(&value[at + 1..])),
        None => (value, None),
    };
    let port_ok = port.is_none_or(|port| port.iter().all(u8::is_ascii_digit));
    let name_ok = !name.is_empty()
        && name
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_'));
    (port_ok && name_ok).then(|| normal(name))
}

/// `name` in lowercase, without a final dot.
fn normal(name: &[u8]) -> String {
    let name = name.strip_suffix(b".").unwrap_or(name);
    String::from_utf8_lossy(name).to_ascii_lowercase()
}
