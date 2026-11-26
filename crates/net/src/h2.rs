//! HTTP/2 frames, by RFC 9113: the connection preface, the frame header,
//! and the payload of each frame type in section 6. A header block's
//! CONTINUATION frames are joined to the HEADERS or PUSH_PROMISE frame they
//! continue, and the block is decoded by HPACK. Every frame the proxy reads
//! comes from the guest, so the reader is strict: a frame that RFC 9113
//! calls a connection error is an error, never a guess. After an error, the
//! reader refuses every later byte. Streams and flow control are the
//! caller's, and so is telling the reader and the writer which SETTINGS
//! each side has acknowledged.

use crate::hpack::{self, Decoder, Encoder, Header};

/// What a client sends first, by section 3.4.
pub const PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// SETTINGS_MAX_FRAME_SIZE before SETTINGS change it, by section 6.5.2.
const MAX_FRAME_SIZE: usize = 1 << 14;
/// HPACK's table size before SETTINGS change it, by section 6.5.2.
const TABLE_SIZE: usize = 4096;
const HEAD: usize = 9;

const DATA: u8 = 0x0;
const HEADERS: u8 = 0x1;
const PRIORITY: u8 = 0x2;
const RST_STREAM: u8 = 0x3;
const SETTINGS: u8 = 0x4;
const PUSH_PROMISE: u8 = 0x5;
const PING: u8 = 0x6;
const GOAWAY: u8 = 0x7;
const WINDOW_UPDATE: u8 = 0x8;
const CONTINUATION: u8 = 0x9;

const END_STREAM: u8 = 0x1;
const ACK: u8 = 0x1;
const END_HEADERS: u8 = 0x4;
const PADDED: u8 = 0x8;
const PRIORITY_FLAG: u8 = 0x20;

const HEADER_TABLE_SIZE: u16 = 0x1;
const ENABLE_PUSH: u16 = 0x2;
const INITIAL_WINDOW_SIZE: u16 = 0x4;
const MAX_FRAME_SIZE_SETTING: u16 = 0x5;

/// The reserved bit of a stream id, which a reader ignores, by section 4.1.
const STREAM_MASK: u32 = 0x7fff_ffff;

/// A frame, with the fields that section 6 gives its type. A stream id has
/// no reserved bit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    /// Section 6.1. `padding` is how many bytes of padding it had. Flow
    /// control counts the whole payload: the data, the padding, and the
    /// Pad Length byte if it has one.
    Data {
        stream: u32,
        data: Vec<u8>,
        padding: Option<u8>,
        end_stream: bool,
    },
    /// Section 6.2, with the block of any CONTINUATION frames after it,
    /// decoded.
    Headers {
        stream: u32,
        headers: Vec<Header>,
        priority: Option<Priority>,
        end_stream: bool,
    },
    /// Section 6.3.
    Priority { stream: u32, priority: Priority },
    /// Section 6.4.
    RstStream { stream: u32, code: u32 },
    /// Section 6.5: each setting's id and value, in order, including ids
    /// that section 6.5.2 doesn't define.
    Settings(Vec<(u16, u32)>),
    /// Section 6.5, with the ACK flag.
    SettingsAck,
    /// Section 6.6, with the block of any CONTINUATION frames after it,
    /// decoded.
    PushPromise {
        stream: u32,
        promised: u32,
        headers: Vec<Header>,
    },
    /// Section 6.7.
    Ping { ack: bool, data: [u8; 8] },
    /// Section 6.8.
    GoAway {
        last_stream: u32,
        code: u32,
        debug: Vec<u8>,
    },
    /// Section 6.9. An increment of 0 on a stream is a stream error, which
    /// is the caller's.
    WindowUpdate { stream: u32, increment: u32 },
}

/// The priority fields of HEADERS and PRIORITY, by section 5.3.2 of RFC
/// 7540, which RFC 9113 keeps the syntax of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Priority {
    pub exclusive: bool,
    pub dependency: u32,
    pub weight: u8,
}

/// Why the reader refused a connection. `code` is the error code of the
/// GOAWAY that says so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The client's preface isn't the string, or the first frame isn't
    /// SETTINGS without ACK, by section 3.4.
    Preface,
    /// A frame past SETTINGS_MAX_FRAME_SIZE, of a length its type doesn't
    /// allow, or too short for its fields, by section 4.2. Section 6.3
    /// makes a PRIORITY frame of the wrong length a stream error, and the
    /// reader makes it a connection error, as section 5.4 allows.
    FrameSize,
    /// Padding as long as what is left of the payload, or longer, by
    /// sections 6.1, 6.2, and 6.6.
    Padding,
    /// A stream id of 0 on a frame that needs a stream, or another on one
    /// that is for the connection, or a promised stream id of 0.
    StreamId,
    /// A setting's value that section 6.5.2 doesn't allow, but for
    /// SETTINGS_INITIAL_WINDOW_SIZE.
    Settings,
    /// A SETTINGS_INITIAL_WINDOW_SIZE past 2^31 - 1, by section 6.5.2.
    InitialWindowSize,
    /// An increment of 0 on the connection, by section 6.9.
    WindowUpdate,
    /// A frame inside a header block other than its CONTINUATION, or a
    /// CONTINUATION outside one, by section 6.10.
    Continuation,
    /// A PUSH_PROMISE from a client, by section 8.4, or from a server once
    /// it has acknowledged SETTINGS_ENABLE_PUSH of 0, by section 6.6.
    Push,
    /// HPACK refused a header block, by section 4.3. It is never
    /// `hpack::Error::ListSize`.
    Hpack(hpack::Error),
    /// A joined header block longer than the list limit, or headers that
    /// take more than it. Section 10.5 lets the reader refuse that as load.
    ListSize,
}

impl Error {
    /// The error code, by section 7.
    pub fn code(self) -> u32 {
        const PROTOCOL_ERROR: u32 = 0x1;
        const FLOW_CONTROL_ERROR: u32 = 0x3;
        const FRAME_SIZE_ERROR: u32 = 0x6;
        const COMPRESSION_ERROR: u32 = 0x9;
        const ENHANCE_YOUR_CALM: u32 = 0xb;
        match self {
            Error::FrameSize => FRAME_SIZE_ERROR,
            Error::InitialWindowSize => FLOW_CONTROL_ERROR,
            Error::Hpack(_) => COMPRESSION_ERROR,
            Error::ListSize => ENHANCE_YOUR_CALM,
            Error::Preface
            | Error::Padding
            | Error::StreamId
            | Error::Settings
            | Error::WindowUpdate
            | Error::Continuation
            | Error::Push => PROTOCOL_ERROR,
        }
    }
}

/// Reads the frames of one direction of one connection.
pub struct Reader {
    held: Vec<u8>,
    /// Where the next frame starts in `held`.
    at: usize,
    /// How much of the preface has come.
    preface: usize,
    state: State,
    failed: Option<Error>,
}

/// What the reader knows of the connection.
struct State {
    from_server: bool,
    /// Whether a server may push, until it acknowledges that it may not.
    push: bool,
    max_frame_size: usize,
    /// Whether the first SETTINGS frame has come.
    settings: bool,
    decoder: Decoder,
    list_limit: usize,
    /// The block that CONTINUATION frames continue.
    open: Option<Open>,
}

/// A header block whose END_HEADERS hasn't come.
struct Open {
    stream: u32,
    block: Vec<u8>,
    kind: Opened,
}

enum Opened {
    Headers {
        priority: Option<Priority>,
        end_stream: bool,
    },
    Push {
        promised: u32,
    },
}

impl Reader {
    /// Reads what a client sends, from its preface on. A header block's
    /// headers may take `list_limit`, counted as HPACK counts them, and so
    /// may the block.
    pub fn client(list_limit: usize) -> Reader {
        Reader::new(false, list_limit)
    }

    /// Reads what a server sends, from its first SETTINGS frame on.
    pub fn server(list_limit: usize) -> Reader {
        Reader::new(true, list_limit)
    }

    fn new(from_server: bool, list_limit: usize) -> Reader {
        Reader {
            held: Vec::new(),
            at: 0,
            preface: if from_server { PREFACE.len() } else { 0 },
            state: State {
                from_server,
                push: from_server,
                max_frame_size: MAX_FRAME_SIZE,
                settings: false,
                decoder: Decoder::new(TABLE_SIZE, list_limit),
                list_limit,
                open: None,
            },
            failed: None,
        }
    }

    /// Applies the settings the caller sent, once the frame that
    /// acknowledges them has come: SETTINGS_HEADER_TABLE_SIZE,
    /// SETTINGS_ENABLE_PUSH, and SETTINGS_MAX_FRAME_SIZE, by section 6.5.3.
    /// It ignores the rest, and takes each value to be one the reader
    /// accepts.
    pub fn apply(&mut self, settings: &[(u16, u32)]) {
        for &(id, value) in settings {
            match id {
                HEADER_TABLE_SIZE => self.state.decoder.set_table_limit(value as usize),
                ENABLE_PUSH => self.state.push = self.state.from_server && value == 1,
                MAX_FRAME_SIZE_SETTING => self.state.max_frame_size = value as usize,
                _ => {}
            }
        }
    }

    /// Adds bytes from the connection. The reader keeps them until
    /// `frame` reads them, so the caller reads before it adds more.
    pub fn push(&mut self, bytes: &[u8]) {
        if self.failed.is_some() {
            return;
        }
        self.held.drain(..self.at);
        self.at = 0;
        self.held.extend_from_slice(bytes);
    }

    /// The next whole frame, or None until more bytes come. A frame of an
    /// unknown type is skipped, by section 4.1, and so is each frame of a
    /// header block but its last.
    pub fn frame(&mut self) -> Result<Option<Frame>, Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let read = self.read();
        if let Err(error) = read {
            self.failed = Some(error);
            self.held = Vec::new();
            self.at = 0;
        }
        read
    }

    fn read(&mut self) -> Result<Option<Frame>, Error> {
        if self.preface < PREFACE.len() {
            let rest = &self.held[self.at..];
            let expected = &PREFACE[self.preface..];
            let length = rest.len().min(expected.len());
            if rest[..length] != expected[..length] {
                return Err(Error::Preface);
            }
            self.preface += length;
            self.at += length;
            if self.preface < PREFACE.len() {
                return Ok(None);
            }
        }
        loop {
            let rest = &self.held[self.at..];
            let Some(head) = rest.first_chunk::<HEAD>() else {
                return Ok(None);
            };
            let length = u32::from_be_bytes([0, head[0], head[1], head[2]]) as usize;
            if length > self.state.max_frame_size {
                return Err(Error::FrameSize);
            }
            let Some(payload) = rest[HEAD..].get(..length) else {
                return Ok(None);
            };
            let stream = u32::from_be_bytes([head[5], head[6], head[7], head[8]]) & STREAM_MASK;
            let frame = self.state.frame(head[3], head[4], stream, payload)?;
            self.at += HEAD + length;
            if frame.is_some() {
                return Ok(frame);
            }
        }
    }
}

impl State {
    /// The frame of this header and payload, or None if it is skipped.
    fn frame(
        &mut self,
        kind: u8,
        flags: u8,
        stream: u32,
        payload: &[u8],
    ) -> Result<Option<Frame>, Error> {
        if let Some(open) = &mut self.open {
            if kind != CONTINUATION || stream != open.stream {
                return Err(Error::Continuation);
            }
            return self.extend(flags, payload);
        }
        if !self.settings {
            if kind != SETTINGS || flags & ACK != 0 {
                return Err(Error::Preface);
            }
            self.settings = true;
        }
        let on_stream = || match stream {
            0 => Err(Error::StreamId),
            _ => Ok(()),
        };
        let on_connection = || match stream {
            0 => Ok(()),
            _ => Err(Error::StreamId),
        };
        let mut payload = Payload(payload);
        let frame = match kind {
            DATA => {
                on_stream()?;
                let padding = payload.padding(flags)?;
                Frame::Data {
                    stream,
                    data: payload.unpad(padding)?.to_vec(),
                    padding,
                    end_stream: flags & END_STREAM != 0,
                }
            }
            HEADERS => {
                on_stream()?;
                let padding = payload.padding(flags)?;
                let priority = match flags & PRIORITY_FLAG {
                    0 => None,
                    _ => Some(priority(payload.take::<5>()?)),
                };
                let block = payload.unpad(padding)?;
                let end_stream = flags & END_STREAM != 0;
                let kind = Opened::Headers {
                    priority,
                    end_stream,
                };
                return self.open(stream, kind, flags, block);
            }
            PRIORITY => {
                on_stream()?;
                Frame::Priority {
                    stream,
                    priority: priority(payload.exactly::<5>()?),
                }
            }
            RST_STREAM => {
                on_stream()?;
                let code = u32::from_be_bytes(*payload.exactly::<4>()?);
                Frame::RstStream { stream, code }
            }
            SETTINGS => {
                on_connection()?;
                if flags & ACK != 0 {
                    payload.exactly::<0>()?;
                    Frame::SettingsAck
                } else {
                    Frame::Settings(self.settings(payload.0)?)
                }
            }
            PUSH_PROMISE => {
                if !self.push {
                    return Err(Error::Push);
                }
                on_stream()?;
                let padding = payload.padding(flags)?;
                let promised = u32::from_be_bytes(*payload.take::<4>()?) & STREAM_MASK;
                if promised == 0 {
                    return Err(Error::StreamId);
                }
                let block = payload.unpad(padding)?;
                return self.open(stream, Opened::Push { promised }, flags, block);
            }
            PING => {
                on_connection()?;
                Frame::Ping {
                    ack: flags & ACK != 0,
                    data: *payload.exactly::<8>()?,
                }
            }
            GOAWAY => {
                on_connection()?;
                let last_stream = u32::from_be_bytes(*payload.take::<4>()?) & STREAM_MASK;
                let code = u32::from_be_bytes(*payload.take::<4>()?);
                Frame::GoAway {
                    last_stream,
                    code,
                    debug: payload.0.to_vec(),
                }
            }
            WINDOW_UPDATE => {
                let increment = u32::from_be_bytes(*payload.exactly::<4>()?) & STREAM_MASK;
                if stream == 0 && increment == 0 {
                    return Err(Error::WindowUpdate);
                }
                Frame::WindowUpdate { stream, increment }
            }
            CONTINUATION => {
                on_stream()?;
                return Err(Error::Continuation);
            }
            _ => return Ok(None),
        };
        Ok(Some(frame))
    }

    /// Each setting of a SETTINGS payload, by section 6.5.1, checked by
    /// section 6.5.2.
    fn settings(&self, payload: &[u8]) -> Result<Vec<(u16, u32)>, Error> {
        let (chunks, rest) = payload.as_chunks::<6>();
        if !rest.is_empty() {
            return Err(Error::FrameSize);
        }
        let mut settings = Vec::with_capacity(chunks.len());
        for setting in chunks {
            let id = u16::from_be_bytes([setting[0], setting[1]]);
            let value = u32::from_be_bytes([setting[2], setting[3], setting[4], setting[5]]);
            let allowed = match id {
                // A server never enables push.
                ENABLE_PUSH => value == 0 || (value == 1 && !self.from_server),
                INITIAL_WINDOW_SIZE if value > STREAM_MASK => {
                    return Err(Error::InitialWindowSize);
                }
                MAX_FRAME_SIZE_SETTING => (1 << 14..1 << 24).contains(&value),
                _ => true,
            };
            if !allowed {
                return Err(Error::Settings);
            }
            settings.push((id, value));
        }
        Ok(settings)
    }

    /// Starts a header block with its first fragment.
    fn open(
        &mut self,
        stream: u32,
        kind: Opened,
        flags: u8,
        fragment: &[u8],
    ) -> Result<Option<Frame>, Error> {
        self.open = Some(Open {
            stream,
            block: Vec::new(),
            kind,
        });
        self.extend(flags, fragment)
    }

    /// Adds a fragment to the open block, and decodes the block at its end.
    fn extend(&mut self, flags: u8, fragment: &[u8]) -> Result<Option<Frame>, Error> {
        let open = self.open.as_mut().expect("an open block");
        if open.block.len() + fragment.len() > self.list_limit {
            return Err(Error::ListSize);
        }
        open.block.extend_from_slice(fragment);
        if flags & END_HEADERS == 0 {
            return Ok(None);
        }
        let open = self.open.take().expect("an open block");
        let headers = (self.decoder.decode(&open.block)).map_err(|error| match error {
            hpack::Error::ListSize => Error::ListSize,
            error => Error::Hpack(error),
        })?;
        let stream = open.stream;
        Ok(Some(match open.kind {
            Opened::Headers {
                priority,
                end_stream,
            } => Frame::Headers {
                stream,
                headers,
                priority,
                end_stream,
            },
            Opened::Push { promised } => Frame::PushPromise {
                stream,
                promised,
                headers,
            },
        }))
    }
}

/// What is left of a payload.
struct Payload<'a>(&'a [u8]);

impl<'a> Payload<'a> {
    /// The next `N` bytes, or FrameSize if the payload is too short.
    fn take<const N: usize>(&mut self) -> Result<&'a [u8; N], Error> {
        let (taken, rest) = self.0.split_first_chunk::<N>().ok_or(Error::FrameSize)?;
        self.0 = rest;
        Ok(taken)
    }

    /// The whole payload, which must be `N` bytes.
    fn exactly<const N: usize>(&mut self) -> Result<&'a [u8; N], Error> {
        self.0.try_into().map_err(|_| Error::FrameSize)
    }

    /// The Pad Length field, if the frame has the PADDED flag.
    fn padding(&mut self, flags: u8) -> Result<Option<u8>, Error> {
        if flags & PADDED == 0 {
            return Ok(None);
        }
        let [padding] = self.take::<1>().map_err(|_| Error::Padding)?;
        Ok(Some(*padding))
    }

    /// What is left before `padding` bytes of padding.
    fn unpad(&self, padding: Option<u8>) -> Result<&'a [u8], Error> {
        let padding = usize::from(padding.unwrap_or(0));
        let length = (self.0.len().checked_sub(padding)).ok_or(Error::Padding)?;
        Ok(&self.0[..length])
    }
}

fn priority(fields: &[u8; 5]) -> Priority {
    let dependency = u32::from_be_bytes([fields[0], fields[1], fields[2], fields[3]]);
    Priority {
        exclusive: dependency & !STREAM_MASK != 0,
        dependency: dependency & STREAM_MASK,
        weight: fields[4],
    }
}

/// Writes the frames of one direction of one connection, within the peer's
/// SETTINGS_MAX_FRAME_SIZE. It splits a header block across CONTINUATION
/// frames where it needs to, and pads nothing but DATA. It panics on any
/// other frame that doesn't fit, so the caller splits DATA, and keeps
/// SETTINGS and GOAWAY within one frame.
pub struct Writer {
    encoder: Encoder,
    max_frame_size: usize,
}

impl Default for Writer {
    fn default() -> Writer {
        Writer::new()
    }
}

impl Writer {
    pub fn new() -> Writer {
        Writer {
            encoder: Encoder::new(TABLE_SIZE),
            max_frame_size: MAX_FRAME_SIZE,
        }
    }

    /// Applies the peer's settings, once the caller has written the frame
    /// that acknowledges them: SETTINGS_HEADER_TABLE_SIZE and
    /// SETTINGS_MAX_FRAME_SIZE, by section 6.5.3. It ignores the rest, and
    /// takes each value to be one the reader accepts.
    pub fn apply(&mut self, settings: &[(u16, u32)]) {
        for &(id, value) in settings {
            match id {
                HEADER_TABLE_SIZE => self.encoder.set_table_limit(value as usize),
                MAX_FRAME_SIZE_SETTING => self.max_frame_size = value as usize,
                _ => {}
            }
        }
    }

    /// Appends `frame` to `out`.
    pub fn write(&mut self, frame: &Frame, out: &mut Vec<u8>) {
        let max = self.max_frame_size;
        let flag = |set: bool, flag: u8| if set { flag } else { 0 };
        match frame {
            Frame::Data {
                stream,
                data,
                padding,
                end_stream,
            } => {
                let flags = flag(*end_stream, END_STREAM) | flag(padding.is_some(), PADDED);
                let padding = padding.map(usize::from);
                let length = data.len() + padding.map_or(0, |padding| padding + 1);
                head(out, max, length, DATA, flags, *stream);
                if let Some(padding) = padding {
                    out.push(padding as u8);
                    out.extend_from_slice(data);
                    out.resize(out.len() + padding, 0);
                } else {
                    out.extend_from_slice(data);
                }
            }
            Frame::Headers {
                stream,
                headers,
                priority,
                end_stream,
            } => {
                let mut fields = Vec::new();
                if let Some(priority) = priority {
                    priority_fields(&mut fields, priority);
                }
                let flags = flag(*end_stream, END_STREAM) | flag(priority.is_some(), PRIORITY_FLAG);
                self.block(out, HEADERS, flags, *stream, &fields, headers);
            }
            Frame::Priority { stream, priority } => {
                head(out, max, 5, PRIORITY, 0, *stream);
                priority_fields(out, priority);
            }
            Frame::RstStream { stream, code } => {
                head(out, max, 4, RST_STREAM, 0, *stream);
                out.extend(code.to_be_bytes());
            }
            Frame::Settings(settings) => {
                head(out, max, settings.len() * 6, SETTINGS, 0, 0);
                for (id, value) in settings {
                    out.extend(id.to_be_bytes());
                    out.extend(value.to_be_bytes());
                }
            }
            Frame::SettingsAck => head(out, max, 0, SETTINGS, ACK, 0),
            Frame::PushPromise {
                stream,
                promised,
                headers,
            } => {
                let fields = promised.to_be_bytes();
                self.block(out, PUSH_PROMISE, 0, *stream, &fields, headers);
            }
            Frame::Ping { ack, data } => {
                head(out, max, 8, PING, flag(*ack, ACK), 0);
                out.extend(data);
            }
            Frame::GoAway {
                last_stream,
                code,
                debug,
            } => {
                head(out, max, 8 + debug.len(), GOAWAY, 0, 0);
                out.extend(last_stream.to_be_bytes());
                out.extend(code.to_be_bytes());
                out.extend_from_slice(debug);
            }
            Frame::WindowUpdate { stream, increment } => {
                head(out, max, 4, WINDOW_UPDATE, 0, *stream);
                out.extend(increment.to_be_bytes());
            }
        }
    }

    /// Writes a HEADERS or PUSH_PROMISE frame with `fields` before its
    /// block, then as many CONTINUATION frames as the rest of the block
    /// takes.
    fn block(
        &mut self,
        out: &mut Vec<u8>,
        kind: u8,
        flags: u8,
        stream: u32,
        fields: &[u8],
        headers: &[Header],
    ) {
        let max = self.max_frame_size;
        let mut block = Vec::new();
        self.encoder.encode(headers, &mut block);
        let (first, mut rest) = block.split_at(block.len().min(max - fields.len()));
        let end = |rest: &[u8]| if rest.is_empty() { END_HEADERS } else { 0 };
        head(
            out,
            max,
            fields.len() + first.len(),
            kind,
            flags | end(rest),
            stream,
        );
        out.extend_from_slice(fields);
        out.extend_from_slice(first);
        while !rest.is_empty() {
            let (fragment, after) = rest.split_at(rest.len().min(max));
            rest = after;
            head(out, max, fragment.len(), CONTINUATION, end(rest), stream);
            out.extend_from_slice(fragment);
        }
    }
}

/// Appends a frame header, by section 4.1. A frame longer than `max` is a
/// bug in the caller, and would desync the connection, so it panics.
fn head(out: &mut Vec<u8>, max: usize, length: usize, kind: u8, flags: u8, stream: u32) {
    assert!(
        length <= max,
        "a frame of {length} bytes is past SETTINGS_MAX_FRAME_SIZE"
    );
    out.extend(&(length as u32).to_be_bytes()[1..]);
    out.extend([kind, flags]);
    out.extend(stream.to_be_bytes());
}

fn priority_fields(out: &mut Vec<u8>, priority: &Priority) {
    let exclusive = if priority.exclusive { !STREAM_MASK } else { 0 };
    out.extend((priority.dependency | exclusive).to_be_bytes());
    out.push(priority.weight);
}
