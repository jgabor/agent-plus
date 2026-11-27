//! `ap inspect [--grants | --network=<destination>] <agent>`, from SPEC.md's
//! `ap inspect` section. It shows the agent's layers, in order, each with
//! its digest and the instruction that made it, and the grants you approved
//! for its Agentfile. `--grants` prints only the grants, in the text form
//! that `--approve=<file>` reads. `--network` says whether the agent may
//! reach a destination, and which rule decides, as [`ap::network::answer`]
//! decides it.
//!
//! Git credentials and the harness's own hosts aren't shown yet, because
//! nothing fetches a source or resolves a harness.

use std::ffi::OsString;
use std::path::Path;

/// The arguments of `ap inspect`.
pub struct Args {
    grants: bool,
    /// The destination that `--network` asks about.
    network: Option<String>,
    name: String,
}

/// Reads `ap inspect`'s arguments: one agent's name, and `--grants` or
/// `--network=<destination>` at most once, in either order. Anything else
/// is bad usage.
pub fn parse(args: &[OsString]) -> Option<Args> {
    let mut grants = false;
    let mut network = None;
    let mut name = None;
    for arg in args {
        let flagged = arg.to_str().and_then(|arg| arg.strip_prefix("--network="));
        if arg == "--grants" && !grants && network.is_none() {
            grants = true;
        } else if let Some(destination) = flagged
            && !grants
            && network.is_none()
        {
            network = Some(destination.to_owned());
        } else if arg.as_encoded_bytes().starts_with(b"-") || name.is_some() {
            return None;
        } else {
            name = Some(arg.to_str()?.to_string());
        }
    }
    Some(Args {
        grants,
        network,
        name: name?,
    })
}

/// Prints what the agent is made of, or answers `--network`, and returns
/// the exit status.
pub fn run(args: &Args) -> u8 {
    if let Some(destination) = &args.network {
        return ask(&args.name, destination);
    }
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

/// Says whether the agent `name` may reach `destination`, and returns 0 if
/// it may, 1 if it may not, 2 if `destination` can't be read, and 125 if
/// `ap` can't answer.
fn ask(name: &str, destination: &str) -> u8 {
    let asked = match ap::network::Asked::read(destination) {
        Ok(asked) => asked,
        Err(message) => {
            eprintln!("ap: {message}");
            return 2;
        }
    };
    let answer = approved(name).and_then(|(agent, _)| {
        // A run with no `--network` flag decides by these rules too.
        let instructions = ap::store::declared(&agent);
        let unread = || format!("ap: agent {name}'s rules can't be read");
        let harness = (agentfile::Settings::read(&instructions).map_err(|_| unread())?)
            .agent
            .harness;
        let args = ap::network::arguments(ap::adapter::reserved(harness).hosts, &instructions);
        ap::network::answer(&args, &asked).ok_or_else(unread)
    });
    let answer = match answer {
        Ok(answer) => answer,
        Err(message) => {
            eprintln!("{message}");
            return 125;
        }
    };
    let sign = if answer.allows { '+' } else { '-' };
    // The destination was read, so it holds only visible ASCII.
    println!("{sign} {destination}  {}", answer.rule);
    if let (true, ap::network::Asked::Name { name, .. }) = (answer.allows, &asked) {
        eprintln!(
            "ap: a run can still deny a connection to {name}: by an IP rule or your host's loopback, for the address it resolves to, or by the name check"
        );
    }
    if answer.allows { 0 } else { 1 }
}

/// The agent named `name`, and the grants approved for its Agentfile.
fn approved(name: &str) -> Result<(::store::Agent, agentfile::Grants), String> {
    let home = std::env::var_os("HOME");
    let root = ap::store::default_root(
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        home.as_deref(),
    )
    .ok_or("ap: HOME isn't an absolute path, so there's no place for the store")?;
    let store = |error: std::io::Error| format!("ap: {error}");
    let unknown = || {
        let shown = agentfile::print_argument(name);
        format!("ap: no agent named {shown}")
    };
    if !::store::valid_name(name) {
        return Err(unknown());
    }
    let agent = ::store::Agents::new(&root)
        .get(name)
        .map_err(store)?
        .ok_or_else(unknown)?;
    // The approval is kept with the Agentfile's canonical path, as the build
    // resolved it, so where the path leads now doesn't matter.
    let Some(grants) = ::store::Approvals::new(&root)
        .get(Path::new(&agent.agentfile))
        .map_err(store)?
    else {
        let shown = agentfile::print_argument(&agent.agentfile);
        return Err(format!("ap: no grants are approved for {shown}"));
    };
    Ok((agent, grants))
}

fn inspect(args: &Args) -> Result<String, String> {
    let (agent, grants) = approved(&args.name)?;
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
