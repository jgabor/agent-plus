//! `ap logs <run>`, from SPEC.md's `ap logs` section. It reads the run's log
//! from the store, which `ap run` writes as it shows the run's output, and
//! writes each part to the stream it was shown on, then the run's network
//! decisions on standard error. For an active run, it stops at what `ap
//! run` has written so far.

use std::ffi::OsString;
use std::io::{self, BufReader, Read, Write};

use ap::runs::{self, Logged};
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
    let log = BufReader::new(log).take(length);
    let unreadable = |error: io::Error| format!("can't read the log of run {name}: {error}");
    show(log, &mut io::stdout().lock(), &mut io::stderr().lock()).map_err(unreadable)
}

/// Writes the output in `log` to `stdout` and `stderr`, each part where it
/// was shown, then the network decisions to `stderr`, each on a line of its
/// own with its time. A reader that went away has nothing more to see.
fn show(mut log: impl Read, stdout: &mut impl Write, stderr: &mut impl Write) -> io::Result<()> {
    let mut decisions = Vec::new();
    // Whether the output shown last ended its line, as no output does.
    let mut ended = true;
    while let Some(part) = runs::read_log(&mut log)? {
        if let Logged::Stdout(bytes) | Logged::Stderr(bytes) = &part
            && let Some(&last) = bytes.last()
        {
            ended = last == b'\n';
        }
        let written = match part {
            Logged::Stdout(bytes) => stdout.write_all(&bytes),
            Logged::Stderr(bytes) => stdout.flush().and_then(|()| stderr.write_all(&bytes)),
            Logged::Decision(at, line) => {
                decisions.push(format!("{}  {line}\n", super::ps::utc(at.into())));
                Ok(())
            }
        };
        if written.is_err() {
            return Ok(());
        }
    }
    if stdout.flush().is_err() {
        return Ok(());
    }
    if !ended && !decisions.is_empty() {
        decisions.insert(0, "\n".to_owned());
    }
    let _ = stderr.write_all(decisions.concat().as_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // spec(ap logs): `ap logs <run>` shows a run's output and its network decisions.
    // spec(ap logs): It prints the run's network decisions after its output, on standard error.
    // spec(ap logs): Each line has the time, `allow` or `deny`, the destination, and the rule that decided.
    #[test]
    fn decisions_follow_the_output_on_standard_error() {
        let mut log = Vec::new();
        runs::write_log(&mut log, false, b"out\n").unwrap();
        let at = 1_794_567_729_123_456_789;
        let allowed = "allow  host.ap.internal:8080  NETWORK +host.ap.internal";
        runs::write_decision(&mut log, at, allowed).unwrap();
        runs::write_log(&mut log, true, b"err\n").unwrap();
        runs::write_decision(
            &mut log,
            at + 1_000_000_000,
            "deny   denied.example  the default",
        )
        .unwrap();
        let finished = log.len();
        // An active run's log can end in a message still being written.
        runs::write_log(&mut log, false, b"more").unwrap();
        for log in [&log[..finished], &log[..log.len() - 1]] {
            let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
            show(log, &mut stdout, &mut stderr).unwrap();
            assert_eq!(String::from_utf8(stdout).unwrap(), "out\n");
            assert_eq!(
                String::from_utf8(stderr).unwrap(),
                "err\n\
                 2026-11-13T11:02:09Z  allow  host.ap.internal:8080  NETWORK +host.ap.internal\n\
                 2026-11-13T11:02:10Z  deny   denied.example  the default\n"
            );
        }
    }

    #[test]
    fn decisions_start_a_line_of_their_own() {
        let mut log = Vec::new();
        runs::write_log(&mut log, true, b"no newline").unwrap();
        runs::write_decision(&mut log, 0, "deny   denied.example  the default").unwrap();
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        show(&log[..], &mut stdout, &mut stderr).unwrap();
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "no newline\n1970-01-01T00:00:00Z  deny   denied.example  the default\n"
        );
        // Output that ends a line, or no output, needs no newline.
        let mut log = Vec::new();
        runs::write_decision(&mut log, 0, "deny   denied.example  the default").unwrap();
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        show(&log[..], &mut stdout, &mut stderr).unwrap();
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "1970-01-01T00:00:00Z  deny   denied.example  the default\n"
        );
    }
}
