//! `ap rm <agent>`, from SPEC.md's `ap rm` section. It removes the agent's
//! record, and frees the layers no other agent uses, as the Build section
//! says. The approval stays, because it belongs to the Agentfile's path and
//! not to the agent.
//!
//! An agent has no persisted state, runs, kept repositories, or pending
//! changes yet, so there's nothing else to remove, and nothing to refuse for.

use std::ffi::OsString;

/// Reads `ap rm`'s arguments: one agent's name. Anything else is bad usage.
pub fn parse(args: &[OsString]) -> Option<String> {
    match args {
        [name] if !name.as_encoded_bytes().starts_with(b"-") => Some(name.to_str()?.to_string()),
        _ => None,
    }
}

/// Removes the agent, and returns the exit status.
pub fn run(name: &str) -> u8 {
    match remove(name) {
        Ok(()) => 0,
        Err(message) => {
            eprintln!("{message}");
            1
        }
    }
}

fn remove(name: &str) -> Result<(), String> {
    let home = std::env::var_os("HOME");
    let root = ap::store::default_root(
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        home.as_deref(),
    )
    .ok_or("ap: HOME isn't an absolute path, so there's no place for the store")?;
    let unknown = || {
        let shown = agentfile::print_argument(name);
        format!("ap: no agent named {shown}")
    };
    if !::store::valid_name(name) {
        return Err(unknown());
    }
    match ::store::Agents::new(root).remove(name) {
        Ok(true) => Ok(()),
        Ok(false) => Err(unknown()),
        Err(error) => Err(format!("ap: {error}")),
    }
}
