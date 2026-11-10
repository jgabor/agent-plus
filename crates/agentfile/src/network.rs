//! Checks the arguments of `NETWORK` and `POLICY`.

use crate::Instruction;

/// Accepts every argument for now.
pub(crate) fn check(_instruction: &Instruction) -> Result<(), String> {
    Ok(())
}
