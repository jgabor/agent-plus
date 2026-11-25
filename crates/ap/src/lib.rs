//! The `ap` program.

use std::io;

pub mod adapter;
pub mod add;
pub mod approval;
pub mod boot;
pub mod fetch;
pub mod filter;
pub mod gitignore;
pub mod json;
pub mod lock;
pub mod network;
pub mod paths;
pub mod pending;
pub mod runs;
pub mod store;
pub mod unified;

/// What a call that only Linux has fails with on macOS, where `ap` doesn't
/// run yet.
#[cfg(target_os = "macos")]
pub fn not_on_macos() -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, "ap doesn't run on macOS yet")
}

/// A running guest. The real implementation is the VMM, and lanes that need
/// a guest before it exists build against `fake-vm`.
pub trait Vm {
    /// Runs a command in the guest, and returns its exit code.
    fn run(&mut self, command: &[&str]) -> io::Result<i32>;
}
