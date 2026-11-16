//! Asks you to approve an Agentfile's grants, by "How approval works" under
//! SPEC.md's Grants section. The approval is kept in the store with the
//! Agentfile's canonical path.
//!
//! A build asks for every grant. A run asks again only for its shares, which
//! resolve again at every run. The promises that need a pulled image, or
//! another agent, aren't kept yet.

use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;

use agentfile::{Error, Grants, Instruction, Location, PathGrant, print_argument};
use store::Approvals;

use crate::paths::HostPath;

/// Lists what the instructions ask of the host. `resolve` resolves a host
/// path, as [`HostPath::resolve`] does.
pub fn grants(
    instructions: &[Instruction],
    mut resolve: impl FnMut(&str) -> io::Result<HostPath>,
) -> Result<Grants, Vec<Error>> {
    Grants::read(instructions, |text| {
        let host = resolve(text).map_err(|error| error.to_string())?;
        let path = host.path().to_str().ok_or_else(|| {
            format!(
                "{} isn't UTF-8, so it can't be approved",
                host.path().display()
            )
        })?;
        Ok(Location {
            path: path.to_string(),
            outside: host.outside(),
        })
    })
}

/// Where an approval comes from.
pub enum Approver<'a> {
    /// `--approve=<file>`, which approves exactly the grants it lists.
    File(&'a Path),
    /// You, on the terminal that standard input is, if it is one.
    Terminal,
}

/// Makes sure you approved `grants` for the Agentfile at the canonical path
/// `agentfile`, which errors call `shown`. A first build lists every grant,
/// and a later one only what widened. It returns an error message when the
/// grants aren't approved.
pub fn approve(
    approvals: &Approvals,
    agentfile: &Path,
    shown: &str,
    grants: &Grants,
    approver: Approver,
) -> Result<(), String> {
    // You may not have chosen the names either, so they show as they are.
    let shown = print_argument(shown);
    let store = |error: io::Error| format!("ap: {error}");
    let approved = approvals.get(agentfile).map_err(store)?;
    let keep = || match approved.as_ref() == Some(grants) {
        true => Ok(()),
        false => approvals.put(agentfile, grants).map_err(store),
    };
    if let Approver::File(file) = approver {
        let listed = listed(file)?;
        let wider = grants.widening(&listed, crate::adapter::reserved);
        if !wider.is_empty() {
            return Err(format!(
                "ap: {shown} asks for more than {} approves:\n{}",
                print_argument(&file.display().to_string()),
                indented(wider.iter().map(ToString::to_string))
            ));
        }
        return keep();
    }
    let asking = match &approved {
        None => format!(
            "{shown} asks for these grants:\n{}",
            indented(grants.to_string().lines().map(str::to_string))
        ),
        Some(old) => {
            let wider = grants.widening(old, crate::adapter::reserved);
            if wider.is_empty() {
                return Ok(());
            }
            format!(
                "{shown} asks for more than you approved:\n{}",
                indented(wider.iter().map(ToString::to_string))
            )
        }
    };
    ask(
        &asking,
        "on a terminal, or with `--approve=<file>`",
        "nothing was built",
    )
    .map_err(|message| format!("ap: {message}"))?;
    keep()
}

/// Makes sure you approved `paths`, the grants of a run's shares as they
/// resolve now, for the Agentfile at the canonical path `agentfile`, which
/// errors call `shown`. It asks only for what you haven't approved, and adds
/// that to the approval. It returns an error message when the paths aren't
/// approved.
pub fn approve_shares(
    approvals: &Approvals,
    agentfile: &Path,
    shown: &str,
    paths: Vec<PathGrant>,
) -> Result<(), String> {
    let shown = print_argument(shown);
    let approved = approvals
        .get(agentfile)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("nothing is approved for {shown}. Build it again"))?;
    // Only the paths differ, so only they can widen.
    let now = Grants {
        paths,
        ..approved.clone()
    };
    let wider = now.widening(&approved, crate::adapter::reserved);
    if wider.is_empty() {
        return Ok(());
    }
    let asking = format!(
        "{shown} asks for more than you approved:\n{}",
        indented(wider.iter().map(ToString::to_string))
    );
    ask(&asking, "on a terminal", "the run didn't start")?;
    let mut kept = approved;
    for path in now.paths {
        match kept.paths.iter_mut().find(|old| old.path == path.path) {
            Some(old) => old.write |= path.write,
            None => kept.paths.push(path),
        }
    }
    kept.paths.sort_by(|a, b| a.path.cmp(&b.path));
    approvals
        .put(agentfile, &kept)
        .map_err(|error| error.to_string())
}

/// Asks you, on the terminal that standard input is, to approve what
/// `asking` lists. Without a terminal it fails, and says to approve them
/// `elsewhere`. A refusal says `refused`.
fn ask(asking: &str, elsewhere: &str, refused: &str) -> Result<(), String> {
    let stdin = io::stdin();
    if !stdin.is_terminal() {
        return Err(format!("{asking}\nap: approve them {elsewhere}"));
    }
    let mut stderr = io::stderr();
    let _ = write!(stderr, "{asking}\nApprove them? [y/N] ");
    let _ = stderr.flush();
    let mut answer = String::new();
    stdin
        .lock()
        .read_line(&mut answer)
        .map_err(|error| error.to_string())?;
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Ok(()),
        _ => Err(format!("you didn't approve the grants, so {refused}")),
    }
}

/// Reads the grants that `--approve=<file>` lists.
fn listed(file: &Path) -> Result<Grants, String> {
    let shown = file.display();
    let text = fs::read_to_string(file).map_err(|error| format!("ap: {shown}: {error}"))?;
    text.parse().map_err(|errors: Vec<Error>| {
        let lines: Vec<String> = errors
            .iter()
            .map(|error| format!("{shown}:{}: {}", error.line, error.message))
            .collect();
        lines.join("\n")
    })
}

/// Each line, indented by two spaces.
fn indented(lines: impl Iterator<Item = String>) -> String {
    lines
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}
