//! Asks you to approve an Agentfile's grants, by "How approval works" under
//! SPEC.md's Grants section. The approval is kept in the store with the
//! Agentfile's canonical path.
//!
//! Only a build asks yet. The promises that need a run, a pulled image, or
//! another agent are a run's to keep.

use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;

use agentfile::{Error, Grants, Instruction, Location};
use store::Approvals;

use crate::paths::HostPath;

/// Lists what the instructions ask of the host. Host paths resolve against
/// the build context and `home`, as [`HostPath::resolve`] says.
pub fn grants(
    instructions: &[Instruction],
    context: &Path,
    home: &Path,
) -> Result<Grants, Vec<Error>> {
    Grants::read(instructions, |text| {
        let host = HostPath::resolve(text, context, home).map_err(|error| error.to_string())?;
        let path = host.path().to_str().ok_or_else(|| {
            format!("{} isn't UTF-8, so it can't be approved", host.path().display())
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
    let store = |error: io::Error| format!("ap: {error}");
    let approved = approvals.get(agentfile).map_err(store)?;
    let keep = || match approved.as_ref() == Some(grants) {
        true => Ok(()),
        false => approvals.put(agentfile, grants).map_err(store),
    };
    if let Approver::File(file) = approver {
        let listed = listed(file)?;
        let wider = grants.widening(&listed);
        if !wider.is_empty() {
            return Err(format!(
                "ap: {shown} asks for more than {} approves:\n{}",
                file.display(),
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
            let wider = grants.widening(old);
            if wider.is_empty() {
                return Ok(());
            }
            format!(
                "{shown} asks for more than you approved:\n{}",
                indented(wider.iter().map(ToString::to_string))
            )
        }
    };
    let stdin = io::stdin();
    if !stdin.is_terminal() {
        return Err(format!(
            "ap: {asking}\nap: approve them on a terminal, or with `--approve=<file>`"
        ));
    }
    let mut stderr = io::stderr();
    let _ = write!(stderr, "{asking}\nApprove them? [y/N] ");
    let _ = stderr.flush();
    let mut answer = String::new();
    stdin
        .lock()
        .read_line(&mut answer)
        .map_err(|error| format!("ap: {error}"))?;
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => keep(),
        _ => Err("ap: you didn't approve the grants, so nothing was built".to_string()),
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
