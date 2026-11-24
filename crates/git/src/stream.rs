//! Inflates a zlib stream from a file a piece at a time, so reading it holds
//! one piece of the file, whatever length the file says it has.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

use miniz_oxide::inflate::stream::{InflateState, inflate};
use miniz_oxide::{DataFormat, MZError, MZFlush, MZStatus};

/// The most of a file read at a time.
pub(crate) const CHUNK: u64 = 1 << 16;

/// Why a stream couldn't be inflated.
pub(crate) enum Why {
    Io(io::Error),
    /// The file ends inside the stream.
    Short,
    NotZlib,
}

impl From<io::Error> for Why {
    fn from(error: io::Error) -> Why {
        Why::Io(error)
    }
}

/// A zlib stream that starts at `at` in a file, and can't go past `end`.
pub(crate) struct Stream<'a> {
    file: &'a File,
    at: u64,
    end: u64,
    state: Box<InflateState>,
    input: Vec<u8>,
    /// What of `input` is read but not yet inflated.
    start: usize,
    len: usize,
}

impl Stream<'_> {
    /// Reads at most `piece` bytes of the file at a time.
    pub(crate) fn new(file: &File, at: u64, end: u64, piece: usize) -> Stream<'_> {
        Stream {
            file,
            at,
            end,
            state: InflateState::new_boxed(DataFormat::Zlib),
            input: vec![0; piece.max(1)],
            start: 0,
            len: 0,
        }
    }

    /// Inflates into `output`, which mustn't be empty, reading more of the
    /// file as it needs to. Returns how many bytes it wrote, and whether the
    /// stream ended. It writes at least one byte unless the stream ends.
    pub(crate) fn next(&mut self, output: &mut [u8]) -> Result<(usize, bool), Why> {
        loop {
            // Zlib may hold output it had no room for, so it is asked even
            // when the file has no more to give.
            if self.start == self.len && self.at < self.end {
                let len = (self.end - self.at).min(self.input.len() as u64) as usize;
                self.file.read_exact_at(&mut self.input[..len], self.at)?;
                self.at += len as u64;
                (self.start, self.len) = (0, len);
            }
            let input = &self.input[self.start..self.len];
            let result = inflate(&mut self.state, input, output, MZFlush::None);
            self.start += result.bytes_consumed;
            match result.status {
                Ok(MZStatus::StreamEnd) => return Ok((result.bytes_written, true)),
                Ok(_) | Err(MZError::Buf) => {}
                Err(_) => return Err(Why::NotZlib),
            }
            if result.bytes_written > 0 {
                return Ok((result.bytes_written, false));
            }
            if result.bytes_consumed == 0 {
                // With input and room for output, zlib either moves or
                // fails, so only the end of the file stops it.
                return Err(match input.is_empty() {
                    true => Why::Short,
                    false => Why::NotZlib,
                });
            }
        }
    }
}
