//! A frame that `store` writes is a zstd frame, and `store` reads it back.

/// Bytes with enough repetition for the encoder to find matches.
fn sample() -> Vec<u8> {
    let mut data = Vec::new();
    for i in 0..10_000u32 {
        data.extend_from_slice(format!("line {} of a layer\n", i % 97).as_bytes());
    }
    data
}

#[test]
fn a_frame_round_trips() {
    let data = sample();
    let frame = store::compress(&data, 3);
    assert_eq!(frame[..4], [0x28, 0xB5, 0x2F, 0xFD], "zstd frame magic");
    assert!(
        frame.len() < data.len() / 4,
        "frame is {} bytes",
        frame.len()
    );
    assert_eq!(store::decompress(&frame).unwrap(), data);
}

#[test]
fn an_empty_input_round_trips() {
    let frame = store::compress(&[], 3);
    assert_eq!(store::decompress(&frame).unwrap(), Vec::<u8>::new());
}

#[test]
fn a_corrupt_frame_is_an_error() {
    let mut frame = store::compress(&sample(), 3);
    frame[0] ^= 0xFF;
    assert!(store::decompress(&frame).is_err());
}

#[test]
fn a_frame_whose_content_fails_its_checksum_is_an_error() {
    let mut frame = store::compress(b"checksum example", 3);
    // A raw block's literal byte, past the frame and block headers.
    frame[9] ^= 1;
    assert!(store::decompress(&frame).is_err());
}

#[test]
fn every_level_makes_a_frame_that_round_trips() {
    let data = sample();
    for level in (-7..=-1).chain(1..=22) {
        let frame = store::compress(&data, level);
        assert_eq!(frame[..4], [0x28, 0xB5, 0x2F, 0xFD], "level {level}");
        assert_eq!(store::decompress(&frame).unwrap(), data, "level {level}");
    }
}

#[test]
fn a_higher_level_makes_a_smaller_frame() {
    let data = varied();
    let size = |level| store::compress(&data, level).len();
    // A fast mode trades ratio for speed, and so does each lower level.
    assert!(size(-7) > size(-1), "{} {}", size(-7), size(-1));
    assert!(size(-1) > size(1), "{} {}", size(-1), size(1));
    assert!(size(1) > size(3), "{} {}", size(1), size(3));
    assert!(size(3) > size(19), "{} {}", size(3), size(19));
}

/// Bytes that are neither random nor easy, so each level finds a different
/// set of matches: words picked by a fixed pseudorandom sequence.
fn varied() -> Vec<u8> {
    const WORDS: [&str; 16] = [
        "agent", "layer", "store", "guest", "host", "grant", "frame", "level", "image", "build",
        "share", "model", "shell", "token", "cache", "block",
    ];
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut data = Vec::new();
    while data.len() < 1 << 20 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        data.extend_from_slice(WORDS[(state % 16) as usize].as_bytes());
        data.push(if state.is_multiple_of(11) {
            b'\n'
        } else {
            b' '
        });
        if state.is_multiple_of(5) {
            data.extend_from_slice(format!("{}", state % 1000).as_bytes());
        }
    }
    data
}
