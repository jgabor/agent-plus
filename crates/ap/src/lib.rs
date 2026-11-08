//! The `ap` program.

use std::io;

/// A running guest. The real implementation is the VMM, and lanes that need
/// a guest before it exists build against `fake-vm`.
pub trait Vm {
    /// Runs a command in the guest, and returns its exit code.
    fn run(&mut self, command: &[&str]) -> io::Result<i32>;
}
