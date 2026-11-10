//! Checks the arguments of `ADD`, `REPO`, `PLUGIN`, `SKILL`, and `MCP`.

use crate::Instruction;

/// Accepts every argument for now.
pub(crate) fn check(_instruction: &Instruction) -> Result<(), String> {
    Ok(())
}
