//! `ap secret set|rm|ls`, from SPEC.md's `ap secret` section. On Linux,
//! v0.1.0 has no secret store, so each one says to give the secret in the
//! host environment variable that `SECRET` reads first, and fails. Nothing
//! falls back to a file, and `set` never reads the value.

use std::ffi::OsString;

/// One `ap secret` subcommand, with the secret's name when it takes one.
pub enum Secret {
    Set(String),
    Rm(String),
    Ls,
}

/// Reads `ap secret`'s arguments. `set` and `rm` take exactly one name, so a
/// value given as an argument is bad usage. A name can't hold `=`, as in
/// `SECRET`, so `NAME=VALUE` is bad usage too, and its value isn't printed.
pub fn parse(args: &[OsString]) -> Option<Secret> {
    let name = |name: &OsString| {
        let name = name.to_str()?;
        let valid = !name.is_empty() && !name.starts_with('-') && !name.contains('=');
        valid.then(|| name.to_owned())
    };
    match args {
        [command] if command == "ls" => Some(Secret::Ls),
        [command, one] if command == "set" => name(one).map(Secret::Set),
        [command, one] if command == "rm" => name(one).map(Secret::Rm),
        _ => None,
    }
}

/// Says there's no secret store, and returns the exit status.
pub fn run(secret: &Secret) -> u8 {
    let variable = match secret {
        Secret::Set(name) | Secret::Rm(name) => {
            let name = agentfile::print_argument(name);
            format!("give it in the environment variable {name}")
        }
        Secret::Ls => "give each secret in the environment variable named after it".to_owned(),
    };
    eprintln!("ap: v0.1.0 has no secret store on Linux, so {variable}, which SECRET reads first");
    1
}
