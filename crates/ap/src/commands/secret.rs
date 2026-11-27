//! `ap secret set|show|ls|rm`, from SPEC.md's `ap secret` section, and how
//! a run reads a secret's value, from SPEC.md's SECRET section.
//!
//! v0.1.0 has no secret store, so `ap secret` keeps commands, never values.
//! Each one prints a secret's value, and a run runs it when the secret's
//! environment variable is unset. They are kept in `~/.config/ap/secrets`,
//! which only the user can read, one line for each name, sorted by name:
//!
//! ```text
//! GITHUB_TOKEN "pass show github/token"
//! ```
//!
//! The command is one argument, printed and read as the store's records
//! are, so it keeps any text, line breaks included.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

/// One `ap secret` subcommand, with the secret's name when it takes one.
pub enum Secret {
    /// `set <name>`, with its command when `--command` gave one.
    Set(String, Option<String>),
    Show(String),
    Ls,
    Rm(String),
}

/// Reads `ap secret`'s arguments. `set`, `show`, and `rm` take exactly one
/// name, so a value given as an argument is bad usage. A name can't hold
/// `=`, as in `SECRET`, so `NAME=VALUE` is bad usage too, and its value
/// isn't printed. A name prints as itself, so it holds no space, quote,
/// backslash, or control character, and is one word in the file and one
/// line in `ls`. A command is UTF-8, and not empty.
pub fn parse(args: &[OsString]) -> Option<Secret> {
    let name = |name: &OsString| {
        let name = name.to_str()?;
        let valid = !name.is_empty()
            && !name.starts_with('-')
            && !name.contains('=')
            && agentfile::print_argument(name) == name;
        valid.then(|| name.to_owned())
    };
    match args {
        [command] if command == "ls" => Some(Secret::Ls),
        [command, one] if command == "set" => Some(Secret::Set(name(one)?, None)),
        [command, one, flag, given] if command == "set" && flag == "--command" => {
            let given = given.to_str().filter(|given| !given.is_empty())?;
            Some(Secret::Set(name(one)?, Some(given.to_owned())))
        }
        [command, one] if command == "show" => name(one).map(Secret::Show),
        [command, one] if command == "rm" => name(one).map(Secret::Rm),
        _ => None,
    }
}

/// Runs one subcommand, and returns the exit status.
pub fn run(secret: &Secret) -> u8 {
    match subcommand(secret) {
        Ok(()) => 0,
        Err(message) => {
            eprintln!("ap: {message}");
            1
        }
    }
}

fn subcommand(secret: &Secret) -> Result<(), String> {
    if let Secret::Set(name, None) = secret {
        return Err(format!(
            "v0.1.0 has no secret store, so give the secret in the environment variable {name}, which SECRET reads first, or name a command that prints it with `ap secret set {name} --command <command>`"
        ));
    }
    let file = file()?;
    // `set` and `rm` hold the lock from their read to their write, so two
    // at once can't lose either's change.
    let _lock = match secret {
        Secret::Set(..) | Secret::Rm(_) => Some(lock(&file)?),
        Secret::Show(_) | Secret::Ls => None,
    };
    let mut commands = read(&file)?;
    let none = |name: &str| format!("no command is kept for {name}");
    match secret {
        Secret::Set(name, command) => {
            // A `set` with no command returned above.
            commands.insert(name.clone(), command.clone().unwrap_or_default());
            write(&file, &commands)
        }
        Secret::Show(name) => {
            let command = commands.get(name).ok_or_else(|| none(name))?;
            println!("{command}");
            Ok(())
        }
        Secret::Ls => {
            for name in commands.keys() {
                println!("{name}");
            }
            Ok(())
        }
        Secret::Rm(name) => {
            commands.remove(name).ok_or_else(|| none(name))?;
            write(&file, &commands)
        }
    }
}

/// `~/.config/ap/secrets`.
fn file() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match home {
        Some(home) if home.is_absolute() => Ok(home.join(".config/ap/secrets")),
        _ => Err("HOME isn't an absolute path, so there's no place for secrets' commands".into()),
    }
}

/// Takes the lock on `file`'s commands, which lasts until the lock file
/// closes, and makes `file`'s directory, which only the user can open, if
/// it is missing.
fn lock(file: &Path) -> Result<File, String> {
    let failed = |error: io::Error| format!("can't lock {}: {error}", file.display());
    let dir = file.parent().expect("the file is in a directory");
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(failed)?;
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(dir.join(".secrets.lock"))
        .map_err(failed)?;
    lock.lock().map_err(failed)?;
    Ok(lock)
}

/// The commands kept in `file`, by name. A missing file keeps none.
fn read(file: &Path) -> Result<BTreeMap<String, String>, String> {
    let text = match fs::read_to_string(file) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("can't read {}: {error}", file.display())),
    };
    let mut commands = BTreeMap::new();
    for (number, line) in (1..).zip(text.lines()) {
        let bad = || format!("{}:{number} isn't a name and its command", file.display());
        let (name, rest) = line.split_once(' ').ok_or_else(bad)?;
        let [command] = &agentfile::read_arguments(rest).map_err(|_| bad())?[..] else {
            return Err(bad());
        };
        if name.is_empty() || commands.insert(name.to_owned(), command.clone()).is_some() {
            return Err(bad());
        }
    }
    Ok(commands)
}

/// Replaces `file` with these commands, in a new file that only the user
/// can read, so a reader sees the old file or the new one. The caller
/// holds the [`lock`], which made the directory.
fn write(file: &Path, commands: &BTreeMap<String, String>) -> Result<(), String> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let failed = |error: io::Error| format!("can't write {}: {error}", file.display());
    let dir = file.parent().expect("the file is in a directory");
    let mut text = String::new();
    for (name, command) in commands {
        text.push_str(&format!("{name} {}\n", agentfile::print_argument(command)));
    }
    // A crashed process can leave a name behind, and its process id can
    // come round again.
    let (temporary, mut opened) = loop {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let temporary = dir.join(format!(".secrets.{}.{id}", std::process::id()));
        let options = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary);
        match options {
            Ok(opened) => break (temporary, opened),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(failed(error)),
        }
    };
    let written = (opened.write_all(text.as_bytes()))
        .and_then(|()| opened.sync_all())
        .and_then(|()| fs::rename(&temporary, file))
        .and_then(|()| File::open(dir)?.sync_all());
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written.map_err(failed)
}

/// The value of the secret whose environment variable is `name`, for a run.
/// It is the variable's when that is set. Otherwise it is what the command
/// kept for `name` prints, which runs with `sh -c`, and gets this process's
/// standard input and standard error, so it can ask on the terminal. An
/// error names the secret as `shown`, and never holds what it printed.
pub fn value(name: &str, shown: &str) -> Result<Vec<u8>, String> {
    if let Some(value) = std::env::var_os(name) {
        return Ok(value.into_vec());
    }
    let commands = read(&file()?)?;
    let Some(command) = commands.get(name) else {
        return Err(format!(
            "{shown} has no value. Set the environment variable `{name}`, or name a command that prints it with `ap secret set {name} --command <command>`"
        ));
    };
    printed(command).map_err(|ended| {
        format!("{shown} has no value: its command, which `ap secret show {name}` prints, {ended}")
    })
}

/// What `command` prints, without its trailing newlines, or how it ended
/// when it failed or printed nothing.
fn printed(command: &str) -> Result<Vec<u8>, String> {
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|error| format!("can't start: {error}"))?;
    let ended = match (output.status.code(), output.status.signal()) {
        (Some(0), _) => None,
        (Some(code), _) => Some(format!("exited with status {code}")),
        (None, Some(signal)) => Some(format!("was killed by signal {signal}")),
        (None, None) => Some(format!("ended with {}", output.status)),
    };
    if let Some(ended) = ended {
        return Err(ended);
    }
    let mut value = output.stdout;
    while value.last() == Some(&b'\n') {
        value.pop();
    }
    if value.is_empty() {
        return Err("printed nothing".to_owned());
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    // spec(SECRET): The value is its standard output without the trailing newlines, as `$(...)` gives it.
    #[test]
    fn the_value_is_what_the_command_prints_less_its_trailing_newlines() {
        for command in [
            "printf 'tok'",
            "printf 'tok\\n'",
            "printf 'tok\\n\\n\\n'",
            "printf 'a\\n\\nb\\n'",
            "printf ' tok \\r\\n'",
            "printf '\\ntok\\t'",
        ] {
            // The shell's own `$(...)` is the oracle. `x` keeps it from
            // printing nothing.
            let oracle = format!("printf '%sx' \"$({command})\"");
            let mut expected = printed(&oracle).unwrap();
            assert_eq!(expected.pop(), Some(b'x'));
            assert_eq!(printed(command).unwrap(), expected, "{command}");
        }
        assert_eq!(printed("printf 'tok\\n'").unwrap(), b"tok");
    }
}
