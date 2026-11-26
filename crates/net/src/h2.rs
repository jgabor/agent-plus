//! HTTP/2 frames, by RFC 9113: the connection preface, the frame header,
//! and the payload of each frame type in section 6. A header block's
//! CONTINUATION frames are joined to the HEADERS or PUSH_PROMISE frame they
//! continue, and the block is decoded by HPACK. Every frame the proxy reads
//! comes from the guest, so the reader is strict: a frame that RFC 9113
//! calls a connection error is an error, never a guess. After an error, the
//! reader refuses every later byte. A `Connection` keeps each stream's
//! state and both sides' flow-control windows, by sections 5.1 and 6.9.
//! Telling each of them which SETTINGS each side has acknowledged is the
//! caller's.

use std::collections::{BTreeMap, VecDeque};

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
const MAX_CONCURRENT_STREAMS: u16 = 0x3;
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
    /// `Connection` refuses.
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

/// Why the reader or a `Connection` refused a frame. `code` is the error
/// code of the GOAWAY or RST_STREAM that says so.
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
    /// that is for the connection, or a promised stream id of 0. Or HEADERS
    /// from a client on a stream id that is even, or not above the last
    /// stream it opened, by section 5.1.1.
    StreamId,
    /// A setting's value that section 6.5.2 doesn't allow, but for
    /// SETTINGS_INITIAL_WINDOW_SIZE.
    Settings,
    /// A SETTINGS_INITIAL_WINDOW_SIZE past 2^31 - 1, by section 6.5.2.
    InitialWindowSize,
    /// An increment of 0, by section 6.9. On a stream, it is a stream
    /// error.
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
    /// A frame other than HEADERS or PRIORITY on a stream that is idle, by
    /// section 5.1. A server that never pushes has every even stream idle.
    Idle,
    /// DATA or HEADERS on a stream after the client ended it, a stream
    /// error by section 5.1. On a closed stream, it is a connection error,
    /// which section 5.1 allows.
    StreamClosed,
    /// HEADERS that open a stream past SETTINGS_MAX_CONCURRENT_STREAMS, a
    /// stream error by section 5.1.2.
    RefusedStream,
    /// DATA past a flow-control window, by section 6.9.1, or a
    /// WINDOW_UPDATE or a change to SETTINGS_INITIAL_WINDOW_SIZE that takes
    /// a window past 2^31 - 1, by sections 6.9.1 and 6.9.2. On a stream's
    /// window, but for a change of SETTINGS, it is a stream error.
    FlowControl,
}

impl Error {
    /// The error code, by section 7.
    pub fn code(self) -> u32 {
        const PROTOCOL_ERROR: u32 = 0x1;
        const FLOW_CONTROL_ERROR: u32 = 0x3;
        const STREAM_CLOSED: u32 = 0x5;
        const FRAME_SIZE_ERROR: u32 = 0x6;
        const REFUSED_STREAM: u32 = 0x7;
        const COMPRESSION_ERROR: u32 = 0x9;
        const ENHANCE_YOUR_CALM: u32 = 0xb;
        match self {
            Error::FrameSize => FRAME_SIZE_ERROR,
            Error::InitialWindowSize | Error::FlowControl => FLOW_CONTROL_ERROR,
            Error::StreamClosed => STREAM_CLOSED,
            Error::RefusedStream => REFUSED_STREAM,
            Error::Hpack(_) => COMPRESSION_ERROR,
            Error::ListSize => ENHANCE_YOUR_CALM,
            Error::Idle
            | Error::Preface
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
                head(out, max, flow_length(data, *padding), DATA, flags, *stream);
                let padding = padding.map(usize::from);
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

/// The length of a DATA frame's payload, which flow control counts: the
/// data, the padding, and the Pad Length byte, by section 6.1.
fn flow_length(data: &[u8], padding: Option<u8>) -> usize {
    data.len() + padding.map_or(0, |padding| usize::from(padding) + 1)
}

fn priority_fields(out: &mut Vec<u8>, priority: &Priority) {
    let exclusive = if priority.exclusive { !STREAM_MASK } else { 0 };
    out.extend((priority.dependency | exclusive).to_be_bytes());
    out.push(priority.weight);
}

/// The window of a new connection and its streams, by section 6.9.2.
const WINDOW: i64 = 65_535;
/// The largest a window can be, by section 6.9.1.
const MAX_WINDOW: i64 = STREAM_MASK as i64;
/// How many of the streams it reset a `Connection` remembers.
const RESETS: usize = 64;

/// What a frame means for its stream, by section 5.1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// HEADERS that open a stream: a request.
    Request {
        stream: u32,
        headers: Vec<Header>,
        end_stream: bool,
    },
    /// HEADERS on a stream that is open, which section 8.1 calls trailers.
    /// Trailers without `end_stream` are malformed, by section 8.1, which
    /// is the caller's to refuse.
    Trailers {
        stream: u32,
        headers: Vec<Header>,
        end_stream: bool,
    },
    /// DATA, without its padding. The caller says when it has consumed it.
    Data {
        stream: u32,
        data: Vec<u8>,
        end_stream: bool,
    },
    /// RST_STREAM: the client closed the stream.
    Reset { stream: u32, code: u32 },
    /// A frame for the connection, which the caller answers: SETTINGS,
    /// which the connection has applied and the caller acknowledges, the
    /// acknowledgement of the caller's SETTINGS, PING, or GOAWAY.
    Connection(Frame),
}

/// Why a `Connection` refused a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// A stream error, by section 5.4.2. The connection has closed the
    /// stream, and the caller writes RST_STREAM with the error's code,
    /// without asking `Connection::send`.
    Stream { stream: u32, error: Error },
    /// A connection error, by section 5.4.1. The caller writes GOAWAY with
    /// the error's code, and the connection refuses every later frame.
    Connection(Error),
}

/// The streams of one connection, from the server's side, and both sides'
/// flow-control windows. It does no I/O. The caller gives it each frame the
/// `Reader` reads, and each HEADERS, DATA, and RST_STREAM it writes.
///
/// It discards what comes on a stream it reset, as section 5.1 says, for
/// the last `RESETS` of them. Past that, a frame on an older one is refused
/// as on any closed stream.
pub struct Connection {
    /// The streams that are open or half-closed.
    streams: BTreeMap<u32, Stream>,
    /// The streams it reset, newest last.
    resets: VecDeque<u32>,
    /// The highest stream id the client has opened.
    last: u32,
    max_streams: usize,
    /// SETTINGS_INITIAL_WINDOW_SIZE: the server's once acknowledged, and
    /// the client's.
    receive_initial: i64,
    send_initial: i64,
    /// The connection's windows: what the client may send, and what the
    /// server may.
    receive: i64,
    send: i64,
    /// What the connection's receive window has to get back.
    unannounced: i64,
    failed: Option<Error>,
}

struct Stream {
    receive: i64,
    send: i64,
    unannounced: i64,
    /// Whether the client has ended the stream, and whether the server has.
    remote_end: bool,
    local_end: bool,
}

/// What a stream id is to the connection.
enum Known {
    Live,
    Reset,
    Closed,
    Idle,
}

impl Default for Connection {
    fn default() -> Connection {
        Connection::new()
    }
}

impl Connection {
    pub fn new() -> Connection {
        Connection {
            streams: BTreeMap::new(),
            resets: VecDeque::new(),
            last: 0,
            max_streams: usize::MAX,
            receive_initial: WINDOW,
            send_initial: WINDOW,
            receive: WINDOW,
            send: WINDOW,
            unannounced: 0,
            failed: None,
        }
    }

    /// Applies the settings the caller sent, once the frame that
    /// acknowledges them has come: SETTINGS_MAX_CONCURRENT_STREAMS and
    /// SETTINGS_INITIAL_WINDOW_SIZE, by sections 5.1.2 and 6.9.2. It
    /// ignores the rest, and takes each value to be one the reader accepts.
    pub fn apply(&mut self, settings: &[(u16, u32)]) {
        for &(id, value) in settings {
            match id {
                MAX_CONCURRENT_STREAMS => self.max_streams = value as usize,
                INITIAL_WINDOW_SIZE => {
                    let change = i64::from(value) - self.receive_initial;
                    for stream in self.streams.values_mut() {
                        stream.receive += change;
                    }
                    self.receive_initial = i64::from(value);
                }
                _ => {}
            }
        }
    }

    /// What `frame`, from the client, means for its stream, or None if it
    /// means nothing the caller acts on.
    pub fn receive(&mut self, frame: Frame) -> Result<Option<Event>, Refused> {
        if let Some(error) = self.failed {
            return Err(Refused::Connection(error));
        }
        let read = self.read(frame);
        if let Err(Refused::Connection(error)) = read {
            self.failed = Some(error);
        }
        read
    }

    fn read(&mut self, frame: Frame) -> Result<Option<Event>, Refused> {
        let refuse = |error| Err(Refused::Connection(error));
        match frame {
            Frame::Headers {
                stream,
                headers,
                end_stream,
                ..
            } => self.headers(stream, headers, end_stream),
            Frame::Data {
                stream,
                data,
                padding,
                end_stream,
            } => self.data(stream, data, padding, end_stream),
            Frame::Priority { .. } => Ok(None),
            Frame::RstStream { stream, code } => match self.known(stream) {
                Known::Idle => refuse(Error::Idle),
                Known::Live => {
                    self.streams.remove(&stream);
                    Ok(Some(Event::Reset { stream, code }))
                }
                Known::Reset | Known::Closed => Ok(None),
            },
            Frame::WindowUpdate { stream, increment } => self.window_update(stream, increment),
            Frame::Settings(settings) => {
                for &(id, value) in &settings {
                    if id != INITIAL_WINDOW_SIZE {
                        continue;
                    }
                    let change = i64::from(value) - self.send_initial;
                    for stream in self.streams.values_mut() {
                        stream.send += change;
                        if stream.send > MAX_WINDOW {
                            return refuse(Error::FlowControl);
                        }
                    }
                    self.send_initial = i64::from(value);
                }
                Ok(Some(Event::Connection(Frame::Settings(settings))))
            }
            Frame::PushPromise { .. } => refuse(Error::Push),
            frame => Ok(Some(Event::Connection(frame))),
        }
    }

    fn known(&self, stream: u32) -> Known {
        if self.streams.contains_key(&stream) {
            Known::Live
        } else if self.resets.contains(&stream) {
            Known::Reset
        } else if !stream.is_multiple_of(2) && stream <= self.last {
            Known::Closed
        } else {
            Known::Idle
        }
    }

    fn headers(
        &mut self,
        stream: u32,
        headers: Vec<Header>,
        end_stream: bool,
    ) -> Result<Option<Event>, Refused> {
        match self.known(stream) {
            Known::Idle if stream.is_multiple_of(2) => Err(Refused::Connection(Error::StreamId)),
            Known::Idle => {
                self.last = stream;
                if self.streams.len() >= self.max_streams {
                    return self.refuse(stream, Error::RefusedStream);
                }
                let opened = Stream {
                    receive: self.receive_initial,
                    send: self.send_initial,
                    unannounced: 0,
                    remote_end: false,
                    local_end: false,
                };
                self.streams.insert(stream, opened);
                if end_stream {
                    self.end(stream, true);
                }
                Ok(Some(Event::Request {
                    stream,
                    headers,
                    end_stream,
                }))
            }
            Known::Live => {
                if self.streams[&stream].remote_end {
                    return self.refuse(stream, Error::StreamClosed);
                }
                if end_stream {
                    self.end(stream, true);
                }
                Ok(Some(Event::Trailers {
                    stream,
                    headers,
                    end_stream,
                }))
            }
            Known::Reset => Ok(None),
            Known::Closed => Err(Refused::Connection(Error::StreamId)),
        }
    }

    fn data(
        &mut self,
        stream: u32,
        data: Vec<u8>,
        padding: Option<u8>,
        end_stream: bool,
    ) -> Result<Option<Event>, Refused> {
        let length = flow_length(&data, padding) as i64;
        let padding = length - data.len() as i64;
        match self.known(stream) {
            Known::Idle => return Err(Refused::Connection(Error::Idle)),
            Known::Closed => return Err(Refused::Connection(Error::StreamClosed)),
            Known::Reset | Known::Live => {}
        }
        // Even DATA that is refused counts toward the connection's window,
        // by section 6.9.
        if length > self.receive {
            return Err(Refused::Connection(Error::FlowControl));
        }
        self.receive -= length;
        let Some(open) = self.streams.get_mut(&stream) else {
            self.unannounced += length;
            return Ok(None);
        };
        // An empty DATA frame needs no window, by section 6.9.1.
        if open.remote_end || length > open.receive.max(0) {
            self.unannounced += length;
            let error = match open.remote_end {
                true => Error::StreamClosed,
                false => Error::FlowControl,
            };
            return self.refuse(stream, error);
        }
        open.receive -= length;
        open.unannounced += padding;
        self.unannounced += padding;
        if end_stream {
            self.end(stream, true);
        }
        Ok(Some(Event::Data {
            stream,
            data,
            end_stream,
        }))
    }

    fn window_update(&mut self, stream: u32, increment: u32) -> Result<Option<Event>, Refused> {
        let increment = i64::from(increment);
        if stream == 0 {
            if increment == 0 {
                return Err(Refused::Connection(Error::WindowUpdate));
            }
            self.send += increment;
            if self.send > MAX_WINDOW {
                return Err(Refused::Connection(Error::FlowControl));
            }
            return Ok(None);
        }
        match self.known(stream) {
            Known::Idle => return Err(Refused::Connection(Error::Idle)),
            Known::Reset | Known::Closed => return Ok(None),
            Known::Live => {}
        }
        if increment == 0 {
            return self.refuse(stream, Error::WindowUpdate);
        }
        let open = self.streams.get_mut(&stream).expect("a live stream");
        open.send += increment;
        if open.send > MAX_WINDOW {
            return self.refuse(stream, Error::FlowControl);
        }
        Ok(None)
    }

    /// Closes `stream` for a stream error.
    fn refuse(&mut self, stream: u32, error: Error) -> Result<Option<Event>, Refused> {
        self.reset(stream);
        Err(Refused::Stream { stream, error })
    }

    fn reset(&mut self, stream: u32) {
        self.streams.remove(&stream);
        if self.resets.len() == RESETS {
            self.resets.pop_front();
        }
        self.resets.push_back(stream);
    }

    /// Ends one side of a live stream, and closes it once both have ended.
    fn end(&mut self, stream: u32, remote: bool) {
        let open = self.streams.get_mut(&stream).expect("a live stream");
        if remote {
            open.remote_end = true;
            // The client sends nothing more, so it needs no window back.
            open.unannounced = 0;
        } else {
            open.local_end = true;
        }
        if open.remote_end && open.local_end {
            self.streams.remove(&stream);
        }
    }

    /// Says that the caller has consumed `bytes` of the DATA on `stream`,
    /// so the client can have that much window back.
    pub fn consumed(&mut self, stream: u32, bytes: usize) {
        let bytes = bytes as i64;
        self.unannounced += bytes;
        if let Some(open) = self.streams.get_mut(&stream)
            && !open.remote_end
        {
            open.unannounced += bytes;
        }
    }

    /// The WINDOW_UPDATE frames the caller writes now. A window gets back
    /// what was consumed of it once that is half its initial size, so the
    /// client can keep sending and the updates aren't small.
    pub fn updates(&mut self) -> Vec<Frame> {
        let due = |initial: i64| (initial / 2).max(1);
        let mut updates = Vec::new();
        if self.unannounced >= due(WINDOW) {
            updates.push(Frame::WindowUpdate {
                stream: 0,
                increment: self.unannounced as u32,
            });
            self.receive += self.unannounced;
            self.unannounced = 0;
        }
        for (&stream, open) in &mut self.streams {
            if open.unannounced >= due(self.receive_initial) {
                updates.push(Frame::WindowUpdate {
                    stream,
                    increment: open.unannounced as u32,
                });
                open.receive += open.unannounced;
                open.unannounced = 0;
            }
        }
        updates
    }

    /// How many bytes of DATA the server may send on `stream` now, by
    /// section 6.9.1. It counts padding and the Pad Length byte.
    pub fn window(&self, stream: u32) -> usize {
        match self.streams.get(&stream) {
            Some(open) if !open.local_end => open.send.min(self.send).max(0) as usize,
            _ => 0,
        }
    }

    /// Takes a frame the server writes: HEADERS, DATA, or RST_STREAM. It
    /// says whether the caller may write it, which it may unless its stream
    /// is idle or closed, or the server has ended it, by section 5.1. The
    /// client can close a stream at any time. A RST_STREAM for a stream
    /// error is written without asking, because the connection has closed
    /// that stream. Every other frame may be written. DATA past `window` is
    /// a bug in the caller, so it panics.
    #[must_use]
    pub fn send(&mut self, frame: &Frame) -> bool {
        let (stream, end_stream) = match frame {
            Frame::Headers {
                stream, end_stream, ..
            } => (*stream, *end_stream),
            Frame::Data {
                stream, end_stream, ..
            } => (*stream, *end_stream),
            Frame::RstStream { stream, .. } => {
                let open = self.streams.contains_key(stream);
                if open {
                    self.reset(*stream);
                }
                return open;
            }
            _ => return true,
        };
        if !self
            .streams
            .get(&stream)
            .is_some_and(|open| !open.local_end)
        {
            return false;
        }
        if let Frame::Data { data, padding, .. } = frame {
            let length = flow_length(data, *padding);
            let window = self.window(stream);
            assert!(
                length <= window,
                "DATA of {length} bytes is past the window of {window}"
            );
            let open = self.streams.get_mut(&stream).expect("an open stream");
            open.send -= length as i64;
            self.send -= length as i64;
        }
        if end_stream {
            self.end(stream, false);
        }
        true
    }
}
