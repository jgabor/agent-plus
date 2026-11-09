//! The frames, and what the guest and the host refuse to read.

use guest::protocol::{MAX_BODY, MAX_OUTPUT, ProtocolError, ToGuest, ToHost};
use std::io::Cursor;

const RUN: u8 = 1;
const STDOUT: u8 = 2;
const EXITED: u8 = 4;
const READY: u8 = 5;

fn frame(body: &[u8]) -> Vec<u8> {
    let mut bytes = (body.len() as u32).to_le_bytes().to_vec();
    bytes.extend_from_slice(body);
    bytes
}

fn to_guest(bytes: Vec<u8>) -> Result<Option<ToGuest>, ProtocolError> {
    ToGuest::read(&mut Cursor::new(bytes))
}

fn to_host(bytes: Vec<u8>) -> Result<Option<ToHost>, ProtocolError> {
    ToHost::read(&mut Cursor::new(bytes))
}

fn run(argv: &[&[u8]]) -> ToGuest {
    ToGuest::Run {
        argv: argv.iter().map(|arg| arg.to_vec()).collect(),
    }
}

#[test]
fn every_message_survives_the_wire() {
    for message in [run(&[b"sh"]), run(&[b"sh", b"", b"\xff a"]), run(&[b""])] {
        let mut wire = Vec::new();
        message.write(&mut wire).unwrap();
        assert_eq!(to_guest(wire).unwrap(), Some(message));
    }
    for message in [
        ToHost::Stdout(b"out".to_vec()),
        ToHost::Stderr(vec![0, 255]),
        ToHost::Stdout(Vec::new()),
        ToHost::Exited(0),
        ToHost::Exited(255),
    ] {
        let mut wire = Vec::new();
        message.write(&mut wire).unwrap();
        assert_eq!(to_host(wire).unwrap(), Some(message));
    }
}

#[test]
fn the_format_is_a_little_endian_length_a_tag_and_a_payload() {
    let mut wire = Vec::new();
    run(&[b"a", b"bc"]).write(&mut wire).unwrap();
    assert_eq!(wire, [6, 0, 0, 0, RUN, b'a', 0, b'b', b'c', 0]);

    let mut wire = Vec::new();
    ToHost::Exited(7).write(&mut wire).unwrap();
    assert_eq!(wire, [2, 0, 0, 0, EXITED, 7]);
}

#[test]
fn ready_carries_the_kernel_release() {
    let ready = ToHost::Ready(b"7.3.3-ap.1".to_vec());
    let mut bytes = Vec::new();
    ready.write(&mut bytes).unwrap();
    assert_eq!(bytes, frame(&[&[READY], &b"7.3.3-ap.1"[..]].concat()));
    assert_eq!(to_host(bytes).unwrap(), Some(ready));
}

#[test]
fn an_end_between_frames_is_clean() {
    assert!(to_guest(Vec::new()).unwrap().is_none());
    assert!(to_host(Vec::new()).unwrap().is_none());
}

#[test]
fn no_message_to_the_host_can_ask_it_to_run_anything() {
    assert!(matches!(
        to_host(frame(&[RUN, b's', b'h', 0])),
        Err(ProtocolError::UnknownTag(RUN))
    ));
}

#[test]
fn the_guest_refuses_what_only_the_host_reads() {
    assert!(matches!(
        to_guest(frame(&[STDOUT, b'x'])),
        Err(ProtocolError::UnknownTag(STDOUT))
    ));
}

#[test]
fn every_unknown_tag_is_refused() {
    for tag in 0..=u8::MAX {
        if tag != RUN {
            assert!(matches!(
                to_guest(frame(&[tag, 0])),
                Err(ProtocolError::UnknownTag(found)) if found == tag
            ));
        }
        if !(STDOUT..=READY).contains(&tag) {
            assert!(matches!(
                to_host(frame(&[tag, 0])),
                Err(ProtocolError::UnknownTag(found)) if found == tag
            ));
        }
    }
}

#[test]
fn an_oversized_length_is_refused_before_its_body_arrives() {
    for length in [MAX_BODY as u32 + 1, u32::MAX] {
        let header = length.to_le_bytes().to_vec();
        assert!(matches!(
            to_guest(header.clone()),
            Err(ProtocolError::TooLarge(found)) if found == length as usize
        ));
        assert!(matches!(
            to_host(header),
            Err(ProtocolError::TooLarge(found)) if found == length as usize
        ));
    }
}

#[test]
fn the_largest_body_is_accepted() {
    let mut body = vec![STDOUT];
    body.resize(MAX_BODY, b'x');
    let Some(ToHost::Stdout(output)) = to_host(frame(&body)).unwrap() else {
        panic!("not stdout");
    };
    assert_eq!(output.len(), MAX_BODY - 1);
}

#[test]
fn malformed_bodies_are_refused() {
    let bad_runs: [&[u8]; 3] = [&[], &[RUN], &[RUN, b's', b'h']];
    for body in bad_runs {
        assert!(
            matches!(to_guest(frame(body)), Err(ProtocolError::Malformed)),
            "{body:?}"
        );
    }
    let bad_reports: [&[u8]; 3] = [&[], &[EXITED], &[EXITED, 1, 2]];
    for body in bad_reports {
        assert!(
            matches!(to_host(frame(body)), Err(ProtocolError::Malformed)),
            "{body:?}"
        );
    }
}

#[test]
fn a_stream_cut_inside_a_frame_is_an_error() {
    let mut wire = Vec::new();
    run(&[b"sh", b"-c"]).write(&mut wire).unwrap();
    for length in 1..wire.len() {
        assert!(
            matches!(to_guest(wire[..length].to_vec()), Err(ProtocolError::Io(_))),
            "{length}"
        );
    }
}

#[test]
fn a_writer_cannot_make_a_frame_the_reader_would_misread() {
    let mut wire = Vec::new();
    assert!(matches!(
        run(&[b"a\0b"]).write(&mut wire),
        Err(ProtocolError::Malformed)
    ));
    assert!(matches!(
        ToGuest::Run { argv: Vec::new() }.write(&mut wire),
        Err(ProtocolError::Malformed)
    ));
    assert!(matches!(
        ToHost::Stdout(vec![0; MAX_BODY]).write(&mut wire),
        Err(ProtocolError::TooLarge(_))
    ));
    assert!(wire.is_empty());
}

#[test]
fn a_full_output_chunk_fits_in_a_frame() {
    let mut wire = Vec::new();
    ToHost::Stderr(vec![0; MAX_OUTPUT])
        .write(&mut wire)
        .unwrap();
}

/// Any bytes at all give an answer, never a panic.
#[test]
fn arbitrary_bytes_never_panic() {
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..20_000 {
        let length = (next() % 32) as usize;
        let mut bytes: Vec<u8> = (0..length).map(|_| next() as u8).collect();
        // Half the inputs get a plausible length, so that bodies get parsed.
        if next() % 2 == 0 {
            bytes = frame(&bytes);
        }
        let _ = to_guest(bytes.clone());
        let _ = to_host(bytes);
    }
}
