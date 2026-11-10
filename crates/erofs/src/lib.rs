//! Writes a directory tree as an uncompressed EROFS image.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Read, Write};

/// A directory, with its entries by name.
pub struct Dir {
    pub mode: u16,
    pub entries: BTreeMap<Vec<u8>, Node>,
}

pub enum Node {
    Dir(Dir),
    File(File),
    Symlink(Symlink),
}

/// A regular file. `content` must yield exactly `size` bytes.
pub struct File {
    pub mode: u16,
    pub size: u64,
    pub content: Box<dyn Read>,
}

pub struct Symlink {
    pub mode: u16,
    pub target: Vec<u8>,
}

#[derive(Debug)]
pub enum Error {
    TooLarge { path: Vec<u8>, size: u64 },
    ImageTooLarge { blocks: u64 },
    Invalid { path: Vec<u8>, reason: &'static str },
    Content { path: Vec<u8>, size: u64 },
    Io(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => e.fmt(f),
            _ => Ok(()),
        }
    }
}

impl std::error::Error for Error {}

pub fn write(_root: Dir, _out: &mut dyn Write) -> Result<(), Error> {
    Ok(())
}
