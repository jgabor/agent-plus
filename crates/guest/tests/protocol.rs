//! The frames, and what the guest and the host refuse to read.

use guest::protocol::{MAX_BODY, MAX_OUTPUT, ProtocolError, Start, ToGuest, ToHost};
use std::io::{self, Cursor, Read};

const RUN: u8 = 1;
const STDOUT: u8 = 2;
const EXITED: u8 = 4;
const READY: u8 = 5;
const START: u8 = 6;
const FAILED: u8 = 7;

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

fn start(prompt: Option<&[u8]>) -> ToGuest {
    ToGuest::Start(Start {
        argv: vec![b"/usr/local/bin/claude".to_vec(), b"-p".to_vec()],
        env: vec![
            (b"HOME".to_vec(), b"/home/agent".to_vec()),
            (b"EMPTY".to_vec(), Vec::new()),
            (b"EQUALS".to_vec(), b"a=b".to_vec()),
        ],
        directory: b"/home/agent".to_vec(),
        prompt: prompt.map(<[u8]>::to_vec),
    })
}

#[test]
fn every_message_survives_the_wire() {
    for message in [
        run(&[b"sh"]),
        run(&[b"sh", b"", b"\xff a"]),
        run(&[b""]),
        start(None),
        start(Some(b"Say what you can see.")),
        start(Some(b"")),
    ] {
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
        ToHost::Failed(b"no prompt".to_vec()),
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
fn a_start_counts_its_arguments_and_variables_and_ends_each_string_with_a_nul() {
    let mut wire = Vec::new();
    ToGuest::Start(Start {
        argv: vec![b"h".to_vec()],
        env: vec![(b"K".to_vec(), b"v".to_vec())],
        directory: b"/d".to_vec(),
        prompt: Some(b"p".to_vec()),
    })
    .write(&mut wire)
    .unwrap();
    let body = [
        &[START][..],
        &1u32.to_le_bytes(),
        &1u32.to_le_bytes(),
        b"h\0K=v\0/d\0p\0",
    ]
    .concat();
    assert_eq!(wire, frame(&body));
}

#[test]
fn a_start_that_execve_or_chdir_would_refuse_is_never_written() {
    let base = Start {
        argv: vec![b"h".to_vec()],
        env: Vec::new(),
        directory: b"/".to_vec(),
        prompt: None,
    };
    let refused = [
        Start {
            argv: Vec::new(),
            ..base.clone()
        },
        Start {
            argv: vec![b"h\0".to_vec()],
            ..base.clone()
        },
        Start {
            env: vec![(Vec::new(), b"v".to_vec())],
            ..base.clone()
        },
        Start {
            env: vec![(b"K=".to_vec(), b"v".to_vec())],
            ..base.clone()
        },
        Start {
            env: vec![(b"K".to_vec(), b"v\0".to_vec())],
            ..base.clone()
        },
        Start {
            directory: b"/\0".to_vec(),
            ..base.clone()
        },
        Start {
            prompt: Some(b"p\0".to_vec()),
            ..base
        },
    ];
    for start in refused {
        let message = ToGuest::Start(start);
        assert!(
            matches!(
                message.write(&mut Vec::new()),
                Err(ProtocolError::Malformed)
            ),
            "{message:?}"
        );
    }
}

#[test]
fn a_start_whose_counts_dont_match_its_strings_is_refused() {
    let body = |argc: u32, envc: u32, strings: &[u8]| {
        [
            &[START][..],
            &argc.to_le_bytes(),
            &envc.to_le_bytes(),
            strings,
        ]
        .concat()
    };
    let bad: [Vec<u8>; 9] = [
        vec![START],
        vec![START, 1, 0, 0, 0],
        body(1, 0, b""),
        body(1, 0, b"h\0"),
        body(0, 0, b"/d\0"),
        body(1, 1, b"h\0/d\0"),
        body(1, 1, b"h\0=v\0/d\0"),
        body(1, 0, b"h\0/d\0p\0extra\0"),
        body(u32::MAX, u32::MAX, b"h\0/d\0"),
    ];
    for body in bad {
        assert!(
            matches!(to_guest(frame(&body)), Err(ProtocolError::Malformed)),
            "{body:?}"
        );
    }
    assert!(to_guest(frame(&body(1, 0, b"h\0/d"))).is_err());
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
    let mut wire = Vec::new();
    start(None).write(&mut wire).unwrap();
    assert!(matches!(
        to_host(wire),
        Err(ProtocolError::UnknownTag(START))
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
        if tag != RUN && tag != START {
            assert!(matches!(
                to_guest(frame(&[tag, 0])),
                Err(ProtocolError::UnknownTag(found)) if found == tag
            ));
        }
        if !(STDOUT..=READY).contains(&tag) && tag != FAILED {
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

/// The limit is part of the format, so it is spelled out in bytes here.
#[test]
fn a_body_holds_at_most_64_kib() {
    let mut body = vec![STDOUT];
    body.resize(65_536, b'x');
    assert!(to_host(frame(&body)).is_ok());
    body.push(b'x');
    assert!(matches!(
        to_host(frame(&body)),
        Err(ProtocolError::TooLarge(65_537))
    ));
}

#[test]
fn the_largest_body_is_written() {
    let mut wire = Vec::new();
    ToHost::Stdout(vec![b'x'; MAX_BODY - 1])
        .write(&mut wire)
        .unwrap();
    assert_eq!(wire.len(), 4 + MAX_BODY);
}

/// Fails the first read with `error`, then reads `bytes`.
struct FailsFirst {
    error: Option<io::ErrorKind>,
    bytes: Cursor<Vec<u8>>,
}

impl Read for FailsFirst {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self.error.take() {
            Some(kind) => Err(kind.into()),
            None => self.bytes.read(buffer),
        }
    }
}

#[test]
fn an_interrupted_read_is_retried_and_any_other_failure_is_an_error() {
    let mut wire = Vec::new();
    ToHost::Exited(3).write(&mut wire).unwrap();
    let mut interrupted = FailsFirst {
        error: Some(io::ErrorKind::Interrupted),
        bytes: Cursor::new(wire.clone()),
    };
    assert_eq!(
        ToHost::read(&mut interrupted).unwrap(),
        Some(ToHost::Exited(3))
    );
    let mut reset = FailsFirst {
        error: Some(io::ErrorKind::ConnectionReset),
        bytes: Cursor::new(wire),
    };
    assert!(matches!(
        ToHost::read(&mut reset),
        Err(ProtocolError::Io(error)) if error.kind() == io::ErrorKind::ConnectionReset
    ));
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
