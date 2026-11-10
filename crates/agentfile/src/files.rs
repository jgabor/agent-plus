//! Checks the arguments of `SHARE`, `WORKDIR`, `PERSIST`, `PERMISSIONS`, and
//! `PORT`, and reads them into typed [`Files`]. Nothing on the host is read.

use crate::{Error, Instruction};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Share {
    pub line: usize,
    /// A host path, as written.
    pub source: String,
    /// A guest path, as written. `None` leaves it to the mirror rule.
    pub destination: Option<String>,
    pub mode: ShareMode,
}

/// What an agent's writes to a share do.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ShareMode {
    /// Writes stay in the agent's overlay until `ap apply`.
    #[default]
    CopyOnWrite,
    /// `rw`: writes change your files.
    ReadWrite,
    /// `ro`: writes fail.
    ReadOnly,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workdir {
    pub line: usize,
    /// A guest path, as written.
    pub path: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Persist {
    pub line: usize,
    pub state: State,
}

/// The state that `PERSIST` keeps between runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Sessions,
    Home,
}

/// One `PERMISSIONS` rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub line: usize,
    /// `true` for `+`, `false` for `-`.
    pub allow: bool,
    pub access: Access,
    /// A guest path, as written. A relative one resolves against the working
    /// directory, at run time.
    pub path: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Port {
    pub line: usize,
    pub guest: u16,
    /// `None` uses the guest port, or the next free one.
    pub host: Option<u16>,
}

/// The `SHARE`, `WORKDIR`, `PERSIST`, `PERMISSIONS`, and `PORT` lines of an
/// Agentfile, each in file order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Files {
    pub share: Vec<Share>,
    pub workdir: Option<Workdir>,
    pub persist: Vec<Persist>,
    pub permissions: Vec<Rule>,
    pub port: Vec<Port>,
}

impl Files {
    /// Reads the files from the instructions that [`crate::parse`] returned.
    /// It skips the other instructions, and returns every error it finds, in
    /// line order.
    pub fn read(instructions: &[Instruction]) -> Result<Files, Vec<Error>> {
        let mut files = Files::default();
        let mut errors = Vec::new();
        for instruction in instructions {
            let line = instruction.line;
            match read(instruction) {
                None => {}
                Some(Err(message)) => errors.push(Error { line, message }),
                Some(Ok(Read::Share(share))) => files.share.push(share),
                Some(Ok(Read::Workdir(workdir))) => files.workdir = Some(workdir),
                Some(Ok(Read::Persist(persist))) => files.persist.push(persist),
                Some(Ok(Read::Rule(rule))) => files.permissions.push(rule),
                Some(Ok(Read::Port(port))) => files.port.push(port),
            }
        }
        if errors.is_empty() {
            Ok(files)
        } else {
            errors.sort_by_key(|error| error.line);
            Err(errors)
        }
    }
}

enum Read {
    Share(Share),
    Workdir(Workdir),
    Persist(Persist),
    Rule(Rule),
    Port(Port),
}

pub(crate) fn check(instruction: &Instruction) -> Result<(), String> {
    match read(instruction) {
        Some(Err(message)) => Err(message),
        _ => Ok(()),
    }
}

/// Reads one instruction, or returns `None` for one this module doesn't own.
fn read(instruction: &Instruction) -> Option<Result<Read, String>> {
    let line = instruction.line;
    let arguments: Vec<&str> = instruction.arguments.iter().map(String::as_str).collect();
    let result = match instruction.name.as_str() {
        "SHARE" => share(line, &arguments).map(Read::Share),
        "WORKDIR" => match arguments.as_slice() {
            [] | [""] => Err("`WORKDIR` needs a guest path".to_string()),
            [path] => Ok(Read::Workdir(Workdir {
                line,
                path: path.to_string(),
            })),
            more => Err(format!(
                "`WORKDIR` takes one guest path, but found {} arguments",
                more.len()
            )),
        },
        "PERSIST" => one("PERSIST", &arguments).and_then(|value| {
            let state = crate::settings::word(
                "PERSIST",
                value,
                &[("sessions", State::Sessions), ("home", State::Home)],
            )?;
            Ok(Read::Persist(Persist { line, state }))
        }),
        "PERMISSIONS" => {
            one("PERMISSIONS", &arguments).and_then(|value| rule(line, value).map(Read::Rule))
        }
        "PORT" => port(line, &arguments).map(Read::Port),
        _ => return None,
    };
    Some(result)
}

/// Reads the one argument of `name`.
fn one<'a>(name: &str, arguments: &[&'a str]) -> Result<&'a str, String> {
    match arguments {
        [value] => Ok(value),
        [] => Err(format!("`{name}` needs an argument")),
        more => Err(format!(
            "`{name}` takes one argument, but found {}",
            more.len()
        )),
    }
}

/// Reads `<src> [<dest>] [rw | ro]`.
fn share(line: usize, arguments: &[&str]) -> Result<Share, String> {
    let (mode, rest) = match arguments {
        [rest @ .., "rw"] if !rest.is_empty() => (ShareMode::ReadWrite, rest),
        [rest @ .., "ro"] if !rest.is_empty() => (ShareMode::ReadOnly, rest),
        _ => (ShareMode::CopyOnWrite, arguments),
    };
    if arguments.len() <= 3
        && let Some(word) = rest
            .iter()
            .skip(1)
            .find(|word| matches!(**word, "rw" | "ro"))
    {
        return Err(format!(
            "`{word}` must be the last argument of `SHARE`. To use it as a destination, write `./{word}`"
        ));
    }
    let (source, destination) = match rest {
        [source] => (*source, None),
        [source, destination] => (*source, Some(*destination)),
        [] => ("", None),
        _ => {
            return Err(format!(
                "`SHARE` takes a host path, an optional destination, and an optional `rw` or `ro`, but found {} arguments",
                arguments.len()
            ));
        }
    };
    if source.is_empty() {
        return Err("`SHARE` needs a host path".to_string());
    }
    if destination == Some("") {
        return Err("`SHARE` can't take an empty destination".to_string());
    }
    Ok(Share {
        line,
        source: source.to_string(),
        destination: destination.map(str::to_string),
        mode,
    })
}

/// Reads `+read:<path>`, `-read:<path>`, `+write:<path>`, or `-write:<path>`.
fn rule(line: usize, value: &str) -> Result<Rule, String> {
    let shape = || {
        format!(
            "`PERMISSIONS` takes `+` or `-`, then `read` or `write`, a colon, and a guest path, such as `-read:.env`, but found `{value}`"
        )
    };
    let (allow, rest) = if let Some(rest) = value.strip_prefix('+') {
        (true, rest)
    } else if let Some(rest) = value.strip_prefix('-') {
        (false, rest)
    } else {
        return Err(shape());
    };
    let (access, path) = rest.split_once(':').ok_or_else(shape)?;
    let access = match access {
        "read" => Access::Read,
        "write" => Access::Write,
        _ => return Err(shape()),
    };
    if path.is_empty() {
        return Err(format!("`PERMISSIONS` needs a guest path after `{value}`"));
    }
    Ok(Rule {
        line,
        allow,
        access,
        path: path.to_string(),
    })
}

/// Reads `<guest port> [<host port>]`.
fn port(line: usize, arguments: &[&str]) -> Result<Port, String> {
    let (guest, host) = match arguments {
        [guest] => (number(guest)?, None),
        [guest, host] => (number(guest)?, Some(number(host)?)),
        [] => return Err("`PORT` needs a guest port".to_string()),
        _ => {
            return Err(format!(
                "`PORT` takes a guest port and an optional host port, but found {} arguments",
                arguments.len()
            ));
        }
    };
    Ok(Port { line, guest, host })
}

/// Reads a TCP port written in decimal digits, from `1` to `65535`.
fn number(value: &str) -> Result<u16, String> {
    Some(value)
        .filter(|value| !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|port| *port != 0)
        .ok_or_else(|| format!("`PORT` takes a port from `1` to `65535`, but found `{value}`"))
}
