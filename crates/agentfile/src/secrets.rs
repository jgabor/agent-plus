//! Checks the arguments of `ENV`, `SECRET`, `SEAL`, and `AUTH`.

use crate::Instruction;

/// Accepts every argument for now.
pub(crate) fn check(_instruction: &Instruction) -> Result<(), String> {
    Ok(())
}
