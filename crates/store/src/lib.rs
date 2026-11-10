//! The local store's layers, and the zstd frames they're kept in, in pure
//! Rust.
//!
//! The encoder is `ruzstd`'s, which only has a mode close to zstd's level 1.
//! A frame it writes is a standard zstd frame, and any zstd decoder reads it.

use std::io::{self, Read};

use ruzstd::decoding::StreamingDecoder;
use ruzstd::encoding::{CompressionLevel, compress_to_vec};

mod layers;

pub use layers::Layers;

/// Compresses `data` into one zstd frame.
pub fn compress(data: &[u8]) -> Vec<u8> {
    compress_to_vec(data, CompressionLevel::Fastest)
}

/// Decompresses the zstd frame at the start of `frame`, and checks its content
/// checksum if it has one.
pub fn decompress(frame: &[u8]) -> io::Result<Vec<u8>> {
    let mut decoder = StreamingDecoder::new(frame).map_err(io::Error::other)?;
    let mut data = Vec::new();
    decoder.read_to_end(&mut data)?;
    let stored = decoder.decoder.get_checksum_from_data();
    if stored.is_some() && stored != decoder.decoder.get_calculated_checksum() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "zstd frame fails its content checksum",
        ));
    }
    Ok(data)
}
