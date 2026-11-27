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
pub mod source;
pub mod store;
pub mod unified;

/// `text` with each control character escaped, so that nothing an agent or
/// a remote named, such as a file, can drive your terminal when it is
/// printed.
pub(crate) fn printable(text: &str) -> String {
    let escape = |c: char| match c.is_control() {
        true => c.escape_default().to_string(),
        false => c.to_string(),
    };
    text.chars().map(escape).collect()
}

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
