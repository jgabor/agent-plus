//! HTTP/2 frames, by RFC 9113. Each test of a refused frame names the
//! section that refuses it. The `h2` crate is the oracle: an `h2` client's
//! requests read the same here as in an `h2` server, and an `h2` client
//! reads the frames our writer writes.

use std::sync::{Arc, Mutex};

use net::h2::{Error, Frame, PREFACE, Priority, Reader, Writer};
use net::hpack::{self, Header};
use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

const LIST_LIMIT: usize = 1 << 20;

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
const MAX_FRAME_SIZE: u16 = 0x5;

// Error codes, by section 7.
const PROTOCOL_ERROR: u32 = 0x1;
const FLOW_CONTROL_ERROR: u32 = 0x3;
const FRAME_SIZE_ERROR: u32 = 0x6;
const COMPRESSION_ERROR: u32 = 0x9;
const ENHANCE_YOUR_CALM: u32 = 0xb;

/// A frame's bytes, by section 4.1.
fn frame(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let length = payload.len() as u32;
    let mut bytes = length.to_be_bytes()[1..].to_vec();
    bytes.extend([kind, flags]);
    bytes.extend(stream.to_be_bytes());
    bytes.extend(payload);
    bytes
}

/// A SETTINGS frame of one setting, by section 6.5.1.
fn setting(id: u16, value: u32) -> Vec<u8> {
    let mut payload = id.to_be_bytes().to_vec();
    payload.extend(value.to_be_bytes());
    frame(SETTINGS, 0, 0, &payload)
}

/// A client's preface and an empty SETTINGS frame, by section 3.4.
fn start() -> Vec<u8> {
    let mut bytes = PREFACE.to_vec();
    bytes.extend(frame(SETTINGS, 0, 0, &[]));
    bytes
}

/// `start()`, then `frames`.
fn started(frames: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = start();
    for frame in frames {
        bytes.extend(frame);
    }
    bytes
}

/// The frames a reader reads from `bytes`, after the first SETTINGS.
fn read_with(mut reader: Reader, bytes: &[u8]) -> Result<Vec<Frame>, Error> {
    reader.push(bytes);
    let mut frames = Vec::new();
    while let Some(frame) = reader.frame()? {
        frames.push(frame);
    }
    Ok(frames)
}

fn read(bytes: &[u8]) -> Result<Vec<Frame>, Error> {
    read_with(Reader::client(LIST_LIMIT), bytes)
}

/// The frames after the first SETTINGS that a client sends.
fn after_start(frames: &[Vec<u8>]) -> Result<Vec<Frame>, Error> {
    let mut read = read(&started(frames))?;
    assert_eq!(read.remove(0), Frame::Settings(vec![]));
    Ok(read)
}

fn refused(frames: &[Vec<u8>]) -> Error {
    after_start(frames).expect_err("refused")
}

fn header(name: &str, value: &str) -> Header {
    Header {
        name: name.into(),
        value: value.into(),
        never_indexed: false,
    }
}

fn request(path: &str) -> Vec<Header> {
    vec![
        header(":method", "GET"),
        header(":scheme", "https"),
        header(":authority", "example.test"),
        header(":path", path),
    ]
}

fn block(headers: &[Header]) -> Vec<u8> {
    let mut block = Vec::new();
    hpack::Encoder::new(4096).encode(headers, &mut block);
    block
}

// Section 3.4, the connection preface.

#[test]
fn a_client_preface_and_settings_start_a_connection_3_4() {
    assert_eq!(read(&start()), Ok(vec![Frame::Settings(vec![])]));
}

#[test]
fn a_wrong_preface_is_an_error_at_its_first_wrong_byte_3_4() {
    let mut reader = Reader::client(LIST_LIMIT);
    reader.push(&PREFACE[..10]);
    assert_eq!(reader.frame(), Ok(None));
    reader.push(b"X");
    assert_eq!(reader.frame(), Err(Error::Preface));
    assert_eq!(read(b"GET / HTTP/1.1\r\n\r\n"), Err(Error::Preface));
}

#[test]
fn the_first_frame_must_be_settings_3_4() {
    let mut bytes = PREFACE.to_vec();
    bytes.extend(frame(PING, 0, 0, &[0; 8]));
    assert_eq!(read(&bytes), Err(Error::Preface));
    let mut bytes = PREFACE.to_vec();
    bytes.extend(frame(SETTINGS, ACK, 0, &[]));
    assert_eq!(read(&bytes), Err(Error::Preface));
}

#[test]
fn a_server_sends_no_preface_string_but_starts_with_settings_3_4() {
    let bytes = frame(SETTINGS, 0, 0, &[0, 3, 0, 0, 0, 100]);
    let read = read_with(Reader::server(LIST_LIMIT), &bytes);
    assert_eq!(read, Ok(vec![Frame::Settings(vec![(3, 100)])]));
    let bytes = frame(PING, 0, 0, &[0; 8]);
    assert_eq!(
        read_with(Reader::server(LIST_LIMIT), &bytes),
        Err(Error::Preface)
    );
}

// Section 4, frames.

#[test]
fn a_frame_past_settings_max_frame_size_is_an_error_from_its_header_4_2() {
    let mut reader = Reader::client(LIST_LIMIT);
    reader.push(&start());
    assert_eq!(reader.frame(), Ok(Some(Frame::Settings(vec![]))));
    reader.push(&frame(DATA, 0, 1, &vec![0; 16384]));
    assert!(matches!(reader.frame(), Ok(Some(Frame::Data { .. }))));
    // The header alone says it is too long.
    reader.push(&frame(DATA, 0, 1, &vec![0; 16385])[..9]);
    assert_eq!(reader.frame(), Err(Error::FrameSize));
    assert_eq!(Error::FrameSize.code(), FRAME_SIZE_ERROR);
}

#[test]
fn an_acknowledged_max_frame_size_is_the_readers_limit_4_2() {
    let mut reader = Reader::client(LIST_LIMIT);
    reader.push(&start());
    assert_eq!(reader.frame(), Ok(Some(Frame::Settings(vec![]))));
    reader.apply(&[(MAX_FRAME_SIZE, 1 << 15)]);
    reader.push(&frame(DATA, 0, 1, &vec![0; 1 << 15]));
    assert!(matches!(reader.frame(), Ok(Some(Frame::Data { .. }))));
    reader.push(&frame(DATA, 0, 1, &vec![0; (1 << 15) + 1])[..9]);
    assert_eq!(reader.frame(), Err(Error::FrameSize));
}

#[test]
fn a_frame_of_an_unknown_type_is_skipped_4_1() {
    let frames = after_start(&[
        frame(0xfa, 0xff, 0, b"anything"),
        frame(0x0b, 0, 7, &[]),
        frame(PING, 0, 0, b"12345678"),
    ]);
    assert_eq!(
        frames,
        Ok(vec![Frame::Ping {
            ack: false,
            data: *b"12345678"
        }])
    );
}

#[test]
fn the_reserved_bit_of_a_stream_id_is_ignored_4_1() {
    let frames = after_start(&[frame(WINDOW_UPDATE, 0, 0x8000_0003, &[0x80, 0, 0, 9])]);
    assert_eq!(
        frames,
        Ok(vec![Frame::WindowUpdate {
            stream: 3,
            increment: 9
        }])
    );
}

#[test]
fn a_header_block_that_hpack_refuses_is_an_error_4_3() {
    let frames = [frame(HEADERS, END_HEADERS, 1, &[0x80])];
    let error = refused(&frames);
    assert_eq!(error, Error::Hpack(hpack::Error::Index));
    assert_eq!(error.code(), COMPRESSION_ERROR);
}

/// A header list past the list limit asks more of the reader than it gives,
/// by section 10.5, whether the joined block is too long or the headers it
/// decodes to are.
#[test]
fn a_header_block_past_the_list_limit_is_enhance_your_calm_10_5() {
    let mut reader = Reader::client(100);
    let mut bytes = start();
    bytes.extend(frame(HEADERS, 0, 1, &[0x00; 60]));
    bytes.extend(frame(CONTINUATION, 0, 1, &[0x00; 60]));
    reader.push(&bytes);
    assert_eq!(reader.frame(), Ok(Some(Frame::Settings(vec![]))));
    assert_eq!(reader.frame(), Err(Error::ListSize));
    assert_eq!(Error::ListSize.code(), ENHANCE_YOUR_CALM);
    // Three bytes of :method GET take 3 * 42, by section 4.1 of RFC 7541.
    let mut reader = Reader::client(100);
    reader.push(&started(&[frame(HEADERS, END_HEADERS, 1, &[0x82; 3])]));
    assert_eq!(reader.frame(), Ok(Some(Frame::Settings(vec![]))));
    assert_eq!(reader.frame(), Err(Error::ListSize));
}

#[test]
fn bytes_in_any_pieces_read_as_the_same_frames() {
    let bytes = started(&[
        frame(HEADERS, END_HEADERS | END_STREAM, 1, &block(&request("/a"))),
        frame(DATA, PADDED, 3, &[2, b'h', b'i', 0, 0]),
        frame(PING, ACK, 0, b"abcdefgh"),
    ]);
    let whole = read(&bytes).unwrap();
    assert_eq!(whole.len(), 4);
    let mut reader = Reader::client(LIST_LIMIT);
    let mut frames = Vec::new();
    for byte in &bytes {
        reader.push(std::slice::from_ref(byte));
        while let Some(frame) = reader.frame().unwrap() {
            frames.push(frame);
        }
    }
    assert_eq!(frames, whole);
}

#[test]
fn after_an_error_every_later_byte_is_refused() {
    let mut reader = Reader::client(LIST_LIMIT);
    reader.push(&started(&[frame(PING, 0, 1, &[0; 8])]));
    assert_eq!(reader.frame(), Ok(Some(Frame::Settings(vec![]))));
    assert_eq!(reader.frame(), Err(Error::StreamId));
    reader.push(&frame(PING, 0, 0, &[0; 8]));
    assert_eq!(reader.frame(), Err(Error::StreamId));
    assert_eq!(reader.frame(), Err(Error::StreamId));
}

/// Every error but a bad SETTINGS_INITIAL_WINDOW_SIZE, a frame's size, a
/// block HPACK refuses, and a list past its limit is a PROTOCOL_ERROR, by
/// sections 3.4, 5.1.1, 6, and 8.4.
#[test]
fn each_other_error_is_a_protocol_error_7() {
    for error in [
        Error::Preface,
        Error::Padding,
        Error::StreamId,
        Error::Settings,
        Error::WindowUpdate,
        Error::Continuation,
        Error::Push,
    ] {
        assert_eq!(error.code(), PROTOCOL_ERROR, "{error:?}");
    }
}

// Section 6.1, DATA.

#[test]
fn data_reads_its_flags_and_padding_6_1() {
    let frames = after_start(&[
        frame(DATA, END_STREAM | PADDED, 1, &[3, b'h', b'i', 0, 0, 0]),
        frame(DATA, 0, 3, b"yo"),
        frame(DATA, PADDED, 5, &[0]),
    ]);
    assert_eq!(
        frames,
        Ok(vec![
            Frame::Data {
                stream: 1,
                data: b"hi".to_vec(),
                padding: Some(3),
                end_stream: true,
            },
            Frame::Data {
                stream: 3,
                data: b"yo".to_vec(),
                padding: None,
                end_stream: false,
            },
            Frame::Data {
                stream: 5,
                data: vec![],
                padding: Some(0),
                end_stream: false,
            },
        ])
    );
}

#[test]
fn data_on_stream_zero_is_an_error_6_1() {
    assert_eq!(refused(&[frame(DATA, 0, 0, b"x")]), Error::StreamId);
}

#[test]
fn data_padding_as_long_as_the_payload_is_an_error_6_1() {
    assert_eq!(refused(&[frame(DATA, PADDED, 1, &[2, 0])]), Error::Padding);
    assert_eq!(refused(&[frame(DATA, PADDED, 1, &[1])]), Error::Padding);
    assert_eq!(refused(&[frame(DATA, PADDED, 1, &[])]), Error::Padding);
}

// Section 6.2, HEADERS.

#[test]
fn headers_read_their_priority_and_drop_their_padding_6_2() {
    let mut payload = vec![2, 0x80, 0, 0, 3, 15];
    payload.extend(block(&request("/p")));
    payload.extend([0, 0]);
    let flags = PADDED | PRIORITY_FLAG | END_HEADERS | END_STREAM;
    let frames = after_start(&[frame(HEADERS, flags, 1, &payload)]);
    assert_eq!(
        frames,
        Ok(vec![Frame::Headers {
            stream: 1,
            headers: request("/p"),
            priority: Some(Priority {
                exclusive: true,
                dependency: 3,
                weight: 15,
            }),
            end_stream: true,
        }])
    );
}

#[test]
fn headers_on_stream_zero_are_an_error_6_2() {
    let frames = [frame(HEADERS, END_HEADERS, 0, &block(&request("/")))];
    assert_eq!(refused(&frames), Error::StreamId);
}

#[test]
fn headers_padding_past_the_payload_is_an_error_6_2() {
    let mut payload = vec![200];
    payload.extend(block(&request("/")));
    let frames = [frame(HEADERS, PADDED | END_HEADERS, 1, &payload)];
    assert_eq!(refused(&frames), Error::Padding);
    // The priority fields count toward the payload too.
    let frames = [frame(
        HEADERS,
        PADDED | PRIORITY_FLAG,
        1,
        &[1, 0, 0, 0, 0, 0],
    )];
    assert_eq!(refused(&frames), Error::Padding);
}

#[test]
fn headers_too_short_for_their_priority_are_an_error_6_2() {
    let frames = [frame(HEADERS, PRIORITY_FLAG | END_HEADERS, 1, &[0, 0, 0])];
    assert_eq!(refused(&frames), Error::FrameSize);
}

// Section 6.3, PRIORITY.

#[test]
fn priority_reads_its_fields_6_3() {
    let frames = after_start(&[frame(PRIORITY, 0, 5, &[0, 0, 0, 1, 255])]);
    assert_eq!(
        frames,
        Ok(vec![Frame::Priority {
            stream: 5,
            priority: Priority {
                exclusive: false,
                dependency: 1,
                weight: 255,
            },
        }])
    );
}

#[test]
fn priority_of_a_length_other_than_5_is_an_error_6_3() {
    assert_eq!(refused(&[frame(PRIORITY, 0, 1, &[0; 4])]), Error::FrameSize);
    assert_eq!(refused(&[frame(PRIORITY, 0, 1, &[0; 6])]), Error::FrameSize);
}

#[test]
fn priority_on_stream_zero_is_an_error_6_3() {
    assert_eq!(refused(&[frame(PRIORITY, 0, 0, &[0; 5])]), Error::StreamId);
}

// Section 6.4, RST_STREAM.

#[test]
fn rst_stream_reads_its_code_6_4() {
    let frames = after_start(&[frame(RST_STREAM, 0, 1, &[0, 0, 0, 8])]);
    assert_eq!(frames, Ok(vec![Frame::RstStream { stream: 1, code: 8 }]));
}

#[test]
fn rst_stream_of_a_length_other_than_4_is_an_error_6_4() {
    let frames = [frame(RST_STREAM, 0, 1, &[0; 3])];
    assert_eq!(refused(&frames), Error::FrameSize);
}

#[test]
fn rst_stream_on_stream_zero_is_an_error_6_4() {
    let frames = [frame(RST_STREAM, 0, 0, &[0; 4])];
    assert_eq!(refused(&frames), Error::StreamId);
}

// Section 6.5, SETTINGS.

#[test]
fn settings_read_each_value_in_order_and_unknown_ones_too_6_5() {
    let payload = [0, 4, 0, 1, 0, 0, 0, 2, 0, 0, 0, 0, 0xab, 0xcd, 1, 2, 3, 4];
    let frames = after_start(&[
        frame(SETTINGS, 0, 0, &payload),
        frame(SETTINGS, ACK, 0, &[]),
    ]);
    assert_eq!(
        frames,
        Ok(vec![
            Frame::Settings(vec![(4, 65536), (2, 0), (0xabcd, 0x0102_0304)]),
            Frame::SettingsAck,
        ])
    );
}

#[test]
fn settings_of_a_length_not_a_multiple_of_6_are_an_error_6_5() {
    let frames = [frame(SETTINGS, 0, 0, &[0, 4, 0, 0, 0])];
    assert_eq!(refused(&frames), Error::FrameSize);
}

#[test]
fn a_settings_ack_with_a_payload_is_an_error_6_5() {
    let frames = [frame(SETTINGS, ACK, 0, &[0, 4, 0, 0, 0, 1])];
    assert_eq!(refused(&frames), Error::FrameSize);
}

#[test]
fn settings_on_a_stream_are_an_error_6_5() {
    assert_eq!(refused(&[frame(SETTINGS, 0, 1, &[])]), Error::StreamId);
}

#[test]
fn a_bad_settings_value_is_an_error_6_5_2() {
    for (id, value, error, code) in [
        // SETTINGS_ENABLE_PUSH
        (2u16, 2u32, Error::Settings, PROTOCOL_ERROR),
        // SETTINGS_INITIAL_WINDOW_SIZE
        (4, 0x8000_0000, Error::InitialWindowSize, FLOW_CONTROL_ERROR),
        // SETTINGS_MAX_FRAME_SIZE
        (5, 16383, Error::Settings, PROTOCOL_ERROR),
        (5, 1 << 24, Error::Settings, PROTOCOL_ERROR),
    ] {
        assert_eq!(refused(&[setting(id, value)]), error, "{id} {value}");
        assert_eq!(error.code(), code, "{id} {value}");
    }
    let mut payload = 4u16.to_be_bytes().to_vec();
    payload.extend(0x7fff_ffffu32.to_be_bytes());
    payload.extend([0, 5, 0, 0xff, 0xff, 0xff]);
    assert!(after_start(&[frame(SETTINGS, 0, 0, &payload)]).is_ok());
}

#[test]
fn a_server_that_enables_push_is_an_error_6_5_2() {
    let bytes = frame(SETTINGS, 0, 0, &[0, 2, 0, 0, 0, 1]);
    let read = read_with(Reader::server(LIST_LIMIT), &bytes);
    assert_eq!(read, Err(Error::Settings));
    let bytes = frame(SETTINGS, 0, 0, &[0, 2, 0, 0, 0, 0]);
    assert!(read_with(Reader::server(LIST_LIMIT), &bytes).is_ok());
}

// Section 6.6, PUSH_PROMISE.

/// A server's empty SETTINGS frame, then `frames`, read by a client.
fn from_server(frames: &[Vec<u8>]) -> Result<Vec<Frame>, Error> {
    let mut bytes = frame(SETTINGS, 0, 0, &[]);
    for frame in frames {
        bytes.extend(frame);
    }
    let mut read = read_with(Reader::server(LIST_LIMIT), &bytes)?;
    assert_eq!(read.remove(0), Frame::Settings(vec![]));
    Ok(read)
}

#[test]
fn push_promise_reads_its_promised_stream_and_block_6_6() {
    let mut payload = vec![1, 0x80, 0, 0, 2];
    payload.extend(block(&request("/pushed")));
    payload.push(0);
    let headers_block = block(&request("/pushed"));
    let (first, rest) = headers_block.split_at(3);
    let mut split = vec![0, 0, 0, 4];
    split.extend(first);
    let frames = from_server(&[
        frame(PUSH_PROMISE, PADDED | END_HEADERS, 1, &payload),
        frame(PUSH_PROMISE, 0, 1, &split),
        frame(CONTINUATION, END_HEADERS, 1, rest),
    ]);
    // The second block indexes what the first one added.
    let frames = frames.unwrap();
    assert_eq!(frames.len(), 2);
    assert_eq!(
        frames[0],
        Frame::PushPromise {
            stream: 1,
            promised: 2,
            headers: request("/pushed"),
        }
    );
    assert!(matches!(
        frames[1],
        Frame::PushPromise {
            stream: 1,
            promised: 4,
            ..
        }
    ));
}

#[test]
fn push_promise_on_stream_zero_or_promising_zero_is_an_error_6_6() {
    let frames = [frame(PUSH_PROMISE, END_HEADERS, 0, &[0, 0, 0, 2])];
    assert_eq!(from_server(&frames), Err(Error::StreamId));
    let frames = [frame(PUSH_PROMISE, END_HEADERS, 1, &[0, 0, 0, 0])];
    assert_eq!(from_server(&frames), Err(Error::StreamId));
}

#[test]
fn push_promise_padding_past_the_payload_is_an_error_6_6() {
    let frames = [frame(
        PUSH_PROMISE,
        PADDED | END_HEADERS,
        1,
        &[1, 0, 0, 0, 2],
    )];
    assert_eq!(from_server(&frames), Err(Error::Padding));
}

#[test]
fn push_promise_too_short_for_its_promised_stream_is_an_error_6_6() {
    let frames = [frame(PUSH_PROMISE, END_HEADERS, 1, &[0, 0, 2])];
    assert_eq!(from_server(&frames), Err(Error::FrameSize));
}

#[test]
fn once_disabled_push_is_acknowledged_push_promise_is_an_error_6_6() {
    let push = |promised: u8| frame(PUSH_PROMISE, END_HEADERS, 1, &[0, 0, 0, promised]);
    let mut reader = Reader::server(LIST_LIMIT);
    reader.push(&frame(SETTINGS, 0, 0, &[]));
    reader.push(&push(2));
    assert_eq!(reader.frame(), Ok(Some(Frame::Settings(vec![]))));
    // A server may push until it acknowledges SETTINGS_ENABLE_PUSH of 0.
    assert!(matches!(
        reader.frame(),
        Ok(Some(Frame::PushPromise { .. }))
    ));
    reader.push(&frame(SETTINGS, ACK, 0, &[]));
    reader.push(&push(4));
    assert_eq!(reader.frame(), Ok(Some(Frame::SettingsAck)));
    reader.apply(&[(ENABLE_PUSH, 0)]);
    assert_eq!(reader.frame(), Err(Error::Push));
    assert_eq!(Error::Push.code(), PROTOCOL_ERROR);
}

#[test]
fn a_client_that_pushes_is_an_error_8_4() {
    let frames = [frame(PUSH_PROMISE, END_HEADERS, 1, &[0, 0, 0, 2])];
    assert_eq!(refused(&frames), Error::Push);
}

// Section 6.7, PING.

#[test]
fn ping_of_a_length_other_than_8_is_an_error_6_7() {
    assert_eq!(refused(&[frame(PING, 0, 0, &[0; 7])]), Error::FrameSize);
    assert_eq!(refused(&[frame(PING, ACK, 0, &[])]), Error::FrameSize);
}

#[test]
fn ping_on_a_stream_is_an_error_6_7() {
    assert_eq!(refused(&[frame(PING, 0, 1, &[0; 8])]), Error::StreamId);
}

// Section 6.8, GOAWAY.

#[test]
fn goaway_reads_its_last_stream_code_and_debug_data_6_8() {
    let frames = after_start(&[frame(
        GOAWAY,
        0,
        0,
        &[0x80, 0, 0, 5, 0, 0, 0, 1, b'b', b'y'],
    )]);
    assert_eq!(
        frames,
        Ok(vec![Frame::GoAway {
            last_stream: 5,
            code: 1,
            debug: b"by".to_vec(),
        }])
    );
}

#[test]
fn goaway_shorter_than_8_is_an_error_6_8() {
    assert_eq!(refused(&[frame(GOAWAY, 0, 0, &[0; 7])]), Error::FrameSize);
}

#[test]
fn goaway_on_a_stream_is_an_error_6_8() {
    assert_eq!(refused(&[frame(GOAWAY, 0, 1, &[0; 8])]), Error::StreamId);
}

// Section 6.9, WINDOW_UPDATE.

#[test]
fn window_update_of_a_length_other_than_4_is_an_error_6_9() {
    let frames = [frame(WINDOW_UPDATE, 0, 0, &[0, 0, 0, 0, 1])];
    assert_eq!(refused(&frames), Error::FrameSize);
}

#[test]
fn a_zero_increment_on_the_connection_is_an_error_6_9() {
    let frames = [frame(WINDOW_UPDATE, 0, 0, &[0x80, 0, 0, 0])];
    assert_eq!(refused(&frames), Error::WindowUpdate);
    // On a stream, it is a stream error, which the stream decides.
    let frames = after_start(&[frame(WINDOW_UPDATE, 0, 1, &[0, 0, 0, 0])]);
    assert_eq!(
        frames,
        Ok(vec![Frame::WindowUpdate {
            stream: 1,
            increment: 0
        }])
    );
}

// Section 6.10, CONTINUATION.

#[test]
fn continuation_joins_the_block_of_its_headers_6_10() {
    let headers = request("/joined");
    let block = block(&headers);
    let frames = after_start(&[
        frame(HEADERS, END_STREAM, 3, &block[..2]),
        frame(CONTINUATION, 0, 3, &block[2..5]),
        frame(CONTINUATION, 0, 3, &[]),
        frame(CONTINUATION, END_HEADERS, 3, &block[5..]),
    ]);
    assert_eq!(
        frames,
        Ok(vec![Frame::Headers {
            stream: 3,
            headers,
            priority: None,
            end_stream: true,
        }])
    );
}

#[test]
fn any_frame_inside_a_header_block_is_an_error_6_10() {
    let block = block(&request("/"));
    for between in [
        frame(PING, 0, 0, &[0; 8]),
        frame(0xfa, 0, 3, &[]),
        frame(DATA, 0, 3, b"x"),
        frame(HEADERS, END_HEADERS, 3, &block),
        frame(CONTINUATION, END_HEADERS, 5, &block),
    ] {
        let frames = [frame(HEADERS, 0, 3, &block), between];
        assert_eq!(refused(&frames), Error::Continuation);
    }
}

#[test]
fn continuation_without_headers_is_an_error_6_10() {
    let frames = [frame(CONTINUATION, END_HEADERS, 1, &block(&request("/")))];
    assert_eq!(refused(&frames), Error::Continuation);
    let frames = [
        frame(HEADERS, END_HEADERS, 1, &block(&request("/"))),
        frame(CONTINUATION, END_HEADERS, 1, &[]),
    ];
    assert_eq!(refused(&frames), Error::Continuation);
}

#[test]
fn continuation_on_stream_zero_is_an_error_6_10() {
    let frames = [frame(CONTINUATION, END_HEADERS, 0, &[])];
    assert_eq!(refused(&frames), Error::StreamId);
}

// Writing.

/// One frame of every type, with the flags and fields each can have.
fn every_frame() -> Vec<Frame> {
    let priority = Priority {
        exclusive: true,
        dependency: 0x7fff_ffff,
        weight: 0,
    };
    vec![
        Frame::Settings(vec![(1, 0), (4, 0x7fff_ffff), (0xffff, 7)]),
        Frame::SettingsAck,
        Frame::Headers {
            stream: 1,
            headers: request("/w"),
            priority: Some(priority),
            end_stream: false,
        },
        Frame::Headers {
            stream: 3,
            headers: request("/w"),
            priority: None,
            end_stream: true,
        },
        Frame::Data {
            stream: 1,
            data: b"body".to_vec(),
            padding: Some(255),
            end_stream: true,
        },
        Frame::Data {
            stream: 5,
            data: vec![],
            padding: None,
            end_stream: false,
        },
        Frame::Priority {
            stream: 7,
            priority,
        },
        Frame::RstStream {
            stream: 3,
            code: 0xffff_ffff,
        },
        Frame::Ping {
            ack: true,
            data: *b"pingpong",
        },
        Frame::GoAway {
            last_stream: 3,
            code: 2,
            debug: b"bye".to_vec(),
        },
        Frame::WindowUpdate {
            stream: 0,
            increment: 0x7fff_ffff,
        },
    ]
}

#[test]
fn every_frame_written_reads_back_the_same() {
    let mut writer = Writer::new();
    let mut bytes = PREFACE.to_vec();
    for frame in every_frame() {
        writer.write(&frame, &mut bytes);
    }
    assert_eq!(read(&bytes), Ok(every_frame()));
}

#[test]
fn push_promise_written_reads_back_the_same() {
    let mut writer = Writer::new();
    let frames = vec![
        Frame::Settings(vec![]),
        Frame::PushPromise {
            stream: 1,
            promised: 2,
            headers: request("/p"),
        },
    ];
    let mut bytes = Vec::new();
    for frame in &frames {
        writer.write(frame, &mut bytes);
    }
    assert_eq!(read_with(Reader::server(LIST_LIMIT), &bytes), Ok(frames));
}

#[test]
fn a_large_header_block_is_written_across_continuation_frames() {
    let mut headers = request("/big");
    headers.push(header("x-big", &"a".repeat(60_000)));
    let frame = Frame::Headers {
        stream: 1,
        headers,
        priority: None,
        end_stream: true,
    };
    let mut bytes = start();
    Writer::new().write(&frame, &mut bytes);
    let kinds = kinds(&bytes[start().len()..], 16384);
    assert_eq!(kinds, [HEADERS, CONTINUATION, CONTINUATION]);
    assert_eq!(read(&bytes), Ok(vec![Frame::Settings(vec![]), frame]));
}

#[test]
fn the_writer_writes_up_to_an_acknowledged_max_frame_size_4_2() {
    let mut headers = request("/big");
    headers.push(header("x-big", &"a".repeat(30_000)));
    let frames = [
        Frame::Headers {
            stream: 1,
            headers,
            priority: None,
            end_stream: false,
        },
        Frame::Data {
            stream: 1,
            data: vec![7; 1 << 15],
            padding: None,
            end_stream: true,
        },
    ];
    let mut writer = Writer::new();
    writer.apply(&[(MAX_FRAME_SIZE, 1 << 15)]);
    let mut bytes = start();
    for frame in &frames {
        writer.write(frame, &mut bytes);
    }
    let kinds = kinds(&bytes[start().len()..], 1 << 15);
    assert_eq!(kinds, [HEADERS, DATA]);
    let mut reader = Reader::client(LIST_LIMIT);
    reader.push(&bytes);
    assert_eq!(reader.frame(), Ok(Some(Frame::Settings(vec![]))));
    reader.apply(&[(MAX_FRAME_SIZE, 1 << 15)]);
    assert_eq!(reader.frame(), Ok(Some(frames[0].clone())));
    assert_eq!(reader.frame(), Ok(Some(frames[1].clone())));
}

/// A peer's SETTINGS_HEADER_TABLE_SIZE of 0, once acknowledged, empties the
/// table, and each block after says so first, by section 4.2 of RFC 7541.
#[test]
fn an_acknowledged_table_size_of_0_gets_blocks_from_a_table_of_0_hpack_4_2() {
    let headers = |path| Frame::Headers {
        stream: 1,
        headers: request(path),
        priority: None,
        end_stream: true,
    };
    let frames = [headers("/a"), headers("/a"), headers("/b")];
    let written = |writer: &mut Writer| {
        let mut bytes = start();
        for frame in &frames {
            writer.write(frame, &mut bytes);
        }
        bytes
    };
    let reader = || {
        let mut reader = Reader::client(LIST_LIMIT);
        reader.push(&start());
        assert_eq!(reader.frame(), Ok(Some(Frame::Settings(vec![]))));
        reader.apply(&[(HEADER_TABLE_SIZE, 0)]);
        reader
    };
    let mut writer = Writer::new();
    writer.apply(&[(HEADER_TABLE_SIZE, 0)]);
    let bytes = written(&mut writer);
    // The first block starts with a dynamic table size update to 0.
    assert_eq!(bytes[start().len() + 9], 0x20);
    let read = read_with(reader(), &bytes[start().len()..]);
    assert_eq!(read, Ok(frames.to_vec()));
    // A reader that has a table of 0 refuses a block that doesn't say so.
    let bytes = written(&mut Writer::new());
    let read = read_with(reader(), &bytes[start().len()..]);
    assert_eq!(read, Err(Error::Hpack(hpack::Error::MissingUpdate)));
}

/// The type and payload of each whole frame in `bytes`.
fn split(mut bytes: &[u8]) -> Vec<(u8, &[u8])> {
    let mut frames = Vec::new();
    while let Some(head) = bytes.first_chunk::<9>() {
        let length = u32::from_be_bytes([0, head[0], head[1], head[2]]) as usize;
        frames.push((head[3], &bytes[9..9 + length]));
        bytes = &bytes[9 + length..];
    }
    frames
}

/// The type of each frame in `bytes`, each of which is within `limit`.
fn kinds(bytes: &[u8], limit: usize) -> Vec<u8> {
    (split(bytes).into_iter())
        .map(|(kind, payload)| {
            assert!(payload.len() <= limit, "{}", payload.len());
            kind
        })
        .collect()
}

// The h2 crate.

/// Rewrites each HEADERS frame a client sends as a padded HEADERS frame
/// with a priority, and half its block, then a CONTINUATION frame with the
/// rest. A PRIORITY frame and a frame of an unknown type go before it.
/// Other frames pass as they are.
struct Rewrite {
    held: Vec<u8>,
    preface: bool,
}

impl Rewrite {
    fn new() -> Rewrite {
        Rewrite {
            held: Vec::new(),
            preface: false,
        }
    }

    fn rewrite(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.held.extend(bytes);
        let mut out = Vec::new();
        if !self.preface {
            if self.held.len() < PREFACE.len() {
                return out;
            }
            out.extend(self.held.drain(..PREFACE.len()));
            self.preface = true;
        }
        while self.held.len() >= 9 {
            let length = u32::from_be_bytes([0, self.held[0], self.held[1], self.held[2]]);
            let end = 9 + length as usize;
            if self.held.len() < end {
                break;
            }
            let held: Vec<u8> = self.held.drain(..end).collect();
            let (kind, flags) = (held[3], held[4]);
            let stream = u32::from_be_bytes([held[5], held[6], held[7], held[8]]);
            if kind != HEADERS {
                out.extend(held);
                continue;
            }
            assert_eq!(flags & (PADDED | PRIORITY_FLAG), 0, "h2 sent no padding");
            let fragment = &held[9..];
            let (first, rest) = fragment.split_at(fragment.len() / 2);
            out.extend(frame(PRIORITY, 0, stream, &[0, 0, 0, 0, 7]));
            out.extend(frame(0xfa, 0, stream, b"skip me"));
            let mut payload = vec![4, 0, 0, 0, 0, 200];
            payload.extend(first);
            payload.extend([0; 4]);
            let flags = flags & !END_HEADERS | PADDED | PRIORITY_FLAG;
            out.extend(frame(HEADERS, flags, stream, &payload));
            let last = held[4] & END_HEADERS;
            out.extend(frame(CONTINUATION, last, stream, rest));
        }
        out
    }
}

/// Each stream's `:authority` and `:path`.
type Seen = Vec<(u32, Option<String>, Option<String>)>;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime")
}

/// Sends `paths` from an `h2` client to an `h2` server, through
/// `Rewrite`. Returns what the server saw, and the bytes it read.
fn through_h2(paths: &[String]) -> (Seen, Vec<u8>) {
    runtime().block_on(async {
        let (client_io, middle_client) = duplex(64 * 1024);
        let (middle_server, server_io) = duplex(64 * 1024);
        let sent = Arc::new(Mutex::new(Vec::new()));

        let server = tokio::spawn(async move {
            let mut connection = h2::server::handshake(server_io)
                .await
                .expect("server handshake");
            let mut seen = Vec::new();
            while let Some(stream) = connection.accept().await {
                let (request, mut respond) = stream.expect("accept stream");
                let authority = request.uri().authority().map(|a| a.as_str().to_owned());
                let path = (request.uri().path_and_query()).map(|p| p.as_str().to_owned());
                seen.push((respond.stream_id().as_u32(), authority, path));
                respond
                    .send_response(http::Response::new(()), true)
                    .expect("send response");
            }
            seen
        });

        let (mut from_client, mut to_client) = tokio::io::split(middle_client);
        let (mut from_server, mut to_server) = tokio::io::split(middle_server);
        let recorded = Arc::clone(&sent);
        let upward = tokio::spawn(async move {
            let mut rewrite = Rewrite::new();
            let mut buffer = vec![0; 16 * 1024];
            loop {
                let count = from_client.read(&mut buffer).await.expect("read client");
                if count == 0 {
                    break;
                }
                let out = rewrite.rewrite(&buffer[..count]);
                recorded.lock().unwrap().extend(&out);
                to_server.write_all(&out).await.expect("write server");
            }
            to_server.shutdown().await.expect("shutdown");
        });
        let downward = tokio::spawn(async move {
            tokio::io::copy(&mut from_server, &mut to_client).await.ok();
            to_client.shutdown().await.ok();
        });

        let (client, connection) = h2::client::handshake(client_io)
            .await
            .expect("client handshake");
        let driver = tokio::spawn(async move {
            connection.await.expect("client connection");
        });
        let mut client = client.ready().await.expect("client ready");
        let mut responses = Vec::new();
        for path in paths {
            let request = http::Request::get(format!("https://example.test{path}"))
                .body(())
                .expect("request");
            let (response, _) = client.send_request(request, true).expect("send request");
            responses.push(response);
            client = client.ready().await.expect("client ready");
        }
        for response in responses {
            let response = response.await.expect("response");
            assert_eq!(response.status(), http::StatusCode::OK);
        }
        drop(client);
        driver.await.expect("client driver");
        let seen = server.await.expect("server");
        upward.await.expect("upward");
        downward.await.expect("downward");
        let sent = sent.lock().unwrap().clone();
        (seen, sent)
    })
}

/// Each HEADERS frame's stream, `:authority`, and `:path`, as our reader
/// reads them.
fn ours(frames: &[Frame]) -> Seen {
    let text = |headers: &[Header], name: &[u8]| {
        (headers.iter())
            .find(|header| header.name == name)
            .map(|header| String::from_utf8(header.value.clone()).unwrap())
    };
    (frames.iter())
        .filter_map(|frame| match frame {
            Frame::Headers {
                stream, headers, ..
            } => Some((
                *stream,
                text(headers, b":authority"),
                text(headers, b":path"),
            )),
            _ => None,
        })
        .collect()
}

#[test]
fn an_h2_clients_requests_read_as_the_h2_server_reads_them() {
    let long = format!("/long?{}", "q".repeat(20_000));
    let paths = ["/one".to_owned(), "/two?x=1".to_owned(), long.clone()];
    let (seen, sent) = through_h2(&paths);
    assert_eq!(seen.len(), 3);
    // The long path takes more than one frame before the rewrite.
    let continuations = (kinds(&sent[PREFACE.len()..], 16384).iter())
        .filter(|&&kind| kind == CONTINUATION)
        .count();
    assert!(continuations >= 4, "{continuations}");
    let frames = read(&sent).expect("our reader");
    assert_eq!(ours(&frames), seen);
    assert_eq!(seen[2].2.as_deref(), Some(&long[..]));
}

#[test]
fn an_h2_client_reads_the_frames_our_writer_writes() {
    let (status, big, body, refused, read) = runtime().block_on(async {
        let (client_io, mut server_io) = duplex(64 * 1024);
        let server = tokio::spawn(async move {
            let mut reader = Reader::client(LIST_LIMIT);
            let mut writer = Writer::new();
            let mut out = Vec::new();
            writer.write(&Frame::Settings(vec![(3, 100)]), &mut out);
            writer.write(
                &Frame::Ping {
                    ack: false,
                    data: *b"ourping!",
                },
                &mut out,
            );
            writer.write(
                &Frame::WindowUpdate {
                    stream: 0,
                    increment: 1000,
                },
                &mut out,
            );
            server_io.write_all(&out).await.expect("write");
            let mut read = Vec::new();
            let mut buffer = vec![0; 16 * 1024];
            loop {
                let count = server_io.read(&mut buffer).await.expect("read");
                if count == 0 {
                    break;
                }
                reader.push(&buffer[..count]);
                let mut out = Vec::new();
                while let Some(frame) = reader.frame().expect("our reader") {
                    match &frame {
                        Frame::Settings(settings) => {
                            writer.write(&Frame::SettingsAck, &mut out);
                            writer.apply(settings);
                        }
                        Frame::Headers { stream: 1, .. } => {
                            let response = Frame::Headers {
                                stream: 1,
                                headers: vec![
                                    header(":status", "200"),
                                    header("x-big", &"b".repeat(30_000)),
                                ],
                                priority: None,
                                end_stream: false,
                            };
                            writer.write(&response, &mut out);
                            // One frame, past the default frame size.
                            let data = Frame::Data {
                                stream: 1,
                                data: vec![b'h'; 20_000],
                                padding: Some(9),
                                end_stream: true,
                            };
                            writer.write(&data, &mut out);
                        }
                        Frame::Headers { stream, .. } => {
                            let reset = Frame::RstStream {
                                stream: *stream,
                                code: 7,
                            };
                            writer.write(&reset, &mut out);
                        }
                        _ => {}
                    }
                    read.push(frame);
                }
                server_io.write_all(&out).await.expect("write");
            }
            read
        });

        // Our writer applies these once it acknowledges them, by section
        // 6.5.3. `h2` refuses a frame past its SETTINGS_MAX_FRAME_SIZE, and
        // a table size update past its SETTINGS_HEADER_TABLE_SIZE, but not
        // a block that leaves out the update.
        let (client, connection) = (h2::client::Builder::new())
            .header_table_size(0)
            .max_frame_size(1 << 15)
            .handshake::<_, &'static [u8]>(client_io)
            .await
            .expect("client handshake");
        let driver = tokio::spawn(async move {
            connection.await.expect("client connection");
        });
        let mut client = client.ready().await.expect("client ready");
        let get = |path: &str| {
            http::Request::get(format!("https://example.test{path}"))
                .body(())
                .expect("request")
        };
        let (first, _) = client.send_request(get("/one"), true).expect("send");
        let mut client = client.ready().await.expect("client ready");
        let (second, _) = client.send_request(get("/two"), true).expect("send");
        let response = first.await.expect("response");
        let status = response.status();
        let big = response.headers()["x-big"].len();
        let mut body = response.into_body();
        let mut bytes = Vec::new();
        while let Some(chunk) = body.data().await {
            let chunk = chunk.expect("chunk");
            body.flow_control()
                .release_capacity(chunk.len())
                .expect("release");
            bytes.extend(chunk);
        }
        let refused = second.await.expect_err("refused").reason();
        drop((body, client));
        driver.await.expect("client driver");
        (status, big, bytes, refused, server.await.expect("server"))
    });
    assert_eq!(status, http::StatusCode::OK);
    assert_eq!(big, 30_000);
    assert_eq!(body, vec![b'h'; 20_000]);
    assert_eq!(refused, Some(h2::Reason::REFUSED_STREAM));
    // The client acknowledged our SETTINGS and answered our PING.
    assert!(read.contains(&Frame::SettingsAck), "{read:?}");
    assert!(
        read.contains(&Frame::Ping {
            ack: true,
            data: *b"ourping!"
        }),
        "{read:?}"
    );
}

const H2_LIMIT: usize = 1000;

/// The code of the GOAWAY an `h2` server sends after a client's preface
/// and empty SETTINGS, then `frames`, with a list limit of `H2_LIMIT`.
fn h2_goaway(frames: &[Vec<u8>]) -> Option<u32> {
    runtime().block_on(async {
        let (mut client_io, server_io) = duplex(64 * 1024);
        let server = tokio::spawn(async move {
            let mut builder = h2::server::Builder::new();
            builder.max_header_list_size(H2_LIMIT as u32);
            let mut connection = builder
                .handshake::<_, &'static [u8]>(server_io)
                .await
                .ok()?;
            while let Some(Ok(_)) = connection.accept().await {}
            Some(())
        });
        client_io.write_all(&started(frames)).await.expect("write");
        client_io.shutdown().await.expect("shutdown");
        let mut sent = Vec::new();
        client_io.read_to_end(&mut sent).await.expect("read");
        server.await.expect("server");
        (split(&sent).into_iter())
            .find(|&(kind, _)| kind == GOAWAY)
            .map(|(_, payload)| u32::from_be_bytes(payload[4..8].try_into().unwrap()))
    })
}

/// `h2` departs from RFC 9113 in most of the cases this leaves out. It
/// sends a PROTOCOL_ERROR for every frame its parser refuses, where sections
/// 6.4, 6.5, 6.7, 6.8, and 6.9 say FRAME_SIZE_ERROR, and section 6.5.2 says
/// FLOW_CONTROL_ERROR. It sends a PROTOCOL_ERROR for a block that HPACK
/// refuses, where section 4.3 says COMPRESSION_ERROR. It sends a
/// COMPRESSION_ERROR for a joined block past its list limit, which HPACK
/// never decoded. The rest can't be compared: `h2` sends no GOAWAY for a
/// wrong preface, keeps the connection for headers past its list limit but
/// within 4 times it, and as a server is never pushed to.
#[test]
fn each_error_has_the_code_an_h2_server_sends() {
    // Thirty user-agent headers of 100 bytes, each a literal of name index
    // 58, take 30 * 142, past the 4 times the limit that `h2` allows.
    let mut user_agents = Vec::new();
    for _ in 0..30 {
        user_agents.extend([0x0f, 58 - 15, 100]);
        user_agents.extend([b'a'; 100]);
    }
    let cases = [
        vec![frame(DATA, 0, 1, &vec![0; 16385])],
        vec![frame(DATA, 0, 0, b"x")],
        vec![frame(HEADERS, PADDED | END_HEADERS, 1, &[9, 0x82])],
        vec![frame(HEADERS, END_HEADERS, 1, &user_agents)],
        vec![frame(HEADERS, 0, 1, &[0x82]), frame(PING, 0, 0, &[0; 8])],
        vec![frame(CONTINUATION, END_HEADERS, 1, &[0x82])],
        vec![frame(RST_STREAM, 0, 0, &[0; 4])],
        vec![setting(2, 2)],
        vec![setting(5, 16383)],
        vec![setting(5, 1 << 24)],
        vec![frame(SETTINGS, 0, 1, &[])],
        vec![frame(PUSH_PROMISE, END_HEADERS, 1, &[0, 0, 0, 2])],
        vec![frame(PING, 0, 1, &[0; 8])],
        vec![frame(GOAWAY, 0, 1, &[0; 8])],
        vec![frame(WINDOW_UPDATE, 0, 0, &[0; 4])],
    ];
    for frames in cases {
        let reader = Reader::client(H2_LIMIT);
        let error = read_with(reader, &started(&frames)).expect_err("refused");
        assert_eq!(
            Some(error.code()),
            h2_goaway(&frames),
            "{error:?} {frames:x?}"
        );
    }
}

#[test]
#[should_panic(expected = "SETTINGS_MAX_FRAME_SIZE")]
fn a_frame_too_long_to_write_is_refused_not_cut() {
    let frame = Frame::Data {
        stream: 1,
        data: vec![0; 1 << 24],
        padding: None,
        end_stream: false,
    };
    Writer::new().write(&frame, &mut Vec::new());
}
