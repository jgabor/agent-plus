//! The HTTP/2 request check, on a connection to a host with URL rules whose
//! guest and server agreed HTTP/2: what the proxy reads of each request
//! there, and what it passes on, by **Decryption** under NETWORK in
//! SPEC.md. The client's frames are written by `net::h2`, and what passes
//! is read back by it. The `h2` crate is the oracle on both ends in
//! `crates/ap/tests/decrypt.rs`.

use net::h2::{Frame, PREFACE, Priority, Reader, Writer};
use net::hpack::Header;
use net::{NameCheck, Refused};

const NAME: &str = "host.ap.internal";
const ENABLE_PUSH: u16 = 0x2;
const INITIAL_WINDOW_SIZE: u16 = 0x4;
const MAX_FRAME_SIZE: u16 = 0x5;
/// SETTINGS_MAX_FRAME_SIZE before SETTINGS change it, which every server
/// takes.
const FRAME: usize = 1 << 14;

fn header(name: &str, value: &str) -> Header {
    Header {
        name: name.into(),
        value: value.into(),
        never_indexed: false,
    }
}

/// A request's pseudo-header fields, for `path` on `NAME`.
fn request(path: &str) -> Vec<Header> {
    vec![
        header(":method", "POST"),
        header(":scheme", "https"),
        header(":authority", NAME),
        header(":path", path),
    ]
}

fn headers(stream: u32, headers: Vec<Header>, end_stream: bool) -> Frame {
    Frame::Headers {
        stream,
        headers,
        priority: None,
        end_stream,
    }
}

fn data(stream: u32, data: &[u8], end_stream: bool) -> Frame {
    Frame::Data {
        stream,
        data: data.to_vec(),
        padding: None,
        end_stream,
    }
}

/// What a client sends: its preface, its SETTINGS, which let it send
/// frames past the first SETTINGS_MAX_FRAME_SIZE, and `frames`.
fn client(frames: &[Frame]) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.apply(&[(MAX_FRAME_SIZE, 1 << 20)]);
    let mut out = PREFACE.to_vec();
    writer.write(
        &Frame::Settings(vec![(INITIAL_WINDOW_SIZE, 1 << 20)]),
        &mut out,
    );
    for frame in frames {
        writer.write(frame, &mut out);
    }
    out
}

/// What passed to the server.
struct Passed {
    /// The frames after the client's preface, read as a server that
    /// changed none of HTTP/2's first settings reads them.
    frames: Vec<Frame>,
    /// Whether the check refused, and when.
    fed: Result<(), Refused>,
    /// The paths the check asked the rules about.
    asked: Vec<String>,
    /// Whether the check held part of a frame at the end.
    holding: bool,
}

/// What passes of `input`, fed in pieces of `piece` bytes, to an HTTP/2
/// check that allows a path under `/allowed`.
fn fed(input: &[u8], piece: usize) -> Passed {
    let mut check = NameCheck::http2(NAME);
    let mut out = Vec::new();
    let mut asked = Vec::new();
    let mut allows = |path: &str| {
        asked.push(path.to_owned());
        path.starts_with("/allowed")
    };
    let mut fed = Ok(());
    for chunk in input.chunks(piece) {
        fed = check.feed_deciding(chunk, &mut out, &mut allows);
        if fed.is_err() {
            break;
        }
    }
    let holding = check.holding();
    let mut frames = Vec::new();
    if !out.is_empty() {
        let mut reader = Reader::client(1 << 20);
        reader.push(&out);
        while let Some(frame) = reader.frame().expect("frames a server reads") {
            frames.push(frame);
        }
        assert!(!reader.holding(), "a frame passed in part");
    }
    Passed {
        frames,
        fed,
        asked,
        holding,
    }
}

/// The headers of each HEADERS frame that passed.
fn heads(frames: &[Frame]) -> Vec<Vec<Header>> {
    (frames.iter())
        .filter_map(|frame| match frame {
            Frame::Headers { headers, .. } => Some(headers.clone()),
            _ => None,
        })
        .collect()
}

/// The data that passed on `stream`, and what flow control counts of it.
fn body(frames: &[Frame], stream: u32) -> (Vec<u8>, usize) {
    let mut body = Vec::new();
    let mut counted = 0;
    for frame in frames {
        if let Frame::Data {
            stream: on,
            data,
            padding,
            ..
        } = frame
            && *on == stream
        {
            body.extend_from_slice(data);
            counted += data.len() + padding.map_or(0, |padding| usize::from(padding) + 1);
        }
    }
    (body, counted)
}

#[test]
fn each_http2_request_is_decided_and_passes_whole() {
    let long: Vec<u8> = (0..40_000).map(|at| at as u8).collect();
    let input = client(&[
        headers(1, request("/allowed/a"), true),
        headers(3, request("/allowed/b"), false),
        Frame::Data {
            stream: 3,
            data: long.clone(),
            padding: Some(9),
            end_stream: false,
        },
        headers(5, request("/allowed/c"), false),
        data(3, b"end", true),
        data(5, b"c", true),
    ]);
    for piece in [1, 7, input.len()] {
        let passed = fed(&input, piece);
        assert_eq!(passed.fed, Ok(()), "in pieces of {piece}");
        assert_eq!(passed.asked, ["/allowed/a", "/allowed/b", "/allowed/c"]);
        assert_eq!(
            heads(&passed.frames),
            ["/allowed/a", "/allowed/b", "/allowed/c"].map(request)
        );
        // Each DATA frame fits what every server takes, and the server's
        // windows count what the client's did.
        let (sent, counted) = body(&passed.frames, 3);
        assert_eq!(sent, [&long[..], b"end"].concat());
        assert_eq!(counted, long.len() + 10 + 3);
        assert_eq!(body(&passed.frames, 5).0, b"c");
        assert!(!passed.holding);
    }
}

// spec(NETWORK): A request that the rules deny never reaches the server, and the proxy resets the connection it came on.
#[test]
fn a_denied_http2_request_never_passes() {
    let input = client(&[
        headers(1, request("/allowed/a"), true),
        headers(3, request("/denied"), false),
        data(3, b"body", true),
    ]);
    let passed = fed(&input, input.len());
    assert_eq!(passed.fed, Err(Refused));
    assert_eq!(heads(&passed.frames), [request("/allowed/a")]);
    assert!(body(&passed.frames, 3).0.is_empty());
}

/// A request for `/allowed`, with `authority` as its `:authority` if it
/// has one, and `extra` after it.
fn naming(authority: Option<&str>, extra: &[(&str, &str)]) -> Vec<Header> {
    let mut headers = request("/allowed");
    headers.retain(|header| header.name != b":authority");
    if let Some(authority) = authority {
        headers.insert(2, header(":authority", authority));
    }
    headers.extend(extra.iter().map(|(name, value)| header(name, value)));
    headers
}

#[test]
fn an_http2_request_must_name_the_host_in_its_authority_and_host() {
    let refused = [
        naming(Some("example.com"), &[]),
        naming(Some(NAME), &[("host", "example.com")]),
        naming(None, &[("host", NAME), ("host", NAME)]),
        naming(None, &[]),
        naming(Some(NAME), &[("host", "")]),
        naming(None, &[("host", "me@host.ap.internal")]),
        naming(Some("host.ap.internal.evil"), &[]),
    ];
    for request in refused {
        let passed = fed(&client(&[headers(1, request.clone(), true)]), 64);
        assert_eq!(passed.fed, Err(Refused), "{request:?}");
        assert!(heads(&passed.frames).is_empty(), "{request:?}");
        assert!(passed.asked.is_empty(), "{request:?}");
    }
    let named = [
        naming(None, &[("host", NAME)]),
        naming(
            Some("HOST.ap.internal."),
            &[("host", "host.ap.internal:443")],
        ),
    ];
    for request in named {
        let passed = fed(&client(&[headers(1, request.clone(), true)]), 64);
        assert_eq!(passed.fed, Ok(()), "{request:?}");
    }
}

// spec(NETWORK): There, the proxy refuses an HTTP/2 request that RFC 9113 calls malformed, or whose `:scheme` isn't `https`, and asks the server never to push.
#[test]
fn a_malformed_http2_request_never_passes() {
    let replaced = |name: &str, value: &str| {
        (request("/allowed").into_iter())
            .map(|field| match field.name == name.as_bytes() {
                true => header(name, value),
                false => field,
            })
            .collect::<Vec<_>>()
    };
    let with = |extra: &[(&str, &str)]| {
        let mut headers = request("/allowed");
        headers.extend(extra.iter().map(|(name, value)| header(name, value)));
        headers
    };
    let without = |name: &str| {
        let mut headers = request("/allowed");
        headers.retain(|header| header.name != name.as_bytes());
        headers
    };
    let mut late = with(&[("accept", "*/*")]);
    late.swap(3, 4);
    let malformed = [
        // Section 8.3.1: each pseudo-header field once, the ones a request
        // has, and before every other field.
        without(":method"),
        without(":scheme"),
        without(":path"),
        with(&[(":path", "/allowed/again")]),
        with(&[(":protocol", "websocket")]),
        with(&[(":status", "200")]),
        late,
        replaced(":method", ""),
        replaced(":method", "GET /denied"),
        replaced(":method", "CONNECT"),
        replaced(":scheme", "http"),
        replaced(":path", ""),
        replaced(":path", "https://host.ap.internal/allowed"),
        replaced(":path", "/allowed /denied"),
        replaced(":path", "/allowed\r\n"),
        // Section 8.2.1: names in lowercase, and values without a line
        // break, a control character, or whitespace at either end.
        with(&[("Accept", "*/*")]),
        with(&[("", "x")]),
        with(&[("x y", "x")]),
        with(&[("x", "a\r\nhost: example.com")]),
        with(&[("x", "a\0")]),
        with(&[("x", " a")]),
        with(&[("x", "a\t")]),
        // Section 8.2.2: fields that are the connection's.
        with(&[("connection", "close")]),
        with(&[("keep-alive", "5")]),
        with(&[("proxy-connection", "close")]),
        with(&[("transfer-encoding", "chunked")]),
        with(&[("upgrade", "websocket")]),
        with(&[("te", "gzip")]),
        // Section 8.1.1: a content-length a server can read one way only.
        with(&[("content-length", "5"), ("content-length", "5")]),
        with(&[("content-length", "5, 5")]),
        with(&[("content-length", "-1")]),
        with(&[("content-length", "")]),
    ];
    for request in malformed {
        let passed = fed(&client(&[headers(1, request.clone(), false)]), 64);
        assert_eq!(passed.fed, Err(Refused), "{request:?}");
        assert!(heads(&passed.frames).is_empty(), "{request:?}");
    }
    let well_formed = with(&[("te", "trailers"), ("x", "a b\tc"), ("cookie", "a=1")]);
    let passed = fed(&client(&[headers(1, well_formed, true)]), 64);
    assert_eq!(passed.fed, Ok(()));
}

#[test]
fn an_http2_path_that_a_server_would_reset_never_passes() {
    // The `http` crate, which the `h2` crate's server reads `:path` with,
    // refuses these, so that server resets the stream.
    let reset = [
        "/allowed/<",
        "/allowed/>",
        "/allowed/`",
        "/allowed?\"",
        "/allowed?<",
        "/allowed?>",
    ];
    for path in reset {
        let passed = fed(&client(&[headers(1, request(path), true)]), 64);
        assert_eq!(passed.fed, Err(Refused), "{path}");
        assert!(heads(&passed.frames).is_empty(), "{path}");
        assert!(passed.asked.is_empty(), "{path}");
    }
    for path in ["/allowed/a", "/allowed/\"{}", "/allowed?`{}"] {
        let passed = fed(&client(&[headers(1, request(path), true)]), 64);
        assert_eq!(passed.fed, Ok(()), "{path}");
        assert_eq!(heads(&passed.frames), [request(path)]);
    }
}

#[test]
fn an_http2_body_has_the_length_its_request_gives() {
    let sized = |length: &str| {
        let mut headers = request("/allowed");
        headers.push(header("content-length", length));
        headers
    };
    // Longer: the byte past it never passes.
    let passed = fed(
        &client(&[
            headers(1, sized("5"), false),
            data(1, b"12345", false),
            data(1, b"6", true),
        ]),
        1,
    );
    assert_eq!(passed.fed, Err(Refused));
    assert_eq!(body(&passed.frames, 1).0, b"12345");
    // Shorter: its end never passes.
    let passed = fed(
        &client(&[headers(1, sized("5"), false), data(1, b"1234", true)]),
        1,
    );
    assert_eq!(passed.fed, Err(Refused));
    assert!(body(&passed.frames, 1).0.is_empty());
    let passed = fed(&client(&[headers(1, sized("5"), true)]), 1);
    assert_eq!(passed.fed, Err(Refused));
    // Its length, with trailers or without.
    let trailers = vec![header("x-sum", "15")];
    let exact = client(&[
        headers(1, sized("5"), false),
        data(1, b"12345", true),
        headers(3, sized("0"), true),
        headers(5, sized("2"), false),
        data(5, b"12", false),
        headers(5, trailers, true),
    ]);
    assert_eq!(fed(&exact, 1).fed, Ok(()));
}

#[test]
fn http2_trailers_end_their_stream_and_carry_no_pseudo_header_field() {
    let refused = [
        (vec![header("x-sum", "1")], false),
        (vec![header(":path", "/denied")], true),
        (vec![header("host", "example.com")], true),
        (vec![header("content-length", "9")], true),
        (vec![header("X-Sum", "1")], true),
    ];
    for (trailers, end_stream) in refused {
        let input = client(&[
            headers(1, request("/allowed"), false),
            data(1, b"a", false),
            headers(1, trailers.clone(), end_stream),
        ]);
        let passed = fed(&input, 64);
        assert_eq!(passed.fed, Err(Refused), "{trailers:?}");
        assert_eq!(heads(&passed.frames), [request("/allowed")]);
    }
}

#[test]
fn the_client_cancelling_a_stream_it_ended_reaches_the_server() {
    let reset = Frame::RstStream {
        stream: 1,
        code: 0x8,
    };
    let update = Frame::WindowUpdate {
        stream: 1,
        increment: 100,
    };
    let input = client(&[
        headers(1, request("/allowed"), true),
        update.clone(),
        reset.clone(),
    ]);
    let passed = fed(&input, 64);
    assert_eq!(passed.fed, Ok(()));
    assert_eq!(
        passed.frames[1..],
        [headers(1, request("/allowed"), true), update, reset]
    );
}

#[test]
fn the_server_is_asked_never_to_push() {
    let input = client(&[Frame::Settings(vec![(ENABLE_PUSH, 1), (0x99, 7)])]);
    let passed = fed(&input, 64);
    assert_eq!(passed.fed, Ok(()));
    let settings: Vec<_> = (passed.frames.iter())
        .filter_map(|frame| match frame {
            Frame::Settings(settings) => Some(settings.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        settings,
        [
            vec![(INITIAL_WINDOW_SIZE, 1 << 20), (ENABLE_PUSH, 0)],
            vec![(0x99, 7), (ENABLE_PUSH, 0)],
        ]
    );
}

// spec(NETWORK): It passes the guest's frames on without priority signals, frames of a type it doesn't know, or the debug data of `GOAWAY`.
#[test]
fn http2_frames_pass_without_priority_unknown_types_or_goaway_debug() {
    let priority = Priority {
        exclusive: true,
        dependency: 1,
        weight: 200,
    };
    let mut input = client(&[
        Frame::Headers {
            stream: 1,
            headers: request("/allowed"),
            priority: Some(priority),
            end_stream: true,
        },
        Frame::Priority {
            stream: 3,
            priority,
        },
        Frame::Ping {
            ack: false,
            data: *b"pingpong",
        },
    ]);
    // A frame of type 0xbe, which nothing defines.
    input.extend([0, 0, 3, 0xbe, 0, 0, 0, 0, 0]);
    input.extend(b"odd");
    let mut writer = Writer::new();
    let goaway = Frame::GoAway {
        last_stream: 1,
        code: 0,
        debug: b"why".to_vec(),
    };
    writer.write(&goaway, &mut input);
    let passed = fed(&input, 5);
    assert_eq!(passed.fed, Ok(()));
    assert_eq!(
        passed.frames[1..],
        [
            headers(1, request("/allowed"), true),
            Frame::Ping {
                ack: false,
                data: *b"pingpong",
            },
            Frame::GoAway {
                last_stream: 1,
                code: 0,
                debug: Vec::new(),
            },
        ]
    );
}

#[test]
fn a_stream_or_connection_error_ends_the_http2_check() {
    let errors = [
        // DATA on an idle stream, section 5.1.
        vec![data(1, b"a", true)],
        // HEADERS on an even stream, section 5.1.1.
        vec![headers(2, request("/allowed"), true)],
        // DATA after the client ended its stream, section 5.1.
        vec![
            headers(1, request("/allowed"), false),
            data(1, b"a", true),
            data(1, b"b", true),
        ],
        // An update of 0, section 6.9.
        vec![
            headers(1, request("/allowed"), false),
            Frame::WindowUpdate {
                stream: 1,
                increment: 0,
            },
        ],
    ];
    for frames in errors {
        assert_eq!(fed(&client(&frames), 64).fed, Err(Refused), "{frames:?}");
    }
    // Something other than the preface.
    let passed = fed(
        b"GET /allowed HTTP/1.1\r\nHost: host.ap.internal\r\n\r\n",
        64,
    );
    assert_eq!(passed.fed, Err(Refused));
    assert!(passed.frames.is_empty());
}

#[test]
fn part_of_an_http2_frame_is_held_until_the_rest_comes() {
    let input = client(&[headers(1, request("/allowed"), true)]);
    let passed = fed(&input[..input.len() - 1], input.len());
    assert_eq!(passed.fed, Ok(()));
    assert!(passed.holding);
    assert!(heads(&passed.frames).is_empty());
    // A block whose CONTINUATION hasn't come.
    let mut writer = Writer::new();
    let mut long = request("/allowed");
    long.push(header("x-long", &"a".repeat(2 * FRAME)));
    let mut out = Vec::new();
    writer.write(&headers(1, long, true), &mut out);
    let mut input = client(&[]);
    input.extend_from_slice(&out[..FRAME + 9]);
    assert!(fed(&input, 64).holding);
}
