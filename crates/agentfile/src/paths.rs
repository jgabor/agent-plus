//! Resolves the guest paths of `ADD`, `SHARE`, and `REPO`, by the guest side
//! of SPEC.md's Paths section, and reports destinations that can't work.
//! Nothing on the host is read, so the host side of that section is left to
//! the build and the run.

use crate::{Error, Files, Instruction, Sources};

/// The agent's home directory, where `~/` and relative guest paths resolve.
const HOME: &str = "/home/agent";

/// The system directories a destination may not overlap. A name ending in
/// `*` covers every top-level directory that starts with what comes before.
const SYSTEM: [&str; 10] = [
    "bin", "boot", "dev", "etc", "lib*", "proc", "run", "sbin", "sys", "usr",
];

/// The harness configuration directories a destination may not overlap,
/// under `/home/agent`.
const HARNESS: [&str; 2] = [".claude", ".codex"];

/// Where one `ADD`, `SHARE`, or `REPO` lands in the guest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuestPath {
    pub line: usize,
    /// An absolute, normalized guest path.
    pub path: String,
}

impl GuestPath {
    /// Gives each `ADD`, `SHARE`, and `REPO` its guest path, in line order.
    /// `directory` is the name of the directory that holds the Agentfile,
    /// which the mirror rule uses for `.`.
    ///
    /// A `REPO` with a local source and no destination is left out, because
    /// its name comes from its canonical path on the host. Every error is
    /// returned, in line order.
    pub fn read(
        instructions: &[Instruction],
        directory: &str,
    ) -> Result<Vec<GuestPath>, Vec<Error>> {
        let (sources, files) = match (Sources::read(instructions), Files::read(instructions)) {
            (Ok(sources), Ok(files)) => (sources, files),
            (sources, files) => {
                let mut errors = sources.err().unwrap_or_default();
                errors.extend(files.err().unwrap_or_default());
                errors.sort_by_key(|error| error.line);
                return Err(errors);
            }
        };
        let mut found: Vec<(usize, Result<Vec<String>, String>)> = Vec::new();
        for add in &sources.add {
            found.push((
                add.line,
                place(&add.source, add.destination.as_deref(), directory),
            ));
        }
        for share in &files.share {
            found.push((
                share.line,
                place(&share.source, share.destination.as_deref(), directory),
            ));
        }
        for repo in &sources.repo {
            let parts = match (&repo.destination, repo.name()) {
                (Some(destination), _) => resolve(destination),
                (None, Some(name)) => resolve(&format!("~/git/{name}")),
                (None, None) => continue,
            };
            found.push((repo.line, Ok(parts)));
        }
        found.sort_by_key(|(line, _)| *line);

        let mut paths = Vec::new();
        let mut errors = Vec::new();
        // The guest paths without errors so far, with their lines.
        let mut placed: Vec<(usize, Vec<String>)> = Vec::new();
        for (line, parts) in found {
            let result = parts.and_then(|parts| {
                reserved(&parts)?;
                if let Some((first, other)) = placed
                    .iter()
                    .find(|(_, other)| overlap(&parts, other))
                {
                    return Err(format!(
                        "the destination `{}` overlaps `{}` from line {first}. One destination can't equal or contain another",
                        join(&parts),
                        join(other)
                    ));
                }
                Ok(parts)
            });
            match result {
                Ok(parts) => {
                    paths.push(GuestPath {
                        line,
                        path: join(&parts),
                    });
                    placed.push((line, parts));
                }
                Err(message) => errors.push(Error { line, message }),
            }
        }
        if errors.is_empty() {
            Ok(paths)
        } else {
            Err(errors)
        }
    }
}

/// The guest path of an `ADD` or `SHARE`: its destination, or else the
/// mirror rule applied to its host path.
fn place(source: &str, destination: Option<&str>, directory: &str) -> Result<Vec<String>, String> {
    if let Some(destination) = destination {
        return Ok(resolve(destination));
    }
    if source == "~" || source.starts_with("~/") || source.starts_with('/') {
        return Ok(resolve(source));
    }
    // A relative host path, which must stay inside the build context.
    let mut parts = Vec::new();
    for part in source.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err(format!(
                        "`{source}` is outside the build context, so the mirror rule can't place it. Give an explicit destination"
                    ));
                }
            }
            part => parts.push(part),
        }
    }
    if parts.is_empty() {
        parts.push(directory);
    }
    Ok(resolve(&format!("~/{}", parts.join("/"))))
}

/// Resolves a guest path to its normalized parts, from the root.
fn resolve(path: &str) -> Vec<String> {
    let full = if path == "~" {
        HOME.to_string()
    } else if let Some(rest) = path.strip_prefix("~/") {
        format!("{HOME}/{rest}")
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        format!("{HOME}/{path}")
    };
    let mut parts: Vec<String> = Vec::new();
    for part in full.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part.to_string()),
        }
    }
    parts
}

fn join(parts: &[String]) -> String {
    format!("/{}", parts.join("/"))
}

/// Whether one path equals or contains the other.
fn overlap(a: &[String], b: &[String]) -> bool {
    let shorter = a.len().min(b.len());
    a[..shorter] == b[..shorter]
}

/// Fails when `parts` equals, contains, or is inside a reserved path.
fn reserved(parts: &[String]) -> Result<(), String> {
    let path = join(parts);
    let system = match parts.first() {
        None => Some("/"),
        Some(top) => SYSTEM
            .into_iter()
            .find(|name| match name.strip_suffix('*') {
                Some(prefix) => top.starts_with(prefix),
                None => top == name,
            }),
    };
    if let Some(name) = system {
        let name = if parts.is_empty() {
            "the system directories".to_string()
        } else {
            format!("`/{name}`")
        };
        return Err(format!(
            "the destination `{path}` is reserved, because it overlaps {name}"
        ));
    }
    let home = resolve("~");
    for name in HARNESS {
        let mut config = home.clone();
        config.push(name.to_string());
        if overlap(parts, &config) {
            return Err(format!(
                "the destination `{path}` is reserved, because it overlaps the harness configuration directory `~/{name}`. Use `PLUGIN`, `SKILL`, `MCP`, `AUTH`, and `PERSIST` instead"
            ));
        }
    }
    Ok(())
}
