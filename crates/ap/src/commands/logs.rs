//! `ap logs <run>`, from SPEC.md's `ap logs` section. It reads the run's log
//! from the store, which `ap run` writes as it shows the run's output, and
//! writes each part to the stream it was shown on. For an active run, it
//! stops at what `ap run` has written so far.

use std::ffi::OsString;
use std::io::{self, BufReader, Read, Write};

use ap::runs::{self, FromRun};
use store::Runs;

/// Reads `ap logs`'s arguments: one run's name. Anything else is bad usage.
pub fn parse(args: &[OsString]) -> Option<String> {
    match args {
        [name] if !name.as_encoded_bytes().starts_with(b"-") => Some(name.to_str()?.to_owned()),
        _ => None,
    }
}

/// Prints the run's log, and returns the exit status.
pub fn run(name: &str) -> u8 {
    match print(name) {
        Ok(()) => 0,
        Err(message) => {
            eprintln!("ap: {message}");
            1
        }
    }
}

fn print(name: &str) -> Result<(), String> {
    let root = ap::store::default_root(
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
    .ok_or("HOME isn't an absolute path, so there's no place for the store")?;
    let runs = Runs::new(&root);
    let missing = || format!("no run is named {name}");
    if !store::valid_run_name(name) || runs.get(name).map_err(|error| error.to_string())?.is_none()
    {
        return Err(missing());
    }
    let log = runs
        .log(name)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("the log of run {name} was deleted"))?;
    // An active run's log ends where it was when it was opened.
    let length = log.metadata().map_err(|error| error.to_string())?.len();
    let mut log = BufReader::new(log).take(length);
    let (mut stdout, mut stderr) = (io::stdout().lock(), io::stderr().lock());
    let unreadable = |error: io::Error| format!("can't read the log of run {name}: {error}");
    // A reader that went away has nothing more to see.
    while let Some(part) = runs::read_log(&mut log).map_err(unreadable)? {
        let written = match part {
            FromRun::Stdout(bytes) => stdout.write_all(&bytes),
            FromRun::Stderr(bytes) => stdout.flush().and_then(|()| stderr.write_all(&bytes)),
            _ => Ok(()),
        };
        if written.is_err() {
            return Ok(());
        }
    }
    let _ = stdout.flush();
    Ok(())
}
