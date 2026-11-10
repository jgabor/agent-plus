//! `ap init [dir]`, from SPEC.md's `ap init` section. The lines it writes
//! are the ones AUTH, NETWORK, and POLICY name, after the `AGENT` line every
//! Agentfile starts with.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;

/// What a new Agentfile holds: signed in through the harness's own flow,
/// with the network and calls to other agents closed.
const AGENTFILE: &str = "AGENT claude:latest\nAUTH login\nNETWORK -\nPOLICY -\n";

/// Reads `ap init`'s arguments: at most one directory, and no flags. `None`
/// is bad usage. The inner `None` is the current directory.
pub fn parse(args: &[OsString]) -> Option<Option<PathBuf>> {
    match args {
        [] => Some(None),
        [dir] if !dir.as_encoded_bytes().starts_with(b"-") => Some(Some(PathBuf::from(dir))),
        _ => None,
    }
}

/// Writes the Agentfile, and returns the exit status. Messages go to
/// standard error.
pub fn run(dir: Option<&PathBuf>) -> u8 {
    match write(dir) {
        Ok(()) => 0,
        Err(message) => {
            eprintln!("{message}");
            1
        }
    }
}

fn write(dir: Option<&PathBuf>) -> Result<(), String> {
    let path = match dir {
        Some(dir) => {
            fs::create_dir_all(dir).map_err(|error| match error.kind() {
                io::ErrorKind::AlreadyExists | io::ErrorKind::NotADirectory => {
                    format!("ap: {} isn't a directory", dir.display())
                }
                _ => format!("ap: {}: {error}", dir.display()),
            })?;
            dir.join("Agentfile")
        }
        None => PathBuf::from("Agentfile"),
    };
    let shown = path.display();
    // `create_new` refuses any existing name, a symlink included, so an
    // Agentfile is never overwritten.
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| match error.kind() {
            io::ErrorKind::AlreadyExists => format!("ap: {shown} already exists"),
            _ => format!("ap: {shown}: {error}"),
        })?;
    file.write_all(AGENTFILE.as_bytes())
        .map_err(|error| format!("ap: {shown}: {error}"))
}
