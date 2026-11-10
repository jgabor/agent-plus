//! Checks the arguments of `AGENT`, `OS`, `MODEL`, `EFFORT`, `MODE`, and
//! `COMPRESSION`.

use crate::Instruction;

/// Accepts every argument for now.
pub(crate) fn check(_instruction: &Instruction) -> Result<(), String> {
    Ok(())
}
