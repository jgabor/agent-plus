//! Checks the arguments of `SHARE`, `WORKDIR`, `PERSIST`, `PERMISSIONS`, and `PORT`.

use crate::Instruction;

/// Accepts every argument for now.
pub(crate) fn check(_instruction: &Instruction) -> Result<(), String> {
    Ok(())
}
