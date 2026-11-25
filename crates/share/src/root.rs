//! What the share server takes: each share's root, and what it allows.

use std::ffi::CString;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

/// What a share allows. A copy-on-write share keeps the agent's changes
/// in a directory of its own, `P`.
#[derive(Debug)]
pub enum Mode<P = OwnedFd> {
    ReadOnly,
    ReadWrite,
    CopyOnWrite(P),
}

/// A share's root, as the server takes it: a directory, and what the share
/// allows. A file share shows one `file` in the directory, by its name, and
/// nothing else of it.
#[derive(Debug)]
pub struct Root<F = OwnedFd> {
    pub directory: F,
    pub file: Option<CString>,
    pub mode: Mode<F>,
}

impl<F> Root<F> {
    /// The share of the directory `directory`.
    pub fn new(directory: F, mode: Mode<F>) -> Self {
        Self {
            directory,
            file: None,
            mode,
        }
    }

    /// The share of the file `name` in `directory`.
    pub fn file(directory: F, name: CString, mode: Mode<F>) -> Self {
        Self {
            directory,
            file: Some(name),
            mode,
        }
    }
}

impl<F: AsFd> Root<F> {
    /// The same root, borrowing its descriptors.
    pub fn as_fd(&self) -> Root<BorrowedFd<'_>> {
        Root {
            directory: self.directory.as_fd(),
            file: self.file.clone(),
            mode: match &self.mode {
                Mode::ReadOnly => Mode::ReadOnly,
                Mode::ReadWrite => Mode::ReadWrite,
                Mode::CopyOnWrite(pending) => Mode::CopyOnWrite(pending.as_fd()),
            },
        }
    }
}
