//! The name check, by **Name checks** under NETWORK in SPEC.md: what a
//! connection shows without decrypting must agree with the name it is
//! attributed to. It reads the guest's bytes as they come. A TLS client
//! hello must name the attributed name, if it names any. Plain HTTP must
//! name it in every request's `Host` header. Anything else passes on the
//! attribution alone. On a connection the proxy decrypts, it reads HTTP/1.1
//! the same way, and HTTP/2 request by request.

use std::collections::BTreeMap;

use rustls::server::Acceptor;

use crate::h2::{self, Event, Frame};
use crate::hpack::Header;

/// The most bytes a client hello, a request head, a chunk's size line, or
/// a body's trailers may take, and the most the first line may take before
/// it is known what the connection carries.
const HEAD_LIMIT: usize = 64 * 1024;
/// The most headers a request head, or a body's trailers, may have.
pub(crate) const HEADERS: usize = 128;
/// The longest frame a server may let a client send, by section 4.2 of
/// RFC 9113.
const LONGEST_FRAME: u32 = (1 << 24) - 1;

/// Reads what the guest sends on one connection, and passes the bytes on
/// once they agree with the name.
pub struct NameCheck {
    name: String,
    /// Bytes read and not yet passed.
    held: Vec<u8>,
    state: State,
    /// A request whose `Host` names another host passes.
    any_host: bool,
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
    /// HTTP/2, which the guest and the server agreed inside TLS.
    Http2(Box<Http2>),
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
            any_host: false,
        }
    }

    /// A check for a connection to `name`, a host with URL rules, which
    /// reads only HTTP. Each request must name `name`, as a plain HTTP
    /// request must, and [`NameCheck::feed_deciding`] decides its target.
    pub fn requests(name: &str) -> NameCheck {
        NameCheck {
            state: State::Http(Http::Head),
            ..NameCheck::new(name)
        }
    }

    /// A check for a connection to `name`, a host with URL rules, whose
    /// guest and server agreed HTTP/2, which reads each request as
    /// [`NameCheck::requests`] reads HTTP/1.1: its `:authority` and `Host`
    /// must name `name`, and [`NameCheck::feed_deciding`] decides its
    /// `:path`.
    pub fn http2(name: &str) -> NameCheck {
        NameCheck {
            state: State::Http2(Box::new(Http2::new())),
            ..NameCheck::new(name)
        }
    }

    /// A check for a connection to `name` that the proxy decrypts to put a
    /// hidden secret's value in, which reads only HTTP, as
    /// [`NameCheck::requests`] does. A request whose `Host` names another
    /// host passes too.
    pub fn injecting(name: &str) -> NameCheck {
        NameCheck {
            any_host: true,
            ..NameCheck::requests(name)
        }
    }

    /// Takes `input`, which the guest sent next, and adds the bytes that may
    /// now pass to `out`. Once it refuses, the connection must end, and
    /// nothing more may pass.
    pub fn feed(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<(), Refused> {
        self.feed_deciding(input, out, &mut |_| true)
    }

    /// Takes `input` as [`NameCheck::feed`] does, and passes a request's
    /// head only if `allows` allows its target.
    pub fn feed_deciding(
        &mut self,
        input: &[u8],
        out: &mut Vec<u8>,
        allows: &mut dyn FnMut(&str) -> bool,
    ) -> Result<(), Refused> {
        self.feed_rewriting(input, out, allows, &mut |head, out| {
            out.extend_from_slice(head)
        })
    }

    /// Takes `input` as [`NameCheck::feed_deciding`] does, and passes each
    /// request head through `rewrite`, which adds what passes to `out`.
    pub fn feed_rewriting(
        &mut self,
        input: &[u8],
        out: &mut Vec<u8>,
        allows: &mut dyn FnMut(&str) -> bool,
        rewrite: &mut dyn FnMut(&[u8], &mut Vec<u8>),
    ) -> Result<(), Refused> {
        self.held.extend_from_slice(input);
        let fed = self.read(out, allows, rewrite);
        if fed.is_err() {
            self.state = State::Refused;
            self.held = Vec::new();
        }
        fed
    }

    /// Passes what it can of the bytes held.
    fn read(
        &mut self,
        out: &mut Vec<u8>,
        allows: &mut dyn FnMut(&str) -> bool,
        rewrite: &mut dyn FnMut(&[u8], &mut Vec<u8>),
    ) -> Result<(), Refused> {
        let host = (!self.any_host).then_some(&*self.name);
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
                State::Http2(http2) => {
                    let fed = http2.feed(&self.held, &self.name, out, allows);
                    self.held.clear();
                    return fed;
                }
                State::Http(http) => match request(*http, host, &self.held, allows)? {
                    Some((taken, next)) => {
                        match *http {
                            Http::Head => rewrite(&self.held[..taken], out),
                            _ => out.extend_from_slice(&self.held[..taken]),
                        }
                        self.held.drain(..taken);
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
            || match &self.state {
                State::Http(Http::Head) | State::First | State::Passing | State::Refused => false,
                State::Http2(http2) => http2.reader.holding(),
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
    // A request line starts with a method, or with the whitespace that a
    // lenient server skips. Anything else passes at once, as a binary
    // protocol's first bytes do.
    let method = line.iter().take_while(|&&byte| token(byte)).count();
    match line.get(method) {
        // Vertical tab too, which `is_ascii_whitespace` leaves out.
        Some(byte) if byte.is_ascii_whitespace() || *byte == 0x0b => {}
        None => return short(),
        Some(_) => return Ok(Some(State::Passing)),
    }
    let Some(end) = line.iter().position(|&byte| byte == b'\n') else {
        return short();
    };
    // A line that a lenient server could read as a request reads as HTTP,
    // so the strict parser decides it.
    let version = (line[..end].windows(5)).any(|window| window.eq_ignore_ascii_case(b"http/"));
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
fn request(
    http: Http,
    name: Option<&str>,
    held: &[u8],
    allows: &mut dyn FnMut(&str) -> bool,
) -> Result<Option<(usize, State)>, Refused> {
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
                httparse::Status::Complete(taken) => {
                    Ok(Some((taken, head(&parsed, name, allows)?)))
                }
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
            // httparse reads a line with no size digit as size 0, and lets
            // an extension hold a bare LF, which ends the line for another
            // server.
            httparse::Status::Complete((taken, _))
                if !held[0].is_ascii_hexdigit() || held[..taken - 1].contains(&b'\n') =>
            {
                Err(Refused)
            }
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
                // httparse ends a line at a bare LF too, and a server that
                // doesn't reads the trailers another way.
                httparse::Status::Complete((taken, _)) => {
                    let mut lines = held[..taken].split_inclusive(|&byte| byte == b'\n');
                    match lines.all(|line| line.ends_with(b"\r\n")) {
                        true => Ok(Some((taken, State::Http(Http::Head)))),
                        false => Err(Refused),
                    }
                }
            }
        }
    }
}

/// What follows a request whose head is `parsed`, if its `Host` names
/// `name`, or `name` is `None`, the server would read its body as this
/// reads it, and `allows` allows its target.
fn head(
    parsed: &httparse::Request,
    name: Option<&str>,
    allows: &mut dyn FnMut(&str) -> bool,
) -> Result<State, Refused> {
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
        (Some(host), None) if name.is_none_or(|name| host_name(host).as_deref() == Some(name)) => {}
        _ => return Err(Refused),
    }
    let mut encodings = named("transfer-encoding");
    let mut lengths = named("content-length");
    let body = match (encodings.next(), lengths.next()) {
        // An HTTP/1.0 server may ignore `Transfer-Encoding`.
        (Some(encoding), None) if encodings.next().is_none() && parsed.version == Some(1) => {
            match encoding.eq_ignore_ascii_case(b"chunked") {
                true => Http::ChunkSize,
                false => return Err(Refused),
            }
        }
        (None, Some(length)) if lengths.next().is_none() => match content_length(length) {
            Some(0) => Http::Head,
            Some(length) => Http::Body(length),
            None => return Err(Refused),
        },
        (None, None) => Http::Head,
        _ => return Err(Refused),
    };
    // A server may switch protocols, or ignore the request to, and the
    // check can't see which.
    if named("upgrade").next().is_some() || !allows(target) {
        return Err(Refused);
    }
    Ok(State::Http(body))
}

/// What the guest sends over HTTP/2, read frame by frame by RFC 9113. The
/// server's frames pass unread, so the reader allows what any server could:
/// frames as long as HTTP/2 lets a server allow, and a header table as
/// large as a head may be. What passes is written again, in frames that
/// any server takes, and in header blocks that use no dynamic table, so the
/// server reads exactly the headers that were checked.
struct Http2 {
    reader: h2::Reader,
    streams: h2::Connection,
    writer: h2::Writer,
    /// The body each open request has left, where its `content-length`
    /// gives one.
    lengths: BTreeMap<u32, u64>,
    /// The preface has passed.
    started: bool,
}

impl Http2 {
    fn new() -> Http2 {
        let mut reader = h2::Reader::client(HEAD_LIMIT);
        reader.apply(&[
            (h2::HEADER_TABLE_SIZE, HEAD_LIMIT as u32),
            (h2::MAX_FRAME_SIZE_SETTING, LONGEST_FRAME),
        ]);
        let mut writer = h2::Writer::new();
        writer.apply(&[(h2::HEADER_TABLE_SIZE, 0)]);
        Http2 {
            reader,
            streams: h2::Connection::watching(),
            writer,
            lengths: BTreeMap::new(),
            started: false,
        }
    }

    /// Reads `input`, and adds the frames that pass to `out`, until a
    /// request that doesn't name `name`, that `allows` doesn't allow, or
    /// that isn't well formed, or a stream or connection error.
    fn feed(
        &mut self,
        input: &[u8],
        name: &str,
        out: &mut Vec<u8>,
        allows: &mut dyn FnMut(&str) -> bool,
    ) -> Result<(), Refused> {
        self.reader.push(input);
        while let Some(frame) = self.reader.frame().map_err(|_| Refused)? {
            if !self.started {
                out.extend_from_slice(h2::PREFACE);
                self.started = true;
            }
            self.pass(frame, name, out, allows)?;
        }
        Ok(())
    }

    /// Adds `frame` to `out`, as the guest's side of its stream allows.
    fn pass(
        &mut self,
        frame: Frame,
        name: &str,
        out: &mut Vec<u8>,
        allows: &mut dyn FnMut(&str) -> bool,
    ) -> Result<(), Refused> {
        // What the client tells the server of a stream that has closed on
        // the client's side, which the server still has open.
        let unseen = match frame {
            Frame::WindowUpdate { stream, increment } => {
                Some(Frame::WindowUpdate { stream, increment })
            }
            Frame::RstStream { stream, code } => Some(Frame::RstStream { stream, code }),
            _ => None,
        };
        let padding = match &frame {
            Frame::Data { padding, .. } => *padding,
            _ => None,
        };
        let frame = match self.streams.receive(frame).map_err(|_| Refused)? {
            Some(Event::Request {
                stream,
                headers,
                end_stream,
            }) => {
                if let Some(length) = http2_request(&headers, name, allows)? {
                    self.lengths.insert(stream, length);
                }
                if end_stream {
                    self.ended(stream)?;
                }
                Frame::Headers {
                    stream,
                    headers,
                    priority: None,
                    end_stream,
                }
            }
            // Trailers end their stream, by section 8.1.
            Some(Event::Trailers {
                stream,
                headers,
                end_stream,
            }) => {
                if !end_stream || !headers.iter().all(trailer) {
                    return Err(Refused);
                }
                self.ended(stream)?;
                Frame::Headers {
                    stream,
                    headers,
                    priority: None,
                    end_stream,
                }
            }
            Some(Event::Data {
                stream,
                data,
                end_stream,
            }) => {
                if let Some(left) = self.lengths.get_mut(&stream) {
                    *left = (left.checked_sub(data.len() as u64)).ok_or(Refused)?;
                }
                if end_stream {
                    self.ended(stream)?;
                }
                self.data(stream, &data, padding, end_stream, out);
                return Ok(());
            }
            Some(Event::Reset { stream, code }) => {
                self.lengths.remove(&stream);
                Frame::RstStream { stream, code }
            }
            // The server never pushes, so nothing comes on a stream that
            // the check doesn't know.
            Some(Event::Connection(Frame::Settings(settings))) => {
                let mut settings: Vec<_> = (settings.into_iter())
                    .filter(|&(id, _)| id != h2::ENABLE_PUSH)
                    .collect();
                settings.push((h2::ENABLE_PUSH, 0));
                if settings.len() * 6 > h2::MAX_FRAME_SIZE {
                    return Err(Refused);
                }
                Frame::Settings(settings)
            }
            // Its debug data is for the server's logs, and may not fit.
            Some(Event::Connection(Frame::GoAway {
                last_stream, code, ..
            })) => Frame::GoAway {
                last_stream,
                code,
                debug: Vec::new(),
            },
            Some(Event::Connection(frame)) => frame,
            // PRIORITY, which RFC 9113 deprecates, goes no further.
            None => match unseen {
                Some(unseen) => unseen,
                None => return Ok(()),
            },
        };
        self.writer.write(&frame, out);
        Ok(())
    }

    /// The guest ended `stream`, whose body must have the length it gave.
    fn ended(&mut self, stream: u32) -> Result<(), Refused> {
        match self.lengths.remove(&stream) {
            Some(left) if left > 0 => Err(Refused),
            _ => Ok(()),
        }
    }

    /// Adds `data` to `out` in frames that any server takes, the last with
    /// `padding`, so the server's windows count what the guest's do.
    fn data(
        &mut self,
        stream: u32,
        data: &[u8],
        padding: Option<u8>,
        end_stream: bool,
        out: &mut Vec<u8>,
    ) {
        let padded = padding.map_or(0, |padding| usize::from(padding) + 1);
        let mut rest = data;
        while rest.len() + padded > h2::MAX_FRAME_SIZE {
            let (part, after) = rest.split_at(rest.len().min(h2::MAX_FRAME_SIZE));
            let part = Frame::Data {
                stream,
                data: part.to_vec(),
                padding: None,
                end_stream: false,
            };
            self.writer.write(&part, out);
            rest = after;
        }
        let last = Frame::Data {
            stream,
            data: rest.to_vec(),
            padding,
            end_stream,
        };
        self.writer.write(&last, out);
    }
}

/// The body length that `headers`, a request's, give, if `allows` allows
/// its `:path`, and the server reads it as this reads it: its fields are
/// well formed, by section 8.2 of RFC 9113, and so are its pseudo-header
/// fields, by section 8.3.1. It isn't `CONNECT`, it names `name` in its
/// `:authority`, its one `Host`, or both, and its `:path` is in origin
/// form.
fn http2_request(
    headers: &[Header],
    name: &str,
    allows: &mut dyn FnMut(&str) -> bool,
) -> Result<Option<u64>, Refused> {
    let pseudo = (headers.iter()).take_while(|header| header.name.starts_with(b":"));
    let (mut method, mut scheme, mut authority, mut path) = (None, None, None, None);
    let mut count = 0;
    for header in pseudo {
        count += 1;
        let field = match &header.name[..] {
            b":method" => &mut method,
            b":scheme" => &mut scheme,
            b":authority" => &mut authority,
            b":path" => &mut path,
            _ => return Err(Refused),
        };
        if field.replace(&header.value[..]).is_some() || !field_value(&header.value) {
            return Err(Refused);
        }
    }
    let fields = &headers[count..];
    if !fields
        .iter()
        .all(|header| field(header) && !framing(header))
    {
        return Err(Refused);
    }
    let method = method.filter(|method| !method.is_empty() && method.iter().all(|&b| token(b)));
    let (Some(method), Some(b"https"), Some(path)) = (method, scheme, path) else {
        return Err(Refused);
    };
    let origin = path.starts_with(b"/") || path == b"*";
    let visible = path.iter().all(u8::is_ascii_graphic);
    if method == b"CONNECT" || !origin || !visible {
        return Err(Refused);
    }
    let named = |wanted: &'static [u8]| {
        (fields.iter())
            .filter(move |header| header.name == wanted)
            .map(|header| &header.value[..])
    };
    let mut hosts = named(b"host");
    let host = hosts.next();
    if hosts.next().is_some() || (authority.is_none() && host.is_none()) {
        return Err(Refused);
    }
    let names = |value: &[u8]| host_name(value).as_deref() == Some(name);
    if !authority.into_iter().chain(host).all(names) {
        return Err(Refused);
    }
    let mut lengths = named(b"content-length");
    let length = match (lengths.next(), lengths.next()) {
        (None, _) => None,
        (Some(length), None) => Some(content_length(length).ok_or(Refused)?),
        _ => return Err(Refused),
    };
    let path = std::str::from_utf8(path).map_err(|_| Refused)?;
    match allows(path) {
        true => Ok(length),
        false => Err(Refused),
    }
}

/// The length in a `Content-Length` value, which must be digits alone.
fn content_length(value: &[u8]) -> Option<u64> {
    let digits = !value.is_empty() && value.iter().all(u8::is_ascii_digit);
    let value = std::str::from_utf8(value).ok().filter(|_| digits)?;
    value.parse().ok()
}

/// Whether `header` may be in a body's trailers: a well-formed field, and
/// none that frames the message or names its host.
fn trailer(header: &Header) -> bool {
    let named = |name: &[u8]| header.name == name;
    field(header) && !framing(header) && !named(b"host") && !named(b"content-length")
}

/// Whether `header` is a field that is well formed in HTTP/2, by section
/// 8.2.1 of RFC 9113: its name is a token in lowercase, and its value has
/// no line break or other control character, and no whitespace at either
/// end.
fn field(header: &Header) -> bool {
    let name = &header.name;
    let lowercase = (name.iter()).all(|&byte| token(byte) && !byte.is_ascii_uppercase());
    !name.is_empty() && lowercase && field_value(&header.value)
}

fn field_value(value: &[u8]) -> bool {
    let space = |byte: Option<&u8>| byte.is_some_and(|byte| matches!(byte, b' ' | b'\t'));
    let plain = (value.iter()).all(|&byte| byte == b'\t' || (byte >= 0x20 && byte != 0x7f));
    plain && !space(value.first()) && !space(value.last())
}

/// Whether `header` is a field that HTTP/2 leaves to the connection, by
/// section 8.2.2 of RFC 9113, which a server could read as framing the
/// message.
fn framing(header: &Header) -> bool {
    let connection = [
        &b"connection"[..],
        b"proxy-connection",
        b"keep-alive",
        b"transfer-encoding",
        b"upgrade",
    ];
    let te = header.name == b"te" && header.value != b"trailers";
    te || connection.contains(&&header.name[..])
}

/// The name in a `Host` header's value, without its port, or `None` if the
/// value isn't a name.
pub(crate) fn host_name(value: &[u8]) -> Option<String> {
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

/// Whether `byte` may be in a method, as RFC 9110 says.
fn token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

/// `name` in lowercase, without a final dot.
fn normal(name: &[u8]) -> String {
    let name = name.strip_suffix(b".").unwrap_or(name);
    String::from_utf8_lossy(name).to_ascii_lowercase()
}
