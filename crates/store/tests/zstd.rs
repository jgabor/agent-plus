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
    let frame = store::compress(&data);
    assert_eq!(frame[..4], [0x28, 0xB5, 0x2F, 0xFD], "zstd frame magic");
    assert!(frame.len() < data.len() / 4, "frame is {} bytes", frame.len());
    assert_eq!(store::decompress(&frame).unwrap(), data);
}

#[test]
fn an_empty_input_round_trips() {
    let frame = store::compress(&[]);
    assert_eq!(store::decompress(&frame).unwrap(), Vec::<u8>::new());
}

#[test]
fn a_corrupt_frame_is_an_error() {
    let mut frame = store::compress(&sample());
    frame[0] ^= 0xFF;
    assert!(store::decompress(&frame).is_err());
}
