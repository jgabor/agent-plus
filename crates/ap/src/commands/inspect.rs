//! `ap inspect [--grants] <agent>`, from SPEC.md's `ap inspect` section. It
//! shows the agent's layers, in order, each with its digest and the
//! instruction that made it, and the grants you approved for its Agentfile.
//! `--grants` prints only the grants, in the text form that
//! `--approve=<file>` reads.
//!
//! Git credentials and the harness's own hosts aren't shown yet, because
//! nothing fetches a source or resolves a harness.

use std::ffi::OsString;
use std::path::Path;

/// The arguments of `ap inspect`.
pub struct Args {
    grants: bool,
    name: String,
}

/// Reads `ap inspect`'s arguments: one agent's name, and `--grants` at most
/// once, in either order. Anything else is bad usage.
pub fn parse(args: &[OsString]) -> Option<Args> {
    let mut grants = false;
    let mut name = None;
    for arg in args {
        if arg == "--grants" && !grants {
            grants = true;
        } else if arg.as_encoded_bytes().starts_with(b"-") || name.is_some() {
            return None;
        } else {
            name = Some(arg.to_str()?.to_string());
        }
    }
    Some(Args {
        grants,
        name: name?,
    })
}

/// Prints what the agent is made of, and returns the exit status.
pub fn run(args: &Args) -> u8 {
    match inspect(args) {
        Ok(text) => {
            print!("{text}");
            0
        }
        Err(message) => {
            eprintln!("{message}");
            1
        }
    }
}

fn inspect(args: &Args) -> Result<String, String> {
    let home = std::env::var_os("HOME");
    let root = ap::store::default_root(
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        home.as_deref(),
    )
    .ok_or("ap: HOME isn't an absolute path, so there's no place for the store")?;
    let store = |error: std::io::Error| format!("ap: {error}");
    let unknown = || {
        let shown = agentfile::print_argument(&args.name);
        format!("ap: no agent named {shown}")
    };
    if !::store::valid_name(&args.name) {
        return Err(unknown());
    }
    let agent = ::store::Agents::new(&root)
        .get(&args.name)
        .map_err(store)?
        .ok_or_else(unknown)?;
    // The approval is kept with the Agentfile's canonical path, as `ap build`
    // resolved it. A missing Agentfile was at that path when it was built.
    let agentfile = Path::new(&agent.directory).join("Agentfile");
    let agentfile = agentfile.canonicalize().unwrap_or(agentfile);
    let Some(grants) = ::store::Approvals::new(&root)
        .get(&agentfile)
        .map_err(store)?
    else {
        let shown = agentfile::print_argument(&agentfile.display().to_string());
        return Err(format!("ap: no grants are approved for {shown}"));
    };
    if args.grants {
        return Ok(grants.to_string());
    }
    let mut text = "layers:\n".to_string();
    for layer in &agent.layers {
        text.push_str(&format!("  {} {}\n", layer.digest, layer.instruction));
    }
    text.push_str("grants:\n");
    for line in grants.to_string().lines() {
        text.push_str(&format!("  {line}\n"));
    }
    Ok(text)
}
