//! `ap ls`, from SPEC.md's `ap ls` section. It prints one line for each
//! built agent, in name order: its name, and the directory it was built
//! from. A directory can hold any character, so it's printed the way an
//! Agentfile argument is, with control characters escaped.

use std::ffi::OsString;

/// Reads `ap ls`'s arguments, of which there are none.
pub fn parse(args: &[OsString]) -> Option<()> {
    args.is_empty().then_some(())
}

/// Lists the agents, and returns the exit status.
pub fn run() -> u8 {
    match list() {
        Ok(lines) => {
            print!("{lines}");
            0
        }
        Err(message) => {
            eprintln!("{message}");
            1
        }
    }
}

fn list() -> Result<String, String> {
    let home = std::env::var_os("HOME");
    let root = ap::store::default_root(
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        home.as_deref(),
    )
    .ok_or("ap: HOME isn't an absolute path, so there's no place for the store")?;
    let agents = ::store::Agents::new(root)
        .list()
        .map_err(|error| format!("ap: {error}"))?;
    Ok(agents
        .iter()
        .map(|agent| {
            let directory = agentfile::print_argument(&agent.directory);
            format!("{} {directory}\n", agent.name)
        })
        .collect())
}
