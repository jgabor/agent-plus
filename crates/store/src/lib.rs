//! The local store's layers, the zstd frames they're kept in, the agents
//! built from them, and the grants you approved, in pure Rust.
//!
//! The encoder is `structured-zstd`'s, which follows zstd's levels, and the
//! decoder is `ruzstd`'s. A frame is a standard zstd frame, and any zstd
//! decoder reads it.

use std::io::{self, Read};

use ruzstd::decoding::StreamingDecoder;
use structured_zstd::encoding::{CompressionLevel, FrameCompressor};

mod agents;
mod approvals;
mod layers;
mod runs;

pub use agents::{Agent, Agents, Declared, Kind, Layer, valid_name};
pub use approvals::Approvals;
pub use layers::Layers;
pub use runs::{NAME_ALPHABET, NAME_LEN, Run, Runs, random_name, record_exit, valid_run_name};

/// Compresses `data` into one zstd frame at a zstd level: `1` to `22`, or a
/// fast mode from `-1` to `-7`. The frame carries a content checksum.
pub fn compress(data: &[u8], level: i8) -> Vec<u8> {
    let mut encoder: FrameCompressor = FrameCompressor::new(CompressionLevel::Level(level.into()));
    encoder.set_content_checksum(true);
    encoder.compress_independent_frame(data)
}

/// Decompresses the zstd frame at the start of `frame`, and checks its content
/// checksum if it has one.
pub fn decompress(frame: &[u8]) -> io::Result<Vec<u8>> {
    let mut data = Vec::new();
    decompress_to(frame, |bytes| {
        data.extend_from_slice(bytes);
        Ok(())
    })?;
    Ok(data)
}

/// Decompresses the zstd frame at the start of `frame` a piece at a time,
/// passing each piece to `out`, and checks its content checksum if it has
/// one.
pub(crate) fn decompress_to(
    frame: impl Read,
    mut out: impl FnMut(&[u8]) -> io::Result<()>,
) -> io::Result<()> {
    let mut decoder = StreamingDecoder::new(frame).map_err(io::Error::other)?;
    let mut piece = vec![0; 64 << 10];
    loop {
        match decoder.read(&mut piece) {
            Ok(0) => break,
            Ok(len) => out(&piece[..len])?,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    let stored = decoder.decoder.get_checksum_from_data();
    if stored.is_some() && stored != decoder.decoder.get_calculated_checksum() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "zstd frame fails its content checksum",
        ));
    }
    Ok(())
}
